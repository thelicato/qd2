mod chrome;
mod dmabuf;
mod framebuffer;
mod keyboard;
mod listener;
mod mouse;
mod utils;

use std::{
    cell::RefCell,
    convert::TryFrom,
    rc::Rc,
    sync::mpsc::{self as std_mpsc, Receiver, TryRecvError},
    thread,
    time::Duration,
};

use anyhow::{Context, Result};
use gtk::{gdk, glib, prelude::*};
use gtk4 as gtk;
use qemu_display::{MouseButton, UpdateDMABUF};
use tokio::sync::{mpsc as tokio_mpsc, oneshot};

use self::mouse::MouseMode;
use crate::qemu::ConnectTarget;

const FRAME_POLL_INTERVAL: Duration = Duration::from_millis(16);

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
            move || listener::run_listener_thread(target, event_tx, ready_tx, input_rx, shutdown_rx)
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
    let current_dmabuf = Rc::new(RefCell::new(None::<dmabuf::DmabufPresentation>));
    #[cfg(unix)]
    let dmabuf_transform = Rc::new(RefCell::new(dmabuf::DmabufViewTransform::default()));
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
                        if let Err(error) = dmabuf::present_dmabuf_paintable(
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
                        match dmabuf::build_dmabuf_presentation(&display, scanout) {
                            Ok(presentation) => {
                                let transform = *dmabuf_transform.borrow();
                                match dmabuf::present_dmabuf_paintable(
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
                            Ok(()) => dmabuf::present_dmabuf_paintable(
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
    Frame(framebuffer::FrameSnapshot),
    #[cfg(unix)]
    Dmabuf(dmabuf::DmabufFrame),
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
    Frame(framebuffer::FrameSnapshot),
    #[cfg(unix)]
    Dmabuf(dmabuf::DmabufFrame),
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

#[cfg(test)]
mod tests {
    use super::{
        InputEvent,
        dmabuf::dmabuf_update_rectangle,
        framebuffer::{Framebuffer, PixelFormat},
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
