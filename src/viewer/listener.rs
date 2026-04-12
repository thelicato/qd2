use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc::{Sender, SyncSender},
};

use anyhow::{Context, Result};
use qemu_display::{
    ConsoleProxy, Cursor, KeyboardProxy, MouseProxy, MouseSet, Scanout, Update, UpdateDMABUF,
};
#[cfg(unix)]
use qemu_display::{ScanoutDMABUF, ScanoutMap, UpdateMap};
use tokio::sync::{mpsc as tokio_mpsc, oneshot};
use zbus::{
    Connection,
    zvariant::{Fd, OwnedObjectPath},
};

#[cfg(unix)]
use std::os::fd::{AsFd, IntoRawFd};

use crate::qemu::{self, ConnectTarget};

use super::{
    InputEvent, ViewerEvent, ViewerReady, audio, clipboard,
    framebuffer::FrameStreamHandler,
    mouse::{self, MouseMode},
};

const LISTENER_PATH: &str = "/org/qemu/Display1/Listener";

pub(super) fn run_listener_thread(
    target: ConnectTarget,
    event_tx: Sender<ViewerEvent>,
    ready_tx: SyncSender<Result<ViewerReady>>,
    input_rx: tokio_mpsc::UnboundedReceiver<InputEvent>,
    shutdown_rx: oneshot::Receiver<()>,
) {
    let result = tokio::runtime::Runtime::new()
        .context("failed to create the async runtime for the display listener")
        .and_then(|runtime| {
            runtime.block_on(listener_main(
                target,
                event_tx,
                ready_tx,
                input_rx,
                shutdown_rx,
            ))
        });

    if let Err(error) = result {
        eprintln!("QD2 listener error: {error:#}");
    }
}

async fn listener_main(
    target: ConnectTarget,
    event_tx: Sender<ViewerEvent>,
    ready_tx: SyncSender<Result<ViewerReady>>,
    mut input_rx: tokio_mpsc::UnboundedReceiver<InputEvent>,
    mut shutdown_rx: oneshot::Receiver<()>,
) -> Result<()> {
    let connection = qemu::connect(target.source_address.as_deref()).await?;
    let mut console = RemoteConsole::new(&connection, &target.owner, target.console_id)
        .await
        .with_context(|| format!("failed to open console {}", target.console_id))?;

    console
        .register_listener(event_tx.clone())
        .await
        .context("failed to register the QEMU display listener")?;
    let clipboard =
        clipboard::register_clipboard_bridge(&connection, &target.owner, event_tx.clone())
            .await
            .context("failed to initialize clipboard sharing")?;
    let _audio =
        match audio::register_audio_output(&connection, &target.owner, &target.vm_name).await {
            Ok(audio) => audio,
            Err(error) => {
                eprintln!("QD2 audio error: {error:#}");
                None
            }
        };
    clipboard::debug(format!(
        "listener clipboard availability: {}",
        if clipboard.is_some() {
            "present"
        } else {
            "absent"
        }
    ));

    let title = format!(
        "QD2 - {} - Console {} ({})",
        target.vm_name, target.console_id, target.console_label
    );
    let keyboard_available = target
        .console_interfaces
        .iter()
        .any(|interface| interface == "org.qemu.Display1.Keyboard");
    let mouse_available = target
        .console_interfaces
        .iter()
        .any(|interface| interface == "org.qemu.Display1.Mouse");
    let mut mouse_mode = if mouse_available {
        match console.mouse_is_absolute().await {
            Ok(is_absolute) => MouseMode::from_is_absolute(is_absolute),
            Err(error) => {
                let _ = event_tx.send(ViewerEvent::Status(format!(
                    "Could not detect mouse mode: {error:#}"
                )));
                MouseMode::Relative
            }
        }
    } else {
        MouseMode::Disabled
    };

    let _ = ready_tx.send(Ok(ViewerReady {
        title,
        width: target.width,
        height: target.height,
        keyboard_available,
        clipboard_available: clipboard.is_some(),
        mouse_mode,
    }));

    loop {
        tokio::select! {
            _ = &mut shutdown_rx => break,
            maybe_input = input_rx.recv() => match maybe_input {
                Some(input) => {
                    if let InputEvent::ClipboardHostChanged(selection, content) = &input {
                        clipboard::debug(format!(
                            "listener received ClipboardHostChanged selection={selection:?}: {}",
                            content
                                .as_ref()
                                .map(|content| content.describe())
                                .unwrap_or_else(|| "empty".to_owned())
                        ));
                        if let Some(clipboard) = &clipboard {
                            if let Err(error) = clipboard
                                .update_host_content(*selection, content.clone())
                                .await
                            {
                                super::clipboard::debug(format!(
                                    "update_host_content failed: {error:#}"
                                ));
                                let _ = event_tx.send(ViewerEvent::Status(format!(
                                    "Clipboard sharing failed: {error:#}"
                                )));
                            }
                        } else {
                            super::clipboard::debug(
                                "dropping ClipboardHostChanged because no QEMU clipboard is available",
                            );
                        }
                        continue;
                    }

                    let needs_mouse_mode = mouse::input_needs_mouse_mode(&input);
                    if let Err(error) = console.handle_input(input).await {
                        let recovered = if needs_mouse_mode {
                            match console.mouse_is_absolute().await {
                                Ok(is_absolute) => {
                                    let detected_mode = MouseMode::from_is_absolute(is_absolute);
                                    if detected_mode != mouse_mode {
                                        mouse_mode = detected_mode;
                                        let _ = event_tx.send(ViewerEvent::MouseModeChanged(detected_mode));
                                        true
                                    } else {
                                        false
                                    }
                                }
                                Err(mode_error) => {
                                    let _ = event_tx.send(ViewerEvent::Status(format!(
                                        "Input forwarding failed: {error:#}\n\nCould not re-check the mouse mode: {mode_error:#}"
                                    )));
                                    true
                                }
                            }
                        } else {
                            false
                        };

                        if !recovered {
                            let _ = event_tx.send(ViewerEvent::Status(format!(
                                "Input forwarding failed: {error:#}"
                            )));
                        }
                    }
                }
                None => break,
            }
        }
    }

    drop(console);
    drop(connection);
    Ok(())
}

struct RemoteConsole {
    proxy: ConsoleProxy<'static>,
    keyboard: KeyboardProxy<'static>,
    mouse: MouseProxy<'static>,
    listener_connection: Option<Connection>,
}

impl RemoteConsole {
    async fn new(connection: &Connection, owner: &str, console_id: u32) -> Result<Self> {
        let object_path =
            OwnedObjectPath::try_from(format!("/org/qemu/Display1/Console_{console_id}"))?;
        let proxy = ConsoleProxy::builder(connection)
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
        })
    }

    async fn mouse_is_absolute(&self) -> Result<bool> {
        self.mouse
            .is_absolute()
            .await
            .context("failed to query the mouse mode")
    }

    async fn handle_input(&self, input: InputEvent) -> Result<()> {
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
            InputEvent::ClipboardHostChanged(_, _) => Ok(()),
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
            InputEvent::MouseAbs { x, y } => self
                .mouse
                .set_abs_position(x, y)
                .await
                .with_context(|| format!("failed to move the absolute mouse to {x},{y}")),
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
    async fn register_listener(&mut self, event_tx: Sender<ViewerEvent>) -> Result<()> {
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
            let shared = Arc::new(SharedListenerState::new(event_tx));

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
}

impl SharedListenerState {
    fn new(event_tx: Sender<ViewerEvent>) -> Self {
        Self {
            handler: Mutex::new(FrameStreamHandler::new(event_tx)),
            disconnected: AtomicBool::new(false),
        }
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
        _x: u32,
        _y: u32,
        width: u32,
        height: u32,
        offset: Vec<u32>,
        stride: Vec<u32>,
        num_planes: u32,
        fourcc: u32,
        _backing_width: u32,
        _backing_height: u32,
        modifier: u64,
        y0_top: bool,
    ) -> zbus::fdo::Result<()> {
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
            match super::dmabuf::DmabufFrame::try_from_raw_parts(
                fds, width, height, offsets, strides, fourcc, modifier, y0_top, num_planes,
            ) {
                Ok(scanout) => handler.emit_dmabuf_scanout(scanout),
                Err(error) => handler.send_status(format!("Unsupported DMABUF scanout: {error:#}")),
            }
        });

        Ok(())
    }
}
