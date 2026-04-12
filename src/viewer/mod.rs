mod chrome;
mod keyboard;
mod mouse;
mod utils;

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

use self::mouse::MouseMode;
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

/// Start the GTK viewer and the background listener that mirrors the QEMU display stream.
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
        mouse_mode,
    }));

    loop {
        tokio::select! {
            _ = &mut shutdown_rx => break,
            maybe_input = input_rx.recv() => match maybe_input {
                Some(input) => {
                    if let Err(error) = console.handle_input(input).await {
                        let recovered = if mouse::input_needs_mouse_mode(input) {
                            match console.mouse_is_absolute().await {
                                Ok(is_absolute) => {
                                    let detected_mode = MouseMode::from_is_absolute(is_absolute);
                                    if detected_mode != mouse_mode {
                                        mouse_mode = detected_mode;
                                        let _ =
                                            event_tx.send(ViewerEvent::MouseModeChanged(detected_mode));
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

/// Build the GTK window and keep it in sync with the latest framebuffer or
/// DMABUF presentation coming from the listener thread.
fn run_window(
    target: &ConnectTarget,
    ready: &ViewerReady,
    event_rx: Receiver<ViewerEvent>,
    input_tx: tokio_mpsc::UnboundedSender<InputEvent>,
) -> Result<()> {
    gtk::init().context("failed to initialize GTK4")?;

    let main_loop = glib::MainLoop::new(None, false);
    let (window_width, window_height) = utils::suggested_window_size(ready.width, ready.height);
    let app_icon = utils::load_app_icon().ok();

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
    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(&container));

    let window = gtk::Window::builder()
        .title(&ready.title)
        .default_width(window_width)
        .default_height(window_height)
        .child(&overlay)
        .build();
    window.set_resizable(true);
    if let Some(icon) = app_icon.clone() {
        window.connect_realize(move |window| {
            if let Err(error) = utils::apply_window_icon(window, &icon) {
                eprintln!("QD2 icon error: {error:#}");
            }
        });
    }

    let display = gtk::prelude::RootExt::display(&window);
    chrome::install_viewer_css(&display);

    let title_label = gtk::Label::new(Some(&ready.title));
    title_label.add_css_class("viewer-title");

    let (titlebar_controls, titlebar_fullscreen_button) =
        chrome::build_viewer_controls(&window, app_icon.as_ref());
    let header_bar = gtk::HeaderBar::new();
    header_bar.set_show_title_buttons(true);
    header_bar.set_title_widget(Some(&title_label));
    header_bar.pack_end(&titlebar_controls);
    window.set_titlebar(Some(&header_bar));

    let (floating_controls, overlay_fullscreen_button) =
        chrome::build_viewer_controls(&window, app_icon.as_ref());
    floating_controls.add_css_class("viewer-floating-controls");

    let fullscreen_revealer = gtk::Revealer::builder()
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Start)
        .margin_top(12)
        .transition_duration(180)
        .transition_type(gtk::RevealerTransitionType::SlideDown)
        .build();
    fullscreen_revealer.set_child(Some(&floating_controls));
    fullscreen_revealer.set_visible(false);
    overlay.add_overlay(&fullscreen_revealer);
    overlay.set_measure_overlay(&fullscreen_revealer, false);
    overlay.set_clip_overlay(&fullscreen_revealer, false);

    let fullscreen_hotspot = gtk::Box::builder()
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Start)
        .width_request(240)
        .height_request(24)
        .build();
    fullscreen_hotspot.set_opacity(0.0);
    fullscreen_hotspot.set_visible(false);
    overlay.add_overlay(&fullscreen_hotspot);
    overlay.set_measure_overlay(&fullscreen_hotspot, false);
    overlay.set_clip_overlay(&fullscreen_hotspot, false);

    let fullscreen_state = Rc::new(RefCell::new(chrome::FullscreenChromeState::default()));
    let titlebar_widget = header_bar.clone().upcast::<gtk::Widget>();
    let fullscreen_buttons = vec![
        titlebar_fullscreen_button.clone(),
        overlay_fullscreen_button.clone(),
    ];
    chrome::sync_fullscreen_chrome(
        &window,
        &titlebar_widget,
        &fullscreen_revealer,
        &fullscreen_hotspot,
        &fullscreen_buttons,
        &fullscreen_state,
    );
    window.connect_fullscreened_notify({
        let header_bar = titlebar_widget.clone();
        let fullscreen_revealer = fullscreen_revealer.clone();
        let fullscreen_hotspot = fullscreen_hotspot.clone();
        let fullscreen_buttons = fullscreen_buttons.clone();
        let fullscreen_state = fullscreen_state.clone();
        move |window| {
            chrome::sync_fullscreen_chrome(
                window,
                &header_bar,
                &fullscreen_revealer,
                &fullscreen_hotspot,
                &fullscreen_buttons,
                &fullscreen_state,
            );
        }
    });

    let hotspot_motion = gtk::EventControllerMotion::new();
    hotspot_motion.connect_enter({
        let window = window.clone();
        let fullscreen_revealer = fullscreen_revealer.clone();
        let fullscreen_state = fullscreen_state.clone();
        move |_, _, _| {
            chrome::reveal_fullscreen_bar(&fullscreen_revealer, &fullscreen_state);
            chrome::schedule_hide_fullscreen_bar(&window, &fullscreen_revealer, &fullscreen_state);
        }
    });
    hotspot_motion.connect_leave({
        let window = window.clone();
        let fullscreen_revealer = fullscreen_revealer.clone();
        let fullscreen_state = fullscreen_state.clone();
        move |_| {
            chrome::schedule_hide_fullscreen_bar(&window, &fullscreen_revealer, &fullscreen_state)
        }
    });
    fullscreen_hotspot.add_controller(hotspot_motion);

    let floating_motion = gtk::EventControllerMotion::new();
    floating_motion.connect_enter({
        let fullscreen_revealer = fullscreen_revealer.clone();
        let fullscreen_state = fullscreen_state.clone();
        move |_, _, _| chrome::reveal_fullscreen_bar(&fullscreen_revealer, &fullscreen_state)
    });
    floating_motion.connect_leave({
        let window = window.clone();
        let fullscreen_revealer = fullscreen_revealer.clone();
        let fullscreen_state = fullscreen_state.clone();
        move |_| {
            chrome::schedule_hide_fullscreen_bar(&window, &fullscreen_revealer, &fullscreen_state)
        }
    });
    floating_controls.add_controller(floating_motion);

    let viewer_shortcuts = gtk::EventControllerKey::new();
    viewer_shortcuts.set_propagation_phase(gtk::PropagationPhase::Capture);
    viewer_shortcuts.connect_key_pressed({
        let window = window.clone();
        move |_, keyval, _, _| match keyval {
            gdk::Key::F11 => {
                chrome::toggle_fullscreen(&window);
                glib::Propagation::Stop
            }
            gdk::Key::Escape if window.is_fullscreen() => {
                window.unfullscreen();
                glib::Propagation::Stop
            }
            _ => glib::Propagation::Proceed,
        }
    });
    picture.add_controller(viewer_shortcuts);

    let ui_state = Rc::new(RefCell::new(UiState::default()));
    let mouse_mode = Rc::new(RefCell::new(ready.mouse_mode));
    if ready.keyboard_available {
        keyboard::install_keyboard_controller(&picture, input_tx.clone());
    }
    mouse::install_mouse_controllers(&picture, ui_state.clone(), input_tx, mouse_mode.clone());

    let event_rx = Rc::new(RefCell::new(event_rx));
    #[cfg(unix)]
    let current_dmabuf = Rc::new(RefCell::new(None::<DmabufPresentation>));
    #[cfg(unix)]
    let dmabuf_transform = Rc::new(RefCell::new(DmabufViewTransform::default()));
    let window_base_title = ready.title.clone();
    #[cfg(unix)]
    {
        let shortcuts = gtk::EventControllerKey::new();
        shortcuts.set_propagation_phase(gtk::PropagationPhase::Capture);
        shortcuts.connect_key_pressed({
            let current_dmabuf = current_dmabuf.clone();
            let dmabuf_transform = dmabuf_transform.clone();
            let picture = picture.clone();
            let status_label = status_label.clone();
            let ui_state = ui_state.clone();
            let window = window.clone();
            let window_base_title = window_base_title.clone();
            move |_, keyval, _, state| {
                let ctrl_alt = state.contains(gdk::ModifierType::CONTROL_MASK)
                    && state.contains(gdk::ModifierType::ALT_MASK);
                if !ctrl_alt {
                    return glib::Propagation::Proceed;
                }

                {
                    let mut transform = dmabuf_transform.borrow_mut();
                    match keyval {
                        gdk::Key::R | gdk::Key::r => transform.rotate_clockwise(),
                        gdk::Key::F | gdk::Key::f => transform.toggle_vertical_flip(),
                        gdk::Key::_0 => transform.reset(),
                        _ => return glib::Propagation::Proceed,
                    }

                    if let Some(presentation) = current_dmabuf.borrow().as_ref() {
                        if let Err(error) = present_dmabuf_paintable(
                            &picture,
                            &status_label,
                            &ui_state,
                            &window,
                            &window_base_title,
                            presentation,
                            *transform,
                        ) {
                            eprintln!("QD2 DMABUF transform error: {error:#}");
                        }
                    }

                    eprintln!("QD2 {}", transform.describe());
                }

                glib::Propagation::Stop
            }
        });
        picture.add_controller(shortcuts);
    }
    glib::timeout_add_local(FRAME_POLL_INTERVAL, {
        let event_rx = event_rx.clone();
        #[cfg(unix)]
        let current_dmabuf = current_dmabuf.clone();
        #[cfg(unix)]
        let dmabuf_transform = dmabuf_transform.clone();
        let display = display.clone();
        let picture = picture.clone();
        let status_label = status_label.clone();
        let ui_state = ui_state.clone();
        let mouse_mode = mouse_mode.clone();
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
                    Ok(ViewerEvent::MouseModeChanged(mode)) => {
                        *mouse_mode.borrow_mut() = mode;
                        ui_state.borrow_mut().last_pointer_guest_position = None;
                    }
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
                            Ok(presentation) => {
                                let transform = *dmabuf_transform.borrow();
                                match present_dmabuf_paintable(
                                    &picture,
                                    &status_label,
                                    &ui_state,
                                    &window,
                                    &window_base_title,
                                    &presentation,
                                    transform,
                                ) {
                                    Ok(()) => {
                                        *current_dmabuf.borrow_mut() = Some(presentation);
                                    }
                                    Err(error) => {
                                        latest_status = Some(format!(
                                            "Could not prepare the DMABUF scanout for display: {error:#}"
                                        ));
                                    }
                                }
                            }
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
                    let transform = *dmabuf_transform.borrow();
                    match current_dmabuf.as_mut() {
                        Some(presentation) => match presentation.refresh(&display, &dmabuf_updates)
                        {
                            Ok(()) => present_dmabuf_paintable(
                                &picture,
                                &status_label,
                                &ui_state,
                                &window,
                                &window_base_title,
                                presentation,
                                transform,
                            )
                            .map(Some),
                            Err(error) => Err(error),
                        },
                        None => Ok(None),
                    }
                };

                match refreshed {
                    Ok(Some(())) => {}
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
#[derive(Copy, Clone, Debug)]
struct DmabufViewTransform {
    rotation_quarters: u8,
    extra_vertical_flip: bool,
}

#[cfg(unix)]
impl Default for DmabufViewTransform {
    fn default() -> Self {
        // Start with a rotated + flipped-friendly orientation for the current
        // Linux/GTK DMABUF path; the runtime shortcuts can still override it.
        Self {
            rotation_quarters: 0,
            extra_vertical_flip: true,
        }
    }
}

#[cfg(unix)]
impl DmabufViewTransform {
    fn rotate_clockwise(&mut self) {
        self.rotation_quarters = (self.rotation_quarters + 1) % 4;
    }

    fn toggle_vertical_flip(&mut self) {
        self.extra_vertical_flip = !self.extra_vertical_flip;
    }

    fn reset(&mut self) {
        *self = Self::default();
    }

    fn describe(self) -> String {
        format!(
            "DMABUF transform: rotate={} extra-flip-y={}",
            self.rotation_quarters * 90,
            if self.extra_vertical_flip {
                "on"
            } else {
                "off"
            }
        )
    }
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

    fn to_paintable(&self, transform: DmabufViewTransform) -> Result<gdk::Paintable> {
        let needs_vertical_flip = !self.y0_top ^ transform.extra_vertical_flip;
        if !needs_vertical_flip && transform.rotation_quarters == 0 {
            return Ok(self.texture.clone().upcast());
        }

        let snapshot = gtk::Snapshot::new();
        let bounds = gtk::graphene::Rect::new(0.0, 0.0, self.width as f32, self.height as f32);

        snapshot.save();

        match transform.rotation_quarters % 4 {
            0 => {}
            1 => {
                snapshot.translate(&gtk::graphene::Point::new(self.height as f32, 0.0));
                snapshot.rotate(90.0);
            }
            2 => {
                snapshot.translate(&gtk::graphene::Point::new(
                    self.width as f32,
                    self.height as f32,
                ));
                snapshot.rotate(180.0);
            }
            3 => {
                snapshot.translate(&gtk::graphene::Point::new(0.0, self.width as f32));
                snapshot.rotate(270.0);
            }
            _ => unreachable!(),
        }

        if needs_vertical_flip {
            snapshot.translate(&gtk::graphene::Point::new(0.0, self.height as f32));
            snapshot.scale(1.0, -1.0);
        }

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

#[cfg(unix)]
fn present_dmabuf_paintable(
    picture: &gtk::Picture,
    status_label: &gtk::Label,
    ui_state: &Rc<RefCell<UiState>>,
    window: &gtk::Window,
    window_base_title: &str,
    presentation: &DmabufPresentation,
    transform: DmabufViewTransform,
) -> Result<()> {
    let paintable = presentation.to_paintable(transform)?;
    present_paintable(
        picture,
        status_label,
        ui_state,
        window,
        window_base_title,
        &paintable,
        paintable.intrinsic_width().max(0) as u32,
        paintable.intrinsic_height().max(0) as u32,
    );
    Ok(())
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
    MouseModeChanged(MouseMode),
    Status(String),
    Disconnected,
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

#[cfg(test)]
mod tests {
    use super::{
        Framebuffer, InputEvent, PixelFormat, dmabuf_update_rectangle,
        keyboard::linux_keycode_to_qnum,
        mouse::{input_needs_mouse_mode, widget_coords_to_guest_position},
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
    fn mouse_mode_is_only_rechecked_for_motion_inputs() {
        assert!(input_needs_mouse_mode(InputEvent::MouseAbs { x: 1, y: 2 }));
        assert!(input_needs_mouse_mode(InputEvent::MouseRel {
            dx: 3,
            dy: -4
        }));
        assert!(!input_needs_mouse_mode(InputEvent::MousePress(
            qemu_display::MouseButton::Left
        )));
        assert!(!input_needs_mouse_mode(InputEvent::KeyPress(42)));
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
