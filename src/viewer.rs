use std::{
    cell::RefCell,
    convert::TryFrom,
    rc::Rc,
    sync::mpsc::{self as std_mpsc, Receiver, Sender, SyncSender, TryRecvError},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use gtk::{cairo, gdk, glib, prelude::*};
use gtk4 as gtk;
use pixman_sys::{
    pixman_format_code_t_PIXMAN_a8b8g8r8, pixman_format_code_t_PIXMAN_a8r8g8b8,
    pixman_format_code_t_PIXMAN_b8g8r8a8, pixman_format_code_t_PIXMAN_b8g8r8x8,
    pixman_format_code_t_PIXMAN_r8g8b8a8, pixman_format_code_t_PIXMAN_r8g8b8x8,
    pixman_format_code_t_PIXMAN_x8b8g8r8, pixman_format_code_t_PIXMAN_x8r8g8b8,
};
use qemu_display::{
    ConsoleProxy, Cursor, KeyboardProxy, MouseButton, MouseProxy, MouseSet, Scanout, Update,
    UpdateDMABUF,
};
#[cfg(unix)]
use qemu_display::{ScanoutDMABUF, ScanoutMap, ScanoutMmap, UpdateMap};
use tokio::sync::{mpsc as tokio_mpsc, oneshot};
use zbus::{
    Connection,
    zvariant::{Fd, OwnedObjectPath},
};

#[cfg(unix)]
use std::os::fd::{AsFd, AsRawFd, FromRawFd, IntoRawFd, OwnedFd};

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
    let (event_tx, event_rx) = std_mpsc::channel();
    let (ready_tx, ready_rx) = std_mpsc::sync_channel(1);
    let (input_tx, input_rx) = tokio_mpsc::unbounded_channel();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();

    let join_handle = thread::Builder::new()
        .name("qd2-display-listener".to_owned())
        .spawn({
            let target = target.clone();
            move || run_listener_thread(target, event_tx, ready_tx, input_rx, shutdown_rx)
        })
        .context("failed to spawn the QEMU display listener thread")?;

    let ready = ready_rx
        .recv()
        .context("display listener thread ended before it reported startup state")??;

    let ui_result = run_window(&target, &ready, event_rx, input_tx);

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
    let mouse_mode = if mouse_available {
        match console.mouse_is_absolute().await {
            Ok(true) => MouseMode::Absolute,
            Ok(false) => MouseMode::Relative,
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
        mouse_mode,
    }));

    loop {
        tokio::select! {
            _ = &mut shutdown_rx => break,
            maybe_input = input_rx.recv() => match maybe_input {
                Some(input) => {
                    if let Err(error) = console.handle_input(input).await {
                        let _ = event_tx.send(ViewerEvent::Status(format!(
                            "Input forwarding failed: {error:#}"
                        )));
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

fn run_window(
    target: &ConnectTarget,
    ready: &ViewerReady,
    event_rx: Receiver<ViewerEvent>,
    input_tx: tokio_mpsc::UnboundedSender<InputEvent>,
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
    picture.set_focusable(true);

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

    let ui_state = Rc::new(RefCell::new(UiState::default()));
    install_input_controllers(
        &picture,
        ui_state.clone(),
        input_tx,
        ready.mouse_mode,
        ready.keyboard_available,
    );

    let event_rx = Rc::new(RefCell::new(event_rx));
    #[cfg(unix)]
    let current_dmabuf = Rc::new(RefCell::new(None::<DmabufPresentation>));
    let display = gtk::prelude::RootExt::display(&window);
    let window_base_title = ready.title.clone();
    glib::timeout_add_local(FRAME_POLL_INTERVAL, {
        let event_rx = event_rx.clone();
        #[cfg(unix)]
        let current_dmabuf = current_dmabuf.clone();
        let display = display.clone();
        let picture = picture.clone();
        let status_label = status_label.clone();
        let ui_state = ui_state.clone();
        let window = window.clone();
        let vm_name = target.vm_name.clone();
        let window_base_title = window_base_title.clone();

        move || {
            let mut latest_presentation = None;
            #[cfg(unix)]
            let mut dmabuf_updates = Vec::new();
            let mut latest_status = None;
            let mut disconnected = false;

            loop {
                let event = {
                    let receiver = event_rx.borrow_mut();
                    receiver.try_recv()
                };

                match event {
                    Ok(ViewerEvent::Frame(frame)) => {
                        latest_presentation = Some(PresentationEvent::Frame(frame))
                    }
                    #[cfg(unix)]
                    Ok(ViewerEvent::Dmabuf(scanout)) => {
                        latest_presentation = Some(PresentationEvent::Dmabuf(scanout))
                    }
                    #[cfg(unix)]
                    Ok(ViewerEvent::DmabufUpdate(update)) => dmabuf_updates.push(update),
                    Ok(ViewerEvent::Status(message)) => latest_status = Some(message),
                    Ok(ViewerEvent::Disconnected) => disconnected = true,
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }

            if let Some(presentation) = latest_presentation {
                match presentation {
                    PresentationEvent::Frame(frame) => {
                        #[cfg(unix)]
                        {
                            *current_dmabuf.borrow_mut() = None;
                        }

                        let bytes = glib::Bytes::from_owned(frame.data);
                        let texture = gdk::MemoryTexture::new(
                            i32::try_from(frame.width).unwrap_or(i32::MAX),
                            i32::try_from(frame.height).unwrap_or(i32::MAX),
                            gdk::MemoryFormat::R8g8b8a8,
                            &bytes,
                            frame.stride,
                        );
                        present_paintable(
                            &picture,
                            &status_label,
                            &ui_state,
                            &window,
                            &window_base_title,
                            &texture,
                            frame.width,
                            frame.height,
                        );
                    }
                    #[cfg(unix)]
                    PresentationEvent::Dmabuf(scanout) => {
                        match build_dmabuf_presentation(&display, scanout) {
                            Ok(presentation) => match presentation.to_paintable() {
                                Ok(paintable) => {
                                    *current_dmabuf.borrow_mut() = Some(presentation);
                                    present_paintable(
                                        &picture,
                                        &status_label,
                                        &ui_state,
                                        &window,
                                        &window_base_title,
                                        &paintable,
                                        paintable.intrinsic_width().max(0) as u32,
                                        paintable.intrinsic_height().max(0) as u32,
                                    );
                                }
                                Err(error) => {
                                    latest_status = Some(format!(
                                        "Could not prepare the DMABUF scanout for display: {error:#}"
                                    ));
                                }
                            },
                            Err(error) => {
                                *current_dmabuf.borrow_mut() = None;
                                latest_status =
                                    Some(format!("Could not import the DMABUF scanout: {error:#}"));
                            }
                        }
                    }
                }
            }

            #[cfg(unix)]
            if !dmabuf_updates.is_empty() {
                let refreshed = {
                    let mut current_dmabuf = current_dmabuf.borrow_mut();
                    match current_dmabuf.as_mut() {
                        Some(presentation) => match presentation.refresh(&display, &dmabuf_updates)
                        {
                            Ok(()) => presentation.to_paintable().map(Some),
                            Err(error) => Err(error),
                        },
                        None => Ok(None),
                    }
                };

                match refreshed {
                    Ok(Some(paintable)) => {
                        present_paintable(
                            &picture,
                            &status_label,
                            &ui_state,
                            &window,
                            &window_base_title,
                            &paintable,
                            paintable.intrinsic_width().max(0) as u32,
                            paintable.intrinsic_height().max(0) as u32,
                        );
                    }
                    Ok(None) => {
                        if latest_status.is_none() {
                            latest_status = Some(
                                "Received a DMABUF update before the initial scanout was available."
                                    .to_owned(),
                            );
                        }
                    }
                    Err(error) => {
                        latest_status =
                            Some(format!("Could not refresh the DMABUF scanout: {error:#}"));
                    }
                }
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
    picture.grab_focus();
    main_loop.run();
    Ok(())
}

enum PresentationEvent {
    Frame(FrameSnapshot),
    #[cfg(unix)]
    Dmabuf(DmabufFrame),
}

fn present_paintable(
    picture: &gtk::Picture,
    status_label: &gtk::Label,
    ui_state: &Rc<RefCell<UiState>>,
    window: &gtk::Window,
    window_base_title: &str,
    paintable: &impl IsA<gdk::Paintable>,
    width: u32,
    height: u32,
) {
    picture.set_paintable(Some(paintable));
    picture.set_visible(true);
    status_label.set_visible(false);

    let mut ui_state = ui_state.borrow_mut();
    if ui_state.frame_size != Some((width, height)) {
        ui_state.last_pointer_guest_position = None;
    }
    ui_state.frame_size = Some((width, height));

    window.set_title(Some(&format!("{window_base_title} - {width}x{height}")));
}

#[cfg(target_os = "linux")]
fn build_dmabuf_presentation(
    display: &gdk::Display,
    scanout: DmabufFrame,
) -> Result<DmabufPresentation> {
    DmabufPresentation::new(display, scanout)
}

#[cfg(all(unix, not(target_os = "linux")))]
fn build_dmabuf_presentation(
    _display: &gdk::Display,
    _scanout: DmabufFrame,
) -> Result<DmabufPresentation> {
    bail!("DMABUF import is currently supported only on Linux GTK builds")
}

#[cfg(unix)]
struct DmabufPresentation {
    texture: gdk::Texture,
    fds: Vec<OwnedFd>,
    width: u32,
    height: u32,
    offset: [u32; 4],
    stride: [u32; 4],
    fourcc: u32,
    modifier: u64,
    y0_top: bool,
    num_planes: u32,
}

#[cfg(unix)]
impl DmabufPresentation {
    fn new(display: &gdk::Display, scanout: DmabufFrame) -> Result<Self> {
        let texture = build_dmabuf_texture(
            display,
            scanout.width,
            scanout.height,
            &scanout.fds,
            &scanout.offset,
            &scanout.stride,
            scanout.fourcc,
            scanout.modifier,
            scanout.num_planes,
            None,
            None,
        )?;

        Ok(Self {
            texture,
            fds: scanout.fds,
            width: scanout.width,
            height: scanout.height,
            offset: scanout.offset,
            stride: scanout.stride,
            fourcc: scanout.fourcc,
            modifier: scanout.modifier,
            y0_top: scanout.y0_top,
            num_planes: scanout.num_planes,
        })
    }

    fn refresh(&mut self, display: &gdk::Display, updates: &[UpdateDMABUF]) -> Result<()> {
        let update_region = dmabuf_update_region(updates, self.width, self.height);
        let previous_texture = self.texture.clone();

        self.texture = build_dmabuf_texture(
            display,
            self.width,
            self.height,
            &self.fds,
            &self.offset,
            &self.stride,
            self.fourcc,
            self.modifier,
            self.num_planes,
            update_region.as_ref(),
            Some(&previous_texture),
        )?;

        Ok(())
    }

    fn to_paintable(&self) -> Result<gdk::Paintable> {
        if self.y0_top {
            return Ok(self.texture.clone().upcast());
        }

        let snapshot = gtk::Snapshot::new();
        let bounds = gtk::graphene::Rect::new(0.0, 0.0, self.width as f32, self.height as f32);

        snapshot.save();
        snapshot.translate(&gtk::graphene::Point::new(0.0, self.height as f32));
        snapshot.scale(1.0, -1.0);
        snapshot.append_texture(&self.texture, &bounds);
        snapshot.restore();

        snapshot
            .to_paintable(Some(&gtk::graphene::Size::new(
                self.width as f32,
                self.height as f32,
            )))
            .context("failed to build a GTK paintable for the DMABUF texture")
    }
}

#[cfg(target_os = "linux")]
fn build_dmabuf_texture(
    display: &gdk::Display,
    width: u32,
    height: u32,
    fds: &[OwnedFd],
    offset: &[u32; 4],
    stride: &[u32; 4],
    fourcc: u32,
    modifier: u64,
    num_planes: u32,
    update_region: Option<&cairo::Region>,
    update_texture: Option<&gdk::Texture>,
) -> Result<gdk::Texture> {
    if !display.dmabuf_formats().contains(fourcc, modifier) {
        bail!(
            "GTK does not support DMABUF fourcc {:#x} with modifier {:#x}",
            fourcc,
            modifier
        );
    }

    let plane_count = usize::try_from(num_planes).context("invalid DMABUF plane count")?;
    if plane_count != fds.len() {
        bail!(
            "DMABUF reported {} planes but provided {} file descriptors",
            num_planes,
            fds.len()
        );
    }

    let mut duplicated_fds = Vec::with_capacity(plane_count);
    for fd in fds.iter().take(plane_count) {
        duplicated_fds.push(
            fd.as_fd()
                .try_clone_to_owned()
                .context("failed to duplicate the DMABUF plane file descriptor")?,
        );
    }

    let mut builder = gdk::DmabufTextureBuilder::new()
        .set_display(display)
        .set_width(width)
        .set_height(height)
        .set_fourcc(fourcc)
        .set_modifier(modifier)
        .set_n_planes(num_planes);

    if let Some(region) = update_region {
        builder = builder.set_update_region(Some(region));
    }
    if let Some(texture) = update_texture {
        builder = builder.set_update_texture(Some(texture));
    }

    for plane in 0..plane_count {
        builder = builder
            .set_offset(plane as u32, offset[plane])
            .set_stride(plane as u32, stride[plane]);

        // SAFETY: the duplicated OwnedFds stay alive until GTK releases the imported texture.
        builder = unsafe { builder.set_fd(plane as u32, duplicated_fds[plane].as_raw_fd()) };
    }

    let texture = unsafe { builder.build_with_release_func(move || drop(duplicated_fds)) }
        .context("GTK rejected the DMABUF scanout")?;

    Ok(texture)
}

#[cfg(unix)]
fn dmabuf_update_region(
    updates: &[UpdateDMABUF],
    width: u32,
    height: u32,
) -> Option<cairo::Region> {
    let rectangles = updates
        .iter()
        .filter_map(|update| dmabuf_update_rectangle(*update, width, height))
        .collect::<Vec<_>>();

    if rectangles.is_empty() {
        None
    } else {
        Some(cairo::Region::create_rectangles(&rectangles))
    }
}

#[cfg(unix)]
fn dmabuf_update_rectangle(
    update: UpdateDMABUF,
    width: u32,
    height: u32,
) -> Option<cairo::RectangleInt> {
    if update.w <= 0 || update.h <= 0 {
        return None;
    }

    let x0 = update.x.clamp(0, i32::try_from(width).unwrap_or(i32::MAX));
    let y0 = update.y.clamp(0, i32::try_from(height).unwrap_or(i32::MAX));
    let x1 = (i64::from(update.x) + i64::from(update.w)).clamp(0, i64::from(width)) as i32;
    let y1 = (i64::from(update.y) + i64::from(update.h)).clamp(0, i64::from(height)) as i32;

    if x1 <= x0 || y1 <= y0 {
        return None;
    }

    Some(cairo::RectangleInt::new(x0, y0, x1 - x0, y1 - y0))
}

struct ViewerReady {
    title: String,
    width: u32,
    height: u32,
    keyboard_available: bool,
    mouse_mode: MouseMode,
}

#[derive(Default)]
struct UiState {
    frame_size: Option<(u32, u32)>,
    last_pointer_guest_position: Option<(u32, u32)>,
}

enum ViewerEvent {
    Frame(FrameSnapshot),
    #[cfg(unix)]
    Dmabuf(DmabufFrame),
    #[cfg(unix)]
    DmabufUpdate(UpdateDMABUF),
    Status(String),
    Disconnected,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum MouseMode {
    Disabled,
    Relative,
    Absolute,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum InputEvent {
    KeyPress(u32),
    KeyRelease(u32),
    MousePress(MouseButton),
    MouseRelease(MouseButton),
    MouseAbs { x: u32, y: u32 },
    MouseRel { dx: i32, dy: i32 },
    MouseWheel(MouseButton),
}

#[derive(Clone)]
struct FrameSnapshot {
    width: u32,
    height: u32,
    stride: usize,
    data: Vec<u8>,
}

#[cfg(unix)]
struct DmabufFrame {
    fds: Vec<OwnedFd>,
    width: u32,
    height: u32,
    offset: [u32; 4],
    stride: [u32; 4],
    fourcc: u32,
    modifier: u64,
    y0_top: bool,
    num_planes: u32,
}

#[cfg(unix)]
impl DmabufFrame {
    fn try_from_scanout(scanout: ScanoutDMABUF) -> Result<Self> {
        let width = scanout.width;
        let height = scanout.height;
        let offset = scanout.offset;
        let stride = scanout.stride;
        let fourcc = scanout.fourcc;
        let modifier = scanout.modifier;
        let y0_top = scanout.y0_top;
        let num_planes = scanout.num_planes;

        Self::try_from_raw_parts(
            scanout.into_raw_fds(),
            width,
            height,
            offset,
            stride,
            fourcc,
            modifier,
            y0_top,
            num_planes,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn try_from_raw_parts(
        raw_fds: [i32; 4],
        width: u32,
        height: u32,
        offset: [u32; 4],
        stride: [u32; 4],
        fourcc: u32,
        modifier: u64,
        y0_top: bool,
        num_planes: u32,
    ) -> Result<Self> {
        let plane_count = usize::try_from(num_planes).context("invalid DMABUF plane count")?;
        if plane_count == 0 || plane_count > 4 {
            bail!("DMABUF plane count {} is not supported", num_planes);
        }

        let mut fds = Vec::with_capacity(plane_count);
        for (index, raw_fd) in raw_fds.into_iter().take(plane_count).enumerate() {
            if raw_fd < 0 {
                bail!("DMABUF plane {index} did not provide a valid file descriptor");
            }

            // SAFETY: QEMU passed ownership of the duplicated DMABUF FDs to us.
            fds.push(unsafe { OwnedFd::from_raw_fd(raw_fd) });
        }

        Ok(Self {
            fds,
            width,
            height,
            offset,
            stride,
            fourcc,
            modifier,
            y0_top,
            num_planes,
        })
    }
}

#[derive(Copy, Clone)]
struct FrameRect {
    x: usize,
    y: usize,
    width: usize,
    height: usize,
}

#[cfg(unix)]
struct MappedFramebuffer {
    mmap: ScanoutMmap,
}

struct Framebuffer {
    width: u32,
    height: u32,
    source_stride: usize,
    format: PixelFormat,
    rgba: Vec<u8>,
    #[cfg(unix)]
    mapped: Option<MappedFramebuffer>,
}

impl Framebuffer {
    fn from_scanout(scanout: Scanout) -> Result<Self> {
        let format = PixelFormat::try_from(scanout.format)?;
        let source_stride =
            usize::try_from(scanout.stride).context("invalid framebuffer stride")?;
        let expected_len = usize::try_from(scanout.height)
            .context("invalid framebuffer height")?
            .checked_mul(source_stride)
            .context("framebuffer size overflow")?;

        if scanout.data.len() < expected_len {
            bail!(
                "scanout payload is too short for {}x{} stride {}",
                scanout.width,
                scanout.height,
                scanout.stride
            );
        }

        let rgba_len = usize::try_from(scanout.width)
            .context("invalid framebuffer width")?
            .checked_mul(usize::try_from(scanout.height).context("invalid framebuffer height")?)
            .and_then(|pixels| pixels.checked_mul(RGBA_BYTES_PER_PIXEL))
            .context("RGBA framebuffer size overflow")?;
        let mut framebuffer = Self {
            width: scanout.width,
            height: scanout.height,
            source_stride,
            format,
            rgba: vec![0; rgba_len],
            #[cfg(unix)]
            mapped: None,
        };
        framebuffer.blit_frame_from_linear(&scanout.data)?;
        Ok(framebuffer)
    }

    #[cfg(unix)]
    fn from_map(scanout: ScanoutMap) -> Result<Self> {
        let format = PixelFormat::try_from(scanout.format)?;
        let source_stride =
            usize::try_from(scanout.stride).context("invalid mapped framebuffer stride")?;
        let expected_len = usize::try_from(scanout.height)
            .context("invalid mapped framebuffer height")?
            .checked_mul(source_stride)
            .context("mapped framebuffer size overflow")?;
        let rgba_len = usize::try_from(scanout.width)
            .context("invalid mapped framebuffer width")?
            .checked_mul(
                usize::try_from(scanout.height).context("invalid mapped framebuffer height")?,
            )
            .and_then(|pixels| pixels.checked_mul(RGBA_BYTES_PER_PIXEL))
            .context("mapped RGBA framebuffer size overflow")?;

        let width = scanout.width;
        let height = scanout.height;
        let mmap = scanout
            .mmap()
            .context("failed to map the shared framebuffer")?;

        if mmap.as_ref().len() < expected_len {
            bail!(
                "mapped framebuffer is too short for {}x{} stride {}",
                width,
                height,
                source_stride
            );
        }

        let mut framebuffer = Self {
            width,
            height,
            source_stride,
            format,
            rgba: vec![0; rgba_len],
            mapped: Some(MappedFramebuffer { mmap }),
        };
        framebuffer.refresh_full_mapped_frame()?;
        Ok(framebuffer)
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

        let rect = self.validate_rect(update.x, update.y, update.w, update.h)?;
        let src_stride = usize::try_from(update.stride).context("invalid update stride")?;
        self.blit_update_from_linear(rect, src_stride, &update.data)
    }

    #[cfg(unix)]
    fn refresh_mapped_region(&mut self, update: UpdateMap) -> Result<()> {
        let rect = self.validate_rect(update.x, update.y, update.w, update.h)?;
        let mapped = self
            .mapped
            .as_ref()
            .context("received a shared-memory update without a shared-memory scanout")?;
        let format = self.format;
        let source_stride = self.source_stride;
        let rgba_stride = self.rgba_stride();
        let rgba = &mut self.rgba;

        Self::blit_update_from_frame_data(
            format,
            source_stride,
            rgba_stride,
            rect,
            mapped.mmap.as_ref(),
            rgba,
        )
    }

    #[cfg(unix)]
    fn refresh_full_mapped_frame(&mut self) -> Result<()> {
        let rect = FrameRect {
            x: 0,
            y: 0,
            width: usize::try_from(self.width).context("invalid framebuffer width")?,
            height: usize::try_from(self.height).context("invalid framebuffer height")?,
        };
        let mapped = self
            .mapped
            .as_ref()
            .context("received a shared-memory scanout without a mapping")?;
        let format = self.format;
        let source_stride = self.source_stride;
        let rgba_stride = self.rgba_stride();
        let rgba = &mut self.rgba;

        Self::blit_update_from_frame_data(
            format,
            source_stride,
            rgba_stride,
            rect,
            mapped.mmap.as_ref(),
            rgba,
        )
    }

    fn snapshot(&self) -> FrameSnapshot {
        FrameSnapshot {
            width: self.width,
            height: self.height,
            stride: self.rgba_stride(),
            data: self.rgba.clone(),
        }
    }

    fn rgba_stride(&self) -> usize {
        usize::try_from(self.width).unwrap_or(0) * RGBA_BYTES_PER_PIXEL
    }

    fn validate_rect(&self, x: i32, y: i32, width: i32, height: i32) -> Result<FrameRect> {
        let x = usize::try_from(x).context("negative update x coordinate")?;
        let y = usize::try_from(y).context("negative update y coordinate")?;
        let width = usize::try_from(width).context("negative update width")?;
        let height = usize::try_from(height).context("negative update height")?;
        let framebuffer_width = usize::try_from(self.width).context("invalid framebuffer width")?;
        let framebuffer_height =
            usize::try_from(self.height).context("invalid framebuffer height")?;

        if x.checked_add(width).context("update width overflow")? > framebuffer_width
            || y.checked_add(height).context("update height overflow")? > framebuffer_height
        {
            bail!("update rectangle falls outside the framebuffer bounds");
        }

        Ok(FrameRect {
            x,
            y,
            width,
            height,
        })
    }

    fn blit_frame_from_linear(&mut self, data: &[u8]) -> Result<()> {
        let rect = FrameRect {
            x: 0,
            y: 0,
            width: usize::try_from(self.width).context("invalid framebuffer width")?,
            height: usize::try_from(self.height).context("invalid framebuffer height")?,
        };
        self.blit_update_from_linear(rect, self.source_stride, data)
    }

    fn blit_update_from_linear(
        &mut self,
        rect: FrameRect,
        src_stride: usize,
        data: &[u8],
    ) -> Result<()> {
        let row_len = rect
            .width
            .checked_mul(self.format.bytes_per_pixel())
            .context("update row size overflow")?;

        for row in 0..rect.height {
            let src_start = row
                .checked_mul(src_stride)
                .context("update source offset overflow")?;
            let src_end = src_start
                .checked_add(row_len)
                .context("update source range overflow")?;
            if src_end > data.len() {
                bail!("update payload is too short for the advertised rectangle");
            }

            let dst_start = rect
                .y
                .checked_add(row)
                .context("update destination y overflow")?
                .checked_mul(self.rgba_stride())
                .context("update destination row overflow")?
                .checked_add(
                    rect.x
                        .checked_mul(RGBA_BYTES_PER_PIXEL)
                        .context("update destination x overflow")?,
                )
                .context("update destination offset overflow")?;
            let dst_end = dst_start
                .checked_add(
                    rect.width
                        .checked_mul(RGBA_BYTES_PER_PIXEL)
                        .context("update destination width overflow")?,
                )
                .context("update destination range overflow")?;

            self.format.write_rgba_row(
                &data[src_start..src_end],
                &mut self.rgba[dst_start..dst_end],
            );
        }

        Ok(())
    }

    fn blit_update_from_frame_data(
        format: PixelFormat,
        source_stride: usize,
        rgba_stride: usize,
        rect: FrameRect,
        data: &[u8],
        rgba: &mut [u8],
    ) -> Result<()> {
        let row_len = rect
            .width
            .checked_mul(format.bytes_per_pixel())
            .context("mapped update row size overflow")?;

        for row in 0..rect.height {
            let src_start = rect
                .y
                .checked_add(row)
                .context("mapped update y overflow")?
                .checked_mul(source_stride)
                .context("mapped update row overflow")?
                .checked_add(
                    rect.x
                        .checked_mul(format.bytes_per_pixel())
                        .context("mapped update x overflow")?,
                )
                .context("mapped update source offset overflow")?;
            let src_end = src_start
                .checked_add(row_len)
                .context("mapped update source range overflow")?;
            if src_end > data.len() {
                bail!("mapped update rectangle falls outside the shared framebuffer");
            }

            let dst_start = rect
                .y
                .checked_add(row)
                .context("mapped update destination y overflow")?
                .checked_mul(rgba_stride)
                .context("mapped update destination row overflow")?
                .checked_add(
                    rect.x
                        .checked_mul(RGBA_BYTES_PER_PIXEL)
                        .context("mapped update destination x overflow")?,
                )
                .context("mapped update destination offset overflow")?;
            let dst_end = dst_start
                .checked_add(
                    rect.width
                        .checked_mul(RGBA_BYTES_PER_PIXEL)
                        .context("mapped update destination width overflow")?,
                )
                .context("mapped update destination range overflow")?;

            format.write_rgba_row(&data[src_start..src_end], &mut rgba[dst_start..dst_end]);
        }

        Ok(())
    }
}

struct FrameStreamHandler {
    event_tx: Sender<ViewerEvent>,
    framebuffer: Option<Framebuffer>,
}

impl FrameStreamHandler {
    fn new(event_tx: Sender<ViewerEvent>) -> Self {
        Self {
            event_tx,
            framebuffer: None,
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

    fn scanout(&mut self, scanout: Scanout) {
        match Framebuffer::from_scanout(scanout) {
            Ok(framebuffer) => {
                self.framebuffer = Some(framebuffer);
                self.send_current_frame();
            }
            Err(error) => self.send_status(format!("Unsupported framebuffer: {error:#}")),
        }
    }

    fn update(&mut self, update: Update) {
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
    fn scanout_map(&mut self, scanout: ScanoutMap) {
        match Framebuffer::from_map(scanout) {
            Ok(framebuffer) => {
                self.framebuffer = Some(framebuffer);
                self.send_current_frame();
            }
            Err(error) => {
                self.send_status(format!("Could not map the shared framebuffer: {error:#}"))
            }
        }
    }

    #[cfg(unix)]
    fn update_map(&mut self, update: UpdateMap) {
        let Some(framebuffer) = &mut self.framebuffer else {
            return;
        };

        match framebuffer.refresh_mapped_region(update) {
            Ok(()) => self.send_current_frame(),
            Err(error) => self.send_status(format!(
                "Failed to refresh the shared framebuffer update: {error:#}"
            )),
        }
    }

    #[cfg(unix)]
    fn scanout_dmabuf(&mut self, scanout: ScanoutDMABUF) {
        match DmabufFrame::try_from_scanout(scanout) {
            Ok(scanout) => self.emit_dmabuf_scanout(scanout),
            Err(error) => self.send_status(format!("Unsupported DMABUF scanout: {error:#}")),
        }
    }

    #[cfg(unix)]
    fn emit_dmabuf_scanout(&mut self, scanout: DmabufFrame) {
        self.framebuffer = None;
        let _ = self.event_tx.send(ViewerEvent::Dmabuf(scanout));
    }

    #[cfg(unix)]
    fn update_dmabuf(&mut self, update: UpdateDMABUF) {
        let _ = self.event_tx.send(ViewerEvent::DmabufUpdate(update));
    }

    fn disable(&mut self) {
        self.framebuffer = None;
        self.send_status("The guest display was disabled.");
    }

    fn mouse_set(&mut self, _set: MouseSet) {}

    fn cursor_define(&mut self, _cursor: Cursor) {}

    fn disconnected(&mut self) {
        let _ = self.event_tx.send(ViewerEvent::Disconnected);
    }

    fn interfaces(&self) -> Vec<String> {
        #[cfg(unix)]
        {
            return vec![
                "org.qemu.Display1.Listener.Unix.Map".to_owned(),
                "org.qemu.Display1.Listener.Unix.ScanoutDMABUF2".to_owned(),
            ];
        }

        #[cfg(not(unix))]
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

    fn write_rgba_row(self, src_row: &[u8], dst_row: &mut [u8]) {
        for (src, dst) in src_row
            .chunks_exact(self.bytes_per_pixel())
            .zip(dst_row.chunks_exact_mut(RGBA_BYTES_PER_PIXEL))
        {
            dst.copy_from_slice(&self.pixel_to_rgba(src));
        }
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
            match DmabufFrame::try_from_raw_parts(
                fds, width, height, offsets, strides, fourcc, modifier, y0_top, num_planes,
            ) {
                Ok(scanout) => handler.emit_dmabuf_scanout(scanout),
                Err(error) => handler.send_status(format!("Unsupported DMABUF scanout: {error:#}")),
            }
        });

        Ok(())
    }
}

fn install_input_controllers(
    picture: &gtk::Picture,
    ui_state: Rc<RefCell<UiState>>,
    input_tx: tokio_mpsc::UnboundedSender<InputEvent>,
    mouse_mode: MouseMode,
    keyboard_available: bool,
) {
    if keyboard_available {
        let key_controller = gtk::EventControllerKey::new();
        key_controller.connect_key_pressed({
            let input_tx = input_tx.clone();
            move |_, _, keycode, _| match gdk_keycode_to_qnum(keycode) {
                Some(qnum) => {
                    let _ = input_tx.send(InputEvent::KeyPress(qnum));
                    glib::Propagation::Stop
                }
                None => glib::Propagation::Proceed,
            }
        });
        key_controller.connect_key_released({
            let input_tx = input_tx.clone();
            move |_, _, keycode, _| {
                if let Some(qnum) = gdk_keycode_to_qnum(keycode) {
                    let _ = input_tx.send(InputEvent::KeyRelease(qnum));
                }
            }
        });
        picture.add_controller(key_controller);
    }

    if mouse_mode == MouseMode::Disabled {
        return;
    }

    let click = gtk::GestureClick::new();
    click.set_button(0);
    click.connect_pressed({
        let picture = picture.clone();
        let ui_state = ui_state.clone();
        let input_tx = input_tx.clone();
        move |gesture, _, x, y| {
            picture.grab_focus();
            sync_mouse_position(&picture, &ui_state, &input_tx, mouse_mode, x, y);

            if let Some(button) = gtk_button_to_qemu(gesture.current_button()) {
                let _ = input_tx.send(InputEvent::MousePress(button));
            }
        }
    });
    click.connect_released({
        let picture = picture.clone();
        let ui_state = ui_state.clone();
        let input_tx = input_tx.clone();
        move |gesture, _, x, y| {
            sync_mouse_position(&picture, &ui_state, &input_tx, mouse_mode, x, y);

            if let Some(button) = gtk_button_to_qemu(gesture.current_button()) {
                let _ = input_tx.send(InputEvent::MouseRelease(button));
            }
        }
    });
    picture.add_controller(click);

    let motion = gtk::EventControllerMotion::new();
    motion.connect_enter({
        let picture = picture.clone();
        let ui_state = ui_state.clone();
        let input_tx = input_tx.clone();
        move |_, x, y| sync_mouse_position(&picture, &ui_state, &input_tx, mouse_mode, x, y)
    });
    motion.connect_motion({
        let picture = picture.clone();
        let ui_state = ui_state.clone();
        let input_tx = input_tx.clone();
        move |_, x, y| sync_mouse_position(&picture, &ui_state, &input_tx, mouse_mode, x, y)
    });
    motion.connect_leave({
        let ui_state = ui_state.clone();
        move |_| {
            ui_state.borrow_mut().last_pointer_guest_position = None;
        }
    });
    picture.add_controller(motion);

    let scroll = gtk::EventControllerScroll::new(
        gtk::EventControllerScrollFlags::BOTH_AXES | gtk::EventControllerScrollFlags::DISCRETE,
    );
    scroll.connect_scroll({
        let input_tx = input_tx.clone();
        move |_, dx, dy| {
            let handled = emit_scroll_buttons(&input_tx, dx, dy);
            if handled {
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        }
    });
    picture.add_controller(scroll);
}

fn sync_mouse_position(
    picture: &gtk::Picture,
    ui_state: &Rc<RefCell<UiState>>,
    input_tx: &tokio_mpsc::UnboundedSender<InputEvent>,
    mouse_mode: MouseMode,
    x: f64,
    y: f64,
) {
    let frame_size = ui_state.borrow().frame_size;
    let Some((frame_width, frame_height)) = frame_size else {
        return;
    };
    let Some((guest_x, guest_y)) = widget_coords_to_guest_position(
        picture.width(),
        picture.height(),
        frame_width,
        frame_height,
        x,
        y,
    ) else {
        ui_state.borrow_mut().last_pointer_guest_position = None;
        return;
    };

    let mut ui_state = ui_state.borrow_mut();
    match mouse_mode {
        MouseMode::Disabled => {}
        MouseMode::Absolute => {
            let _ = input_tx.send(InputEvent::MouseAbs {
                x: guest_x,
                y: guest_y,
            });
            ui_state.last_pointer_guest_position = Some((guest_x, guest_y));
        }
        MouseMode::Relative => {
            if let Some((prev_x, prev_y)) = ui_state.last_pointer_guest_position {
                let dx = guest_x as i32 - prev_x as i32;
                let dy = guest_y as i32 - prev_y as i32;
                if dx != 0 || dy != 0 {
                    let _ = input_tx.send(InputEvent::MouseRel { dx, dy });
                }
            }
            ui_state.last_pointer_guest_position = Some((guest_x, guest_y));
        }
    }
}

fn emit_scroll_buttons(
    input_tx: &tokio_mpsc::UnboundedSender<InputEvent>,
    _dx: f64,
    dy: f64,
) -> bool {
    let mut handled = false;

    for _ in 0..scroll_ticks(dy).unsigned_abs() {
        let button = if dy.is_sign_positive() {
            MouseButton::WheelDown
        } else {
            MouseButton::WheelUp
        };
        let _ = input_tx.send(InputEvent::MouseWheel(button));
        handled = true;
    }

    handled
}

fn scroll_ticks(delta: f64) -> i32 {
    if delta.abs() < f64::EPSILON {
        0
    } else {
        let rounded = delta.round() as i32;
        if rounded == 0 {
            delta.signum() as i32
        } else {
            rounded
        }
    }
}

fn gtk_button_to_qemu(button: u32) -> Option<MouseButton> {
    match button {
        1 => Some(MouseButton::Left),
        2 => Some(MouseButton::Middle),
        3 => Some(MouseButton::Right),
        8 => Some(MouseButton::Side),
        9 => Some(MouseButton::Extra),
        _ => None,
    }
}

fn widget_coords_to_guest_position(
    widget_width: i32,
    widget_height: i32,
    frame_width: u32,
    frame_height: u32,
    x: f64,
    y: f64,
) -> Option<(u32, u32)> {
    if widget_width <= 0 || widget_height <= 0 || frame_width == 0 || frame_height == 0 {
        return None;
    }

    let widget_width = f64::from(widget_width);
    let widget_height = f64::from(widget_height);
    let frame_width_f = f64::from(frame_width);
    let frame_height_f = f64::from(frame_height);
    let scale = (widget_width / frame_width_f).min(widget_height / frame_height_f);
    if !scale.is_finite() || scale <= 0.0 {
        return None;
    }

    let display_width = frame_width_f * scale;
    let display_height = frame_height_f * scale;
    let x_offset = (widget_width - display_width) / 2.0;
    let y_offset = (widget_height - display_height) / 2.0;
    let local_x = x - x_offset;
    let local_y = y - y_offset;
    if local_x < 0.0 || local_y < 0.0 || local_x > display_width || local_y > display_height {
        return None;
    }

    let guest_x = (local_x / scale)
        .floor()
        .clamp(0.0, f64::from(frame_width.saturating_sub(1))) as u32;
    let guest_y = (local_y / scale)
        .floor()
        .clamp(0.0, f64::from(frame_height.saturating_sub(1))) as u32;

    Some((guest_x, guest_y))
}

fn gdk_keycode_to_qnum(keycode: u32) -> Option<u32> {
    keycode.checked_sub(8).and_then(linux_keycode_to_qnum)
}

fn linux_keycode_to_qnum(linux_keycode: u32) -> Option<u32> {
    let qnum = match linux_keycode {
        1..=83 => linux_keycode,
        85 => 118,
        86..=88 => linux_keycode,
        89 => 115,
        90 => 120,
        91 => 119,
        92 => 121,
        93 => 112,
        94 => 123,
        95 => 92,
        96 => 156,
        97 => 157,
        98 => 181,
        99 => 183,
        100 => 184,
        101 => 91,
        102 => 199,
        103 => 200,
        104 => 201,
        105 => 203,
        106 => 205,
        107 => 207,
        108 => 208,
        109 => 209,
        110 => 210,
        111 => 211,
        112 => 239,
        113 => 160,
        114 => 174,
        115 => 176,
        116 => 222,
        117 => 89,
        118 => 206,
        119 => 198,
        120 => 139,
        121 => 126,
        122 => 114,
        123 => 113,
        124 => 125,
        125 => 219,
        126 => 220,
        127 => 221,
        128 => 232,
        129 => 133,
        130 => 134,
        131 => 135,
        132 => 140,
        133 => 248,
        134 => 100,
        135 => 101,
        136 => 193,
        137 => 188,
        138 => 245,
        139 => 158,
        140 => 161,
        141 => 102,
        142 => 223,
        143 => 227,
        144 => 103,
        145 => 104,
        146 => 105,
        147 => 147,
        148 => 159,
        149 => 151,
        150 => 130,
        151 => 106,
        152 => 146,
        153 => 107,
        154 => 166,
        155 => 236,
        156 => 230,
        157 => 235,
        158 => 234,
        159 => 233,
        160 => 163,
        161 => 108,
        162 => 253,
        163 => 153,
        164 => 162,
        165 => 144,
        166 => 164,
        167 => 177,
        168 => 152,
        169 => 99,
        171 => 129,
        172 => 178,
        173 => 231,
        176 => 136,
        177 => 117,
        178 => 143,
        179 => 246,
        180 => 251,
        181 => 137,
        182 => 138,
        183 => 93,
        184 => 94,
        185 => 95,
        186 => 85,
        187 => 131,
        188 => 247,
        189 => 132,
        190 => 90,
        191 => 116,
        192 => 249,
        193 => 109,
        194 => 111,
        200 => 168,
        201 => 169,
        202 => 171,
        203 => 172,
        204 => 173,
        205 => 165,
        206 => 175,
        207 => 179,
        208 => 180,
        209 => 182,
        210 => 185,
        211 => 186,
        212 => 187,
        213 => 189,
        214 => 190,
        215 => 191,
        216 => 192,
        217 => 229,
        218 => 194,
        219 => 195,
        220 => 196,
        221 => 197,
        222 => 148,
        223 => 202,
        224 => 204,
        225 => 212,
        226 => 237,
        227 => 214,
        228 => 215,
        229 => 216,
        230 => 217,
        231 => 218,
        232 => 228,
        233 => 142,
        234 => 213,
        235 => 240,
        236 => 241,
        237 => 242,
        238 => 243,
        239 => 244,
        _ => return None,
    };

    Some(qnum)
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
    use super::{
        Framebuffer, PixelFormat, dmabuf_update_rectangle, linux_keycode_to_qnum,
        widget_coords_to_guest_position,
    };
    use qemu_display::{Scanout, UpdateDMABUF};

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

    #[test]
    fn extended_linux_keycodes_are_translated_to_qnum() {
        assert_eq!(linux_keycode_to_qnum(97), Some(157));
        assert_eq!(linux_keycode_to_qnum(103), Some(200));
        assert_eq!(linux_keycode_to_qnum(125), Some(219));
    }

    #[test]
    fn widget_coordinates_account_for_letterboxing() {
        assert_eq!(
            widget_coords_to_guest_position(800, 600, 640, 480, 400.0, 300.0),
            Some((320, 240))
        );
        assert_eq!(
            widget_coords_to_guest_position(800, 600, 640, 360, 400.0, 74.0),
            None
        );
        assert_eq!(
            widget_coords_to_guest_position(800, 600, 640, 360, 400.0, 300.0),
            Some((320, 180))
        );
    }

    #[test]
    #[cfg(unix)]
    fn dmabuf_update_rectangles_are_clipped_to_the_frame() {
        let rect = dmabuf_update_rectangle(
            UpdateDMABUF {
                x: -8,
                y: 4,
                w: 20,
                h: 12,
            },
            16,
            10,
        )
        .expect("partially visible DMABUF updates should be clipped");

        assert_eq!(rect.x(), 0);
        assert_eq!(rect.y(), 4);
        assert_eq!(rect.width(), 12);
        assert_eq!(rect.height(), 6);

        assert!(
            dmabuf_update_rectangle(
                UpdateDMABUF {
                    x: 40,
                    y: 0,
                    w: 5,
                    h: 5,
                },
                16,
                10,
            )
            .is_none(),
            "fully out-of-bounds DMABUF updates should be ignored",
        );
    }
}
