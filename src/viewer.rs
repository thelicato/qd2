use std::{
    cell::RefCell,
    convert::TryFrom,
    rc::Rc,
    sync::mpsc::{self, Receiver, Sender, SyncSender, TryRecvError},
    thread,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use gtk::{gdk, glib, prelude::*};
use gtk4 as gtk;
use pixman_sys::{
    pixman_format_code_t_PIXMAN_a8b8g8r8, pixman_format_code_t_PIXMAN_a8r8g8b8,
    pixman_format_code_t_PIXMAN_b8g8r8a8, pixman_format_code_t_PIXMAN_b8g8r8x8,
    pixman_format_code_t_PIXMAN_r8g8b8a8, pixman_format_code_t_PIXMAN_r8g8b8x8,
    pixman_format_code_t_PIXMAN_x8b8g8r8, pixman_format_code_t_PIXMAN_x8r8g8b8,
};
#[cfg(unix)]
use qemu_display::ScanoutDMABUF;
use qemu_display::{
    ConsoleListenerHandler, ConsoleProxy, Cursor, MouseSet, Scanout, Update, UpdateDMABUF,
};
use tokio::sync::oneshot;
use zbus::{
    Connection,
    zvariant::{Fd, OwnedObjectPath},
};

use crate::qemu::{self, ConnectTarget};

const LISTENER_PATH: &str = "/org/qemu/Display1/Listener";
const FRAME_POLL_INTERVAL: Duration = Duration::from_millis(16);
const PIXMAN_A8B8G8R8: u32 = pixman_format_code_t_PIXMAN_a8b8g8r8;
const PIXMAN_A8R8G8B8: u32 = pixman_format_code_t_PIXMAN_a8r8g8b8;
const PIXMAN_B8G8R8A8: u32 = pixman_format_code_t_PIXMAN_b8g8r8a8;
const PIXMAN_B8G8R8X8: u32 = pixman_format_code_t_PIXMAN_b8g8r8x8;
const PIXMAN_R8G8B8A8: u32 = pixman_format_code_t_PIXMAN_r8g8b8a8;
const PIXMAN_R8G8B8X8: u32 = pixman_format_code_t_PIXMAN_r8g8b8x8;
const PIXMAN_X8B8G8R8: u32 = pixman_format_code_t_PIXMAN_x8b8g8r8;
const PIXMAN_X8R8G8B8: u32 = pixman_format_code_t_PIXMAN_x8r8g8b8;
const RGBA_BYTES_PER_PIXEL: usize = 4;

pub fn connect(target: ConnectTarget) -> Result<()> {
    let (event_tx, event_rx) = mpsc::channel();
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let (shutdown_tx, shutdown_rx) = oneshot::channel();

    let join_handle = thread::Builder::new()
        .name("qd2-display-listener".to_owned())
        .spawn({
            let target = target.clone();
            move || run_listener_thread(target, event_tx, ready_tx, shutdown_rx)
        })
        .context("failed to spawn the QEMU display listener thread")?;

    let ready = ready_rx
        .recv()
        .context("display listener thread ended before it reported startup state")??;

    let ui_result = run_window(&target, &ready, event_rx);

    let _ = shutdown_tx.send(());
    join_handle
        .join()
        .map_err(|_| anyhow::anyhow!("display listener thread panicked"))?;

    ui_result
}

fn run_listener_thread(
    target: ConnectTarget,
    event_tx: Sender<ViewerEvent>,
    ready_tx: SyncSender<Result<ViewerReady>>,
    shutdown_rx: oneshot::Receiver<()>,
) {
    let result = tokio::runtime::Runtime::new()
        .context("failed to create the async runtime for the display listener")
        .and_then(|runtime| {
            runtime.block_on(listener_main(target, event_tx, ready_tx, shutdown_rx))
        });

    if let Err(error) = result {
        eprintln!("QD2 listener error: {error:#}");
    }
}

async fn listener_main(
    target: ConnectTarget,
    event_tx: Sender<ViewerEvent>,
    ready_tx: SyncSender<Result<ViewerReady>>,
    shutdown_rx: oneshot::Receiver<()>,
) -> Result<()> {
    let connection = qemu::connect(target.source_address.as_deref()).await?;
    let mut console = RemoteConsole::new(&connection, &target.owner, target.console_id)
        .await
        .with_context(|| format!("failed to open console {}", target.console_id))?;

    console
        .register_listener(FrameStreamHandler::new(event_tx.clone()))
        .await
        .context("failed to register the QEMU display listener")?;

    let title = format!(
        "QD2 - {} - Console {} ({})",
        target.vm_name, target.console_id, target.console_label
    );

    let _ = ready_tx.send(Ok(ViewerReady {
        title,
        width: target.width,
        height: target.height,
    }));

    match shutdown_rx.await {
        Ok(()) | Err(_) => {}
    }

    drop(console);
    drop(connection);
    Ok(())
}

fn run_window(
    target: &ConnectTarget,
    ready: &ViewerReady,
    event_rx: Receiver<ViewerEvent>,
) -> Result<()> {
    gtk::init().context("failed to initialize GTK4")?;

    let main_loop = glib::MainLoop::new(None, false);
    let (window_width, window_height) = suggested_window_size(ready.width, ready.height);

    let picture = gtk::Picture::new();
    picture.set_hexpand(true);
    picture.set_vexpand(true);
    picture.set_can_shrink(true);
    picture.set_content_fit(gtk::ContentFit::Contain);
    picture.set_visible(false);

    let status_label = gtk::Label::new(Some("Waiting for framebuffer..."));
    status_label.set_wrap(true);
    status_label.set_selectable(true);
    status_label.set_margin_top(12);
    status_label.set_margin_bottom(12);
    status_label.set_margin_start(12);
    status_label.set_margin_end(12);

    let container = gtk::Box::new(gtk::Orientation::Vertical, 0);
    container.append(&status_label);
    container.append(&picture);

    let window = gtk::Window::builder()
        .title(&ready.title)
        .default_width(window_width)
        .default_height(window_height)
        .child(&container)
        .build();
    window.set_resizable(true);

    let event_rx = Rc::new(RefCell::new(event_rx));
    let window_base_title = ready.title.clone();
    glib::timeout_add_local(FRAME_POLL_INTERVAL, {
        let event_rx = event_rx.clone();
        let picture = picture.clone();
        let status_label = status_label.clone();
        let window = window.clone();
        let vm_name = target.vm_name.clone();
        let window_base_title = window_base_title.clone();

        move || {
            let mut latest_frame = None;
            let mut latest_status = None;
            let mut disconnected = false;

            loop {
                let event = {
                    let receiver = event_rx.borrow_mut();
                    receiver.try_recv()
                };

                match event {
                    Ok(ViewerEvent::Frame(frame)) => latest_frame = Some(frame),
                    Ok(ViewerEvent::Status(message)) => latest_status = Some(message),
                    Ok(ViewerEvent::Disconnected) => disconnected = true,
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }

            if let Some(frame) = latest_frame {
                let bytes = glib::Bytes::from_owned(frame.data);
                let texture = gdk::MemoryTexture::new(
                    i32::try_from(frame.width).unwrap_or(i32::MAX),
                    i32::try_from(frame.height).unwrap_or(i32::MAX),
                    gdk::MemoryFormat::R8g8b8a8,
                    &bytes,
                    frame.stride,
                );
                picture.set_paintable(Some(&texture));
                picture.set_visible(true);
                status_label.set_visible(false);
                window.set_title(Some(&format!(
                    "{} - {}x{}",
                    window_base_title, frame.width, frame.height
                )));
            }

            if let Some(message) = latest_status {
                status_label.set_label(&message);
                status_label.set_visible(true);
            }

            if disconnected {
                status_label.set_label(&format!("Disconnected from `{vm_name}`."));
                status_label.set_visible(true);
            }

            glib::ControlFlow::Continue
        }
    });

    window.connect_close_request({
        let main_loop = main_loop.clone();
        move |_| {
            main_loop.quit();
            glib::Propagation::Proceed
        }
    });

    window.present();
    main_loop.run();
    Ok(())
}

struct ViewerReady {
    title: String,
    width: u32,
    height: u32,
}

enum ViewerEvent {
    Frame(FrameSnapshot),
    Status(String),
    Disconnected,
}

#[derive(Clone)]
struct FrameSnapshot {
    width: u32,
    height: u32,
    stride: usize,
    data: Vec<u8>,
}

struct Framebuffer {
    width: u32,
    height: u32,
    stride: usize,
    format: PixelFormat,
    data: Vec<u8>,
}

impl Framebuffer {
    fn from_scanout(scanout: Scanout) -> Result<Self> {
        let format = PixelFormat::try_from(scanout.format)?;
        let stride = usize::try_from(scanout.stride).context("invalid framebuffer stride")?;
        let expected_len = usize::try_from(scanout.height)
            .context("invalid framebuffer height")?
            .checked_mul(stride)
            .context("framebuffer size overflow")?;

        if scanout.data.len() < expected_len {
            bail!(
                "scanout payload is too short for {}x{} stride {}",
                scanout.width,
                scanout.height,
                scanout.stride
            );
        }

        Ok(Self {
            width: scanout.width,
            height: scanout.height,
            stride,
            format,
            data: scanout.data,
        })
    }

    fn apply_update(&mut self, update: Update) -> Result<()> {
        let update_format = PixelFormat::try_from(update.format)?;
        if update_format != self.format {
            bail!(
                "update format {:#x} does not match the current framebuffer format {:#x}",
                update.format,
                self.format.pixman_code()
            );
        }

        let x = usize::try_from(update.x).context("negative update x coordinate")?;
        let y = usize::try_from(update.y).context("negative update y coordinate")?;
        let width = usize::try_from(update.w).context("negative update width")?;
        let height = usize::try_from(update.h).context("negative update height")?;
        let src_stride = usize::try_from(update.stride).context("invalid update stride")?;
        let row_len = width
            .checked_mul(self.format.bytes_per_pixel())
            .context("update row size overflow")?;

        for row in 0..height {
            let src_start = row
                .checked_mul(src_stride)
                .context("update source offset overflow")?;
            let src_end = src_start
                .checked_add(row_len)
                .context("update source range overflow")?;
            let dst_start = y
                .checked_add(row)
                .context("update destination y overflow")?
                .checked_mul(self.stride)
                .context("update destination row overflow")?
                .checked_add(
                    x.checked_mul(self.format.bytes_per_pixel())
                        .context("update destination x overflow")?,
                )
                .context("update destination offset overflow")?;
            let dst_end = dst_start
                .checked_add(row_len)
                .context("update destination range overflow")?;

            if src_end > update.data.len() || dst_end > self.data.len() {
                bail!("update rectangle falls outside the framebuffer bounds");
            }

            self.data[dst_start..dst_end].copy_from_slice(&update.data[src_start..src_end]);
        }

        Ok(())
    }

    fn snapshot(&self) -> FrameSnapshot {
        FrameSnapshot {
            width: self.width,
            height: self.height,
            stride: self.width as usize * RGBA_BYTES_PER_PIXEL,
            data: self.format.to_rgba_bytes(
                self.width as usize,
                self.height as usize,
                self.stride,
                &self.data,
            ),
        }
    }
}

struct FrameStreamHandler {
    event_tx: Sender<ViewerEvent>,
    framebuffer: Option<Framebuffer>,
    dmabuf_reported: bool,
}

impl FrameStreamHandler {
    fn new(event_tx: Sender<ViewerEvent>) -> Self {
        Self {
            event_tx,
            framebuffer: None,
            dmabuf_reported: false,
        }
    }

    fn send_status(&self, message: impl Into<String>) {
        let _ = self.event_tx.send(ViewerEvent::Status(message.into()));
    }

    fn send_current_frame(&self) {
        if let Some(framebuffer) = &self.framebuffer {
            let _ = self
                .event_tx
                .send(ViewerEvent::Frame(framebuffer.snapshot()));
        }
    }
}

#[async_trait]
impl ConsoleListenerHandler for FrameStreamHandler {
    async fn scanout(&mut self, scanout: Scanout) {
        match Framebuffer::from_scanout(scanout) {
            Ok(framebuffer) => {
                self.framebuffer = Some(framebuffer);
                self.send_current_frame();
            }
            Err(error) => self.send_status(format!("Unsupported framebuffer: {error:#}")),
        }
    }

    async fn update(&mut self, update: Update) {
        let Some(framebuffer) = &mut self.framebuffer else {
            return;
        };

        match framebuffer.apply_update(update) {
            Ok(()) => self.send_current_frame(),
            Err(error) => {
                self.send_status(format!("Failed to apply framebuffer update: {error:#}"))
            }
        }
    }

    #[cfg(unix)]
    async fn scanout_dmabuf(&mut self, _scanout: ScanoutDMABUF) {
        if !self.dmabuf_reported {
            self.dmabuf_reported = true;
            self.send_status("DMABUF scanout is not supported by `qd2 connect` yet.");
        }
    }

    #[cfg(unix)]
    async fn update_dmabuf(&mut self, _update: UpdateDMABUF) {
        if !self.dmabuf_reported {
            self.dmabuf_reported = true;
            self.send_status("DMABUF updates are not supported by `qd2 connect` yet.");
        }
    }

    async fn disable(&mut self) {
        self.framebuffer = None;
        self.send_status("The guest display was disabled.");
    }

    async fn mouse_set(&mut self, _set: MouseSet) {}

    async fn cursor_define(&mut self, _cursor: Cursor) {}

    fn disconnected(&mut self) {
        let _ = self.event_tx.send(ViewerEvent::Disconnected);
    }

    fn interfaces(&self) -> Vec<String> {
        Vec::new()
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum PixelFormat {
    A8b8g8r8,
    A8r8g8b8,
    B8g8r8a8,
    B8g8r8x8,
    R8g8b8a8,
    R8g8b8x8,
    X8b8g8r8,
    X8r8g8b8,
}

impl PixelFormat {
    fn bytes_per_pixel(self) -> usize {
        RGBA_BYTES_PER_PIXEL
    }

    fn pixman_code(self) -> u32 {
        match self {
            Self::A8b8g8r8 => PIXMAN_A8B8G8R8,
            Self::A8r8g8b8 => PIXMAN_A8R8G8B8,
            Self::B8g8r8a8 => PIXMAN_B8G8R8A8,
            Self::B8g8r8x8 => PIXMAN_B8G8R8X8,
            Self::R8g8b8a8 => PIXMAN_R8G8B8A8,
            Self::R8g8b8x8 => PIXMAN_R8G8B8X8,
            Self::X8b8g8r8 => PIXMAN_X8B8G8R8,
            Self::X8r8g8b8 => PIXMAN_X8R8G8B8,
        }
    }

    fn to_rgba_bytes(self, width: usize, height: usize, stride: usize, data: &[u8]) -> Vec<u8> {
        let mut rgba = vec![0; width * height * RGBA_BYTES_PER_PIXEL];

        for y in 0..height {
            let src_row_start = y * stride;
            let dst_row_start = y * width * RGBA_BYTES_PER_PIXEL;
            let src_row = &data[src_row_start..src_row_start + width * self.bytes_per_pixel()];
            let dst_row = &mut rgba[dst_row_start..dst_row_start + width * RGBA_BYTES_PER_PIXEL];

            for (src, dst) in src_row
                .chunks_exact(self.bytes_per_pixel())
                .zip(dst_row.chunks_exact_mut(RGBA_BYTES_PER_PIXEL))
            {
                dst.copy_from_slice(&self.pixel_to_rgba(src));
            }
        }

        rgba
    }

    fn pixel_to_rgba(self, src: &[u8]) -> [u8; RGBA_BYTES_PER_PIXEL] {
        debug_assert_eq!(src.len(), self.bytes_per_pixel());

        #[cfg(target_endian = "little")]
        {
            match self {
                Self::A8b8g8r8 => [src[0], src[1], src[2], src[3]],
                Self::A8r8g8b8 => [src[2], src[1], src[0], src[3]],
                Self::B8g8r8a8 => [src[1], src[2], src[3], src[0]],
                Self::B8g8r8x8 => [src[1], src[2], src[3], u8::MAX],
                Self::R8g8b8a8 => [src[3], src[2], src[1], src[0]],
                Self::R8g8b8x8 => [src[3], src[2], src[1], u8::MAX],
                Self::X8b8g8r8 => [src[0], src[1], src[2], u8::MAX],
                Self::X8r8g8b8 => [src[2], src[1], src[0], u8::MAX],
            }
        }

        #[cfg(target_endian = "big")]
        {
            match self {
                Self::A8b8g8r8 => [src[3], src[2], src[1], src[0]],
                Self::A8r8g8b8 => [src[1], src[2], src[3], src[0]],
                Self::B8g8r8a8 => [src[2], src[1], src[0], src[3]],
                Self::B8g8r8x8 => [src[2], src[1], src[0], u8::MAX],
                Self::R8g8b8a8 => [src[0], src[1], src[2], src[3]],
                Self::R8g8b8x8 => [src[0], src[1], src[2], u8::MAX],
                Self::X8b8g8r8 => [src[3], src[2], src[1], u8::MAX],
                Self::X8r8g8b8 => [src[1], src[2], src[3], u8::MAX],
            }
        }
    }
}

impl TryFrom<u32> for PixelFormat {
    type Error = anyhow::Error;

    fn try_from(value: u32) -> Result<Self> {
        match value {
            PIXMAN_A8B8G8R8 => Ok(Self::A8b8g8r8),
            PIXMAN_A8R8G8B8 => Ok(Self::A8r8g8b8),
            PIXMAN_B8G8R8A8 => Ok(Self::B8g8r8a8),
            PIXMAN_B8G8R8X8 => Ok(Self::B8g8r8x8),
            PIXMAN_R8G8B8A8 => Ok(Self::R8g8b8a8),
            PIXMAN_R8G8B8X8 => Ok(Self::R8g8b8x8),
            PIXMAN_X8B8G8R8 => Ok(Self::X8b8g8r8),
            PIXMAN_X8R8G8B8 => Ok(Self::X8r8g8b8),
            _ => bail!("pixel format {value:#x} is not supported yet"),
        }
    }
}

struct RemoteConsole {
    proxy: ConsoleProxy<'static>,
    listener_connection: Option<Connection>,
}

impl RemoteConsole {
    async fn new(connection: &Connection, owner: &str, console_id: u32) -> Result<Self> {
        let object_path =
            OwnedObjectPath::try_from(format!("/org/qemu/Display1/Console_{console_id}"))?;
        let proxy = ConsoleProxy::builder(connection)
            .destination(owner.to_owned())?
            .path(object_path)?
            .build()
            .await
            .with_context(|| format!("failed to build the console proxy for owner `{owner}`"))?;

        Ok(Self {
            proxy,
            listener_connection: None,
        })
    }

    async fn register_listener<H: ConsoleListenerHandler>(&mut self, handler: H) -> Result<()> {
        #[cfg(not(unix))]
        {
            let _ = handler;
            bail!("`qd2 connect` currently requires a Unix platform");
        }

        #[cfg(unix)]
        {
            use std::os::unix::net::UnixStream;

            let (socket0, socket1) =
                UnixStream::pair().context("failed to allocate the listener socket pair")?;
            let listener_fd: Fd<'_> = (&socket0).into();

            self.proxy
                .register_listener(listener_fd)
                .await
                .context("QEMU rejected the display listener registration")?;

            let listener_connection = zbus::connection::Builder::unix_stream(socket1)
                .p2p()
                .serve_at(LISTENER_PATH, LocalConsoleListener::new(handler))?
                .build()
                .await
                .context("failed to publish the local QEMU display listener")?;

            self.listener_connection = Some(listener_connection);
            Ok(())
        }
    }
}

#[derive(Debug)]
struct LocalConsoleListener<H: ConsoleListenerHandler> {
    handler: H,
}

impl<H: ConsoleListenerHandler> LocalConsoleListener<H> {
    fn new(handler: H) -> Self {
        Self { handler }
    }
}

impl<H: ConsoleListenerHandler> Drop for LocalConsoleListener<H> {
    fn drop(&mut self) {
        self.handler.disconnected();
    }
}

#[zbus::interface(name = "org.qemu.Display1.Listener", spawn = false)]
impl<H: ConsoleListenerHandler> LocalConsoleListener<H> {
    async fn scanout(
        &mut self,
        width: u32,
        height: u32,
        stride: u32,
        format: u32,
        data: serde_bytes::ByteBuf,
    ) {
        self.handler
            .scanout(Scanout {
                width,
                height,
                stride,
                format,
                data: data.into_vec(),
            })
            .await;
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
        self.handler
            .update(Update {
                x,
                y,
                w,
                h,
                stride,
                format,
                data: data.into_vec(),
            })
            .await;
    }

    #[cfg(unix)]
    #[zbus(name = "ScanoutDMABUF")]
    async fn scanout_dmabuf(
        &mut self,
        _fd: Fd<'_>,
        _width: u32,
        _height: u32,
        _stride: u32,
        _fourcc: u32,
        _modifier: u64,
        _y0_top: bool,
    ) -> zbus::fdo::Result<()> {
        Err(zbus::fdo::Error::NotSupported(
            "DMABUF scanout is not supported by `qd2 connect` yet".into(),
        ))
    }

    #[cfg(unix)]
    #[zbus(name = "UpdateDMABUF")]
    async fn update_dmabuf(&mut self, _x: i32, _y: i32, _w: i32, _h: i32) -> zbus::fdo::Result<()> {
        Err(zbus::fdo::Error::NotSupported(
            "DMABUF updates are not supported by `qd2 connect` yet".into(),
        ))
    }

    async fn disable(&mut self) {
        self.handler.disable().await;
    }

    async fn mouse_set(&mut self, x: i32, y: i32, on: i32) {
        self.handler.mouse_set(MouseSet { x, y, on }).await;
    }

    async fn cursor_define(
        &mut self,
        width: i32,
        height: i32,
        hot_x: i32,
        hot_y: i32,
        data: Vec<u8>,
    ) {
        self.handler
            .cursor_define(Cursor {
                width,
                height,
                hot_x,
                hot_y,
                data,
            })
            .await;
    }

    #[zbus(property)]
    fn interfaces(&self) -> Vec<String> {
        self.handler.interfaces()
    }
}

fn suggested_window_size(width: u32, height: u32) -> (i32, i32) {
    let width = width.clamp(640, 1280);
    let height = height.clamp(480, 960);
    (
        i32::try_from(width).unwrap_or(1280),
        i32::try_from(height).unwrap_or(960),
    )
}

#[cfg(test)]
mod tests {
    use super::{Framebuffer, PixelFormat};
    use qemu_display::Scanout;

    #[test]
    #[cfg(target_endian = "little")]
    fn snapshot_converts_xrgb_pixels_to_rgba() {
        let framebuffer = Framebuffer::from_scanout(Scanout {
            width: 1,
            height: 1,
            stride: 4,
            format: PixelFormat::X8r8g8b8.pixman_code(),
            data: vec![0x10, 0x20, 0x30, 0x00],
        })
        .expect("x8r8g8b8 scanout should be accepted");

        let snapshot = framebuffer.snapshot();
        assert_eq!(snapshot.stride, 4);
        assert_eq!(snapshot.data, vec![0x30, 0x20, 0x10, 0xff]);
    }

    #[test]
    #[cfg(target_endian = "little")]
    fn snapshot_preserves_abgr_alpha() {
        let framebuffer = Framebuffer::from_scanout(Scanout {
            width: 1,
            height: 1,
            stride: 4,
            format: PixelFormat::A8b8g8r8.pixman_code(),
            data: vec![0x12, 0x34, 0x56, 0x78],
        })
        .expect("a8b8g8r8 scanout should be accepted");

        let snapshot = framebuffer.snapshot();
        assert_eq!(snapshot.data, vec![0x12, 0x34, 0x56, 0x78]);
    }

    #[test]
    #[cfg(target_endian = "little")]
    fn snapshot_respects_framebuffer_stride() {
        let framebuffer = Framebuffer::from_scanout(Scanout {
            width: 1,
            height: 2,
            stride: 8,
            format: PixelFormat::B8g8r8x8.pixman_code(),
            data: vec![
                0x00, 0x11, 0x22, 0x33, 0xaa, 0xbb, 0xcc, 0xdd, 0x00, 0x44, 0x55, 0x66, 0xee, 0xff,
                0x11, 0x22,
            ],
        })
        .expect("b8g8r8x8 scanout should be accepted");

        let snapshot = framebuffer.snapshot();
        assert_eq!(snapshot.stride, 4);
        assert_eq!(
            snapshot.data,
            vec![0x11, 0x22, 0x33, 0xff, 0x44, 0x55, 0x66, 0xff]
        );
    }
}
