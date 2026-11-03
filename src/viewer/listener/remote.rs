use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

#[cfg(not(unix))]
use anyhow::bail;
use anyhow::{Context, Result};
use qemu_display::{
    ConsoleProxy, Cursor, KeyboardProxy, MouseProxy, MouseSet, Scanout, Update, UpdateDMABUF,
};
#[cfg(unix)]
use qemu_display::{ScanoutDMABUF, ScanoutMap, UpdateMap};
use zbus::{
    Connection,
    proxy::CacheProperties,
    zvariant::{Fd, OwnedObjectPath},
};

#[cfg(unix)]
use std::os::fd::{AsFd, IntoRawFd};

use super::super::{InputEvent, events::EventSender, framebuffer::FrameStreamHandler};

use super::head_map::{HeadMap, clamp_to_console, compute_head_map, map_abs_position};

const LISTENER_PATH: &str = "/org/qemu/Display1/Listener";

pub(super) struct RemoteConsole {
    proxy: ConsoleProxy<'static>,
    keyboard: KeyboardProxy<'static>,
    mouse: MouseProxy<'static>,
    listener_connection: Option<Connection>,
    console_id: u32,
    /// Every console of the VM (including ours), for reading head sizes.
    siblings: Vec<(u32, ConsoleProxy<'static>)>,
    explicit_layout: Vec<(u32, i32, i32)>,
    head_map: std::sync::Mutex<Option<HeadMap>>,
    /// Head map learned from the scanout itself (`ScanoutDMABUF2` backing
    /// size + view offset), shared with the listener. Authoritative when set:
    /// an X11 guest scans every head out of one framebuffer, so the view's
    /// offset IS the head's position in the desktop, no guessing needed.
    scanout_map: Arc<std::sync::Mutex<Option<HeadMap>>>,
    /// This console's own size, for clamping single-head positions.
    self_size: std::sync::Mutex<Option<(u32, u32)>>,
    head_map_refreshed: std::sync::Mutex<Option<std::time::Instant>>,
}

impl RemoteConsole {
    pub(super) async fn new(
        connection: &Connection,
        owner: &str,
        console_id: u32,
        console_ids: &[u32],
        explicit_layout: Vec<(u32, i32, i32)>,
    ) -> Result<Self> {
        let mut siblings = Vec::new();
        for id in console_ids {
            let path = OwnedObjectPath::try_from(format!("/org/qemu/Display1/Console_{id}"))?;
            let sibling = ConsoleProxy::builder(connection)
                .cache_properties(CacheProperties::No)
                .destination(owner.to_owned())?
                .path(path)?
                .build()
                .await
                .with_context(|| format!("failed to build the proxy for console {id}"))?;
            siblings.push((*id, sibling));
        }
        let object_path =
            OwnedObjectPath::try_from(format!("/org/qemu/Display1/Console_{console_id}"))?;
        let proxy = ConsoleProxy::builder(connection)
            .cache_properties(CacheProperties::No)
            .destination(owner.to_owned())?
            .path(object_path.clone())?
            .build()
            .await
            .with_context(|| format!("failed to build the console proxy for owner `{owner}`"))?;
        let keyboard = KeyboardProxy::builder(connection)
            .destination(owner.to_owned())?
            .path(object_path.clone())?
            .build()
            .await
            .with_context(|| format!("failed to build the keyboard proxy for owner `{owner}`"))?;
        let mouse = MouseProxy::builder(connection)
            .cache_properties(CacheProperties::No)
            .destination(owner.to_owned())?
            .path(object_path)?
            .build()
            .await
            .with_context(|| format!("failed to build the mouse proxy for owner `{owner}`"))?;

        Ok(Self {
            proxy,
            keyboard,
            mouse,
            listener_connection: None,
            console_id,
            siblings,
            explicit_layout,
            head_map: std::sync::Mutex::new(None),
            scanout_map: Arc::new(std::sync::Mutex::new(None)),
            self_size: std::sync::Mutex::new(None),
            head_map_refreshed: std::sync::Mutex::new(None),
        })
    }

    pub(super) async fn mouse_is_absolute(&self) -> Result<bool> {
        self.mouse
            .is_absolute()
            .await
            .context("failed to query the mouse mode")
    }

    pub(super) async fn set_ui_info(&self, width: u32, height: u32) -> Result<()> {
        self.proxy
            .set_ui_info(0, 0, 0, 0, width, height)
            .await
            .with_context(|| format!("failed to request a guest resize to {width}x{height}"))
    }

    pub(super) async fn check_alive(&self) -> Result<()> {
        self.proxy
            .label()
            .await
            .context("failed to reach the remote console")
            .map(|_| ())
    }

    /// Re-read every head's size and recompute where this console sits in
    /// the guest desktop. Cheap (one property read per head); called at
    /// startup, on every button press, and at most once per second while the
    /// pointer moves, so other windows resizing their heads is picked up.
    pub(super) async fn refresh_head_map(&self) -> Result<()> {
        // Throttle retries even when a console query fails.
        *self.head_map_refreshed.lock().unwrap() = Some(std::time::Instant::now());
        let mut sizes = Vec::with_capacity(self.siblings.len());
        for (id, proxy) in &self.siblings {
            let width = proxy
                .width()
                .await
                .with_context(|| format!("console {id} width"))?;
            let height = proxy
                .height()
                .await
                .with_context(|| format!("console {id} height"))?;
            sizes.push((*id, width, height));
        }
        if let Some((_, w, h)) = sizes.iter().find(|(id, _, _)| *id == self.console_id) {
            *self.self_size.lock().unwrap() = Some((*w, *h));
        }
        let map = compute_head_map(&sizes, &self.explicit_layout, self.console_id);
        *self.head_map.lock().unwrap() = map;
        *self.head_map_refreshed.lock().unwrap() = Some(std::time::Instant::now());
        Ok(())
    }

    async fn refresh_head_map_if_stale(&self) {
        let stale = self
            .head_map_refreshed
            .lock()
            .unwrap()
            .is_none_or(|at| at.elapsed() >= std::time::Duration::from_secs(1));
        if stale && !self.siblings.is_empty() {
            let _ = self.refresh_head_map().await;
        }
    }

    fn map_abs(&self, x: i32, y: i32) -> (u32, u32) {
        let (x, y) = (i64::from(x), i64::from(y));
        if let Some(map) = *self.scanout_map.lock().unwrap() {
            return map_abs_position(&map, x, y);
        }
        match *self.head_map.lock().unwrap() {
            Some(map) => map_abs_position(&map, x, y),
            None => clamp_to_console(*self.self_size.lock().unwrap(), x, y),
        }
    }

    pub(super) async fn handle_input(&self, input: InputEvent) -> Result<()> {
        match input {
            InputEvent::KeyPress(keycode) => self
                .keyboard
                .press(keycode)
                .await
                .with_context(|| format!("failed to send key press for qnum {keycode}")),
            InputEvent::KeyRelease(keycode) => self
                .keyboard
                .release(keycode)
                .await
                .with_context(|| format!("failed to send key release for qnum {keycode}")),
            InputEvent::ClipboardViewerFocused(_)
            | InputEvent::ClipboardHostChanged(_, _)
            | InputEvent::UiInfo { .. } => Ok(()),
            InputEvent::MousePress(button) => self
                .mouse
                .press(button)
                .await
                .with_context(|| format!("failed to send mouse press for {button:?}")),
            InputEvent::MouseRelease(button) => self
                .mouse
                .release(button)
                .await
                .with_context(|| format!("failed to send mouse release for {button:?}")),
            InputEvent::MouseAbs {
                x,
                y,
                refresh_layout,
            } => {
                // Refresh before positioning a click, not after its coordinates
                // have already been mapped using an older layout.
                if refresh_layout {
                    let _ = self.refresh_head_map().await;
                } else {
                    self.refresh_head_map_if_stale().await;
                }
                let (mx, my) = self.map_abs(x, y);
                self.mouse
                    .set_abs_position(mx, my)
                    .await
                    .with_context(|| format!("failed to move the absolute mouse to {x},{y}"))
            }
            InputEvent::MouseRel { dx, dy } => self
                .mouse
                .rel_motion(dx, dy)
                .await
                .with_context(|| format!("failed to move the relative mouse by {dx},{dy}")),
            InputEvent::MouseWheel(button) => {
                self.mouse
                    .press(button)
                    .await
                    .with_context(|| format!("failed to send mouse wheel press for {button:?}"))?;
                self.mouse
                    .release(button)
                    .await
                    .with_context(|| format!("failed to send mouse wheel release for {button:?}"))
            }
        }
    }

    /// Register the local peer-to-peer listener object that QEMU pushes scanout
    /// updates into for this console.
    pub(super) async fn register_listener(&mut self, event_tx: EventSender) -> Result<()> {
        #[cfg(not(unix))]
        {
            let _ = event_tx;
            bail!("`qd2 connect` currently requires a Unix platform");
        }

        #[cfg(unix)]
        {
            use std::os::unix::net::UnixStream;

            let (socket0, socket1) =
                UnixStream::pair().context("failed to allocate the listener socket pair")?;
            let listener_fd: Fd<'_> = (&socket0).into();
            let shared = Arc::new(SharedListenerState::new(event_tx, self.scanout_map.clone()));

            self.proxy
                .register_listener(listener_fd)
                .await
                .context("QEMU rejected the display listener registration")?;

            let listener_connection = zbus::connection::Builder::unix_stream(socket1)
                .p2p()
                .serve_at(LISTENER_PATH, LocalConsoleListener::new(shared.clone()))?
                .build()
                .await
                .context("failed to publish the local QEMU display listener")?;

            listener_connection
                .object_server()
                .at(LISTENER_PATH, LocalConsoleListenerMap::new(shared.clone()))
                .await
                .context("failed to publish the shared-memory listener interface")?;
            listener_connection
                .object_server()
                .at(LISTENER_PATH, LocalConsoleListenerDmabuf2::new(shared))
                .await
                .context("failed to publish the DMABUF2 listener interface")?;

            self.listener_connection = Some(listener_connection);
            Ok(())
        }
    }
}

struct SharedListenerState {
    handler: Mutex<FrameStreamHandler>,
    disconnected: AtomicBool,
    scanout_map: Arc<std::sync::Mutex<Option<HeadMap>>>,
}

impl SharedListenerState {
    fn new(event_tx: EventSender, scanout_map: Arc<std::sync::Mutex<Option<HeadMap>>>) -> Self {
        Self {
            handler: Mutex::new(FrameStreamHandler::new(event_tx)),
            disconnected: AtomicBool::new(false),
            scanout_map,
        }
    }

    /// Record (or clear) the head geometry the latest scanout implies.
    fn set_scanout_map(&self, map: Option<HeadMap>) {
        *self.scanout_map.lock().unwrap() = map;
    }

    fn with_handler<T>(&self, f: impl FnOnce(&mut FrameStreamHandler) -> T) -> T {
        let mut handler = self.handler.lock().expect("listener mutex was poisoned");
        f(&mut handler)
    }

    fn disconnected(&self) {
        if !self.disconnected.swap(true, Ordering::SeqCst) {
            self.with_handler(|handler| handler.disconnected());
        }
    }

    fn interfaces(&self) -> Vec<String> {
        self.with_handler(|handler| handler.interfaces())
    }
}

#[derive(Clone)]
struct LocalConsoleListener {
    shared: Arc<SharedListenerState>,
}

impl LocalConsoleListener {
    fn new(shared: Arc<SharedListenerState>) -> Self {
        Self { shared }
    }
}

impl Drop for LocalConsoleListener {
    fn drop(&mut self) {
        self.shared.disconnected();
    }
}

#[zbus::interface(name = "org.qemu.Display1.Listener", spawn = false)]
impl LocalConsoleListener {
    async fn scanout(
        &mut self,
        width: u32,
        height: u32,
        stride: u32,
        format: u32,
        data: serde_bytes::ByteBuf,
    ) {
        self.shared.set_scanout_map(None);
        self.shared.with_handler(|handler| {
            handler.scanout(Scanout {
                width,
                height,
                stride,
                format,
                data: data.into_vec(),
            });
        });
    }

    async fn update(
        &mut self,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        stride: u32,
        format: u32,
        data: serde_bytes::ByteBuf,
    ) {
        self.shared.with_handler(|handler| {
            handler.update(Update {
                x,
                y,
                w,
                h,
                stride,
                format,
                data: data.into_vec(),
            });
        });
    }

    #[cfg(unix)]
    #[zbus(name = "ScanoutDMABUF")]
    async fn scanout_dmabuf(
        &mut self,
        fd: Fd<'_>,
        width: u32,
        height: u32,
        stride: u32,
        fourcc: u32,
        modifier: u64,
        y0_top: bool,
    ) -> zbus::fdo::Result<()> {
        let fd = fd
            .as_fd()
            .try_clone_to_owned()
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;

        self.shared.set_scanout_map(None);
        self.shared.with_handler(|handler| {
            handler.scanout_dmabuf(ScanoutDMABUF {
                fd: [fd.into_raw_fd(), -1, -1, -1],
                width,
                height,
                offset: [0; 4],
                stride: [stride, 0, 0, 0],
                fourcc,
                modifier,
                y0_top,
                num_planes: 1,
            });
        });

        Ok(())
    }

    #[cfg(unix)]
    #[zbus(name = "UpdateDMABUF")]
    async fn update_dmabuf(&mut self, x: i32, y: i32, w: i32, h: i32) -> zbus::fdo::Result<()> {
        self.shared.with_handler(|handler| {
            handler.update_dmabuf(UpdateDMABUF { x, y, w, h });
        });

        Ok(())
    }

    async fn disable(&mut self) {
        self.shared.set_scanout_map(None);
        self.shared.with_handler(|handler| handler.disable());
    }

    async fn mouse_set(&mut self, x: i32, y: i32, on: i32) {
        self.shared
            .with_handler(|handler| handler.mouse_set(MouseSet { x, y, on }));
    }

    async fn cursor_define(
        &mut self,
        width: i32,
        height: i32,
        hot_x: i32,
        hot_y: i32,
        data: Vec<u8>,
    ) {
        self.shared.with_handler(|handler| {
            handler.cursor_define(Cursor {
                width,
                height,
                hot_x,
                hot_y,
                data,
            });
        });
    }

    #[zbus(property)]
    fn interfaces(&self) -> Vec<String> {
        self.shared.interfaces()
    }
}

#[cfg(unix)]
#[derive(Clone)]
struct LocalConsoleListenerMap {
    shared: Arc<SharedListenerState>,
}

#[cfg(unix)]
impl LocalConsoleListenerMap {
    fn new(shared: Arc<SharedListenerState>) -> Self {
        Self { shared }
    }
}

#[cfg(unix)]
impl Drop for LocalConsoleListenerMap {
    fn drop(&mut self) {
        self.shared.disconnected();
    }
}

#[cfg(unix)]
#[zbus::interface(name = "org.qemu.Display1.Listener.Unix.Map", spawn = false)]
impl LocalConsoleListenerMap {
    async fn scanout_map(
        &mut self,
        fd: Fd<'_>,
        offset: u32,
        width: u32,
        height: u32,
        stride: u32,
        format: u32,
    ) -> zbus::fdo::Result<()> {
        let fd = fd
            .as_fd()
            .try_clone_to_owned()
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;

        self.shared.set_scanout_map(None);
        self.shared.with_handler(|handler| {
            handler.scanout_map(ScanoutMap {
                fd,
                offset,
                width,
                height,
                stride,
                format,
            });
        });

        Ok(())
    }

    async fn update_map(&mut self, x: i32, y: i32, w: i32, h: i32) -> zbus::fdo::Result<()> {
        self.shared
            .with_handler(|handler| handler.update_map(UpdateMap { x, y, w, h }));
        Ok(())
    }
}

#[cfg(unix)]
#[derive(Clone)]
struct LocalConsoleListenerDmabuf2 {
    shared: Arc<SharedListenerState>,
}

#[cfg(unix)]
impl LocalConsoleListenerDmabuf2 {
    fn new(shared: Arc<SharedListenerState>) -> Self {
        Self { shared }
    }
}

#[cfg(unix)]
impl Drop for LocalConsoleListenerDmabuf2 {
    fn drop(&mut self) {
        self.shared.disconnected();
    }
}

#[cfg(unix)]
#[zbus::interface(name = "org.qemu.Display1.Listener.Unix.ScanoutDMABUF2", spawn = false)]
impl LocalConsoleListenerDmabuf2 {
    #[zbus(name = "ScanoutDMABUF2")]
    async fn scanout_dmabuf(
        &mut self,
        fd: Vec<Fd<'_>>,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        offset: Vec<u32>,
        stride: Vec<u32>,
        num_planes: u32,
        fourcc: u32,
        backing_width: u32,
        backing_height: u32,
        modifier: u64,
        y0_top: bool,
    ) -> zbus::fdo::Result<()> {
        // The buffer is `backing_width` x `backing_height`; this console shows
        // the (x, y, width, height) window of it. X11 guests scan every head
        // out of one shared framebuffer, so the two differ whenever the guest
        // has more than one output. Older QEMUs report a zero backing size.
        let (texture_width, texture_height, view) = super::super::dmabuf::scanout_geometry(
            x,
            y,
            width,
            height,
            backing_width,
            backing_height,
        );
        self.shared.set_scanout_map(HeadMap::from_scanout(
            view.x,
            view.y,
            view.width,
            view.height,
            texture_width,
            texture_height,
        ));
        let mut fds = [-1; 4];
        for (index, fd) in fd.into_iter().take(4).enumerate() {
            let owned = fd
                .as_fd()
                .try_clone_to_owned()
                .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
            fds[index] = owned.into_raw_fd();
        }

        let mut offsets = [0; 4];
        for (index, value) in offset.into_iter().take(4).enumerate() {
            offsets[index] = value;
        }

        let mut strides = [0; 4];
        for (index, value) in stride.into_iter().take(4).enumerate() {
            strides[index] = value;
        }

        self.shared.with_handler(|handler| {
            match super::super::dmabuf::DmabufFrame::try_from_raw_parts(
                fds,
                texture_width,
                texture_height,
                offsets,
                strides,
                fourcc,
                modifier,
                y0_top,
                num_planes,
                view,
            ) {
                Ok(scanout) => handler.emit_dmabuf_scanout(scanout),
                Err(error) => handler.send_status(format!("Unsupported DMABUF scanout: {error:#}")),
            }
        });

        Ok(())
    }
}
