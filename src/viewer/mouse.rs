use std::{cell::RefCell, rc::Rc};

use gtk::{gdk, glib, prelude::*};
use gtk4 as gtk;
use qemu_display::MouseButton;
use tokio::sync::mpsc as tokio_mpsc;

use super::{InputEvent, UiState, grab};

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(super) enum MouseMode {
    Disabled,
    Relative,
    Absolute,
}

impl MouseMode {
    pub(super) fn from_is_absolute(is_absolute: bool) -> Self {
        if is_absolute {
            Self::Absolute
        } else {
            Self::Relative
        }
    }
}

pub(super) fn install_mouse_controllers(
    picture: &gtk::Picture,
    ui_state: Rc<RefCell<UiState>>,
    input_tx: tokio_mpsc::UnboundedSender<InputEvent>,
    mouse_mode: Rc<RefCell<MouseMode>>,
    input_grab: grab::SharedInputGrab,
    activate_grab: impl Fn(Option<gdk::Event>) + 'static,
) {
    if *mouse_mode.borrow() == MouseMode::Disabled {
        return;
    }

    let click = gtk::GestureClick::new();
    click.set_button(0);
    click.connect_pressed({
        let picture = picture.clone();
        let ui_state = ui_state.clone();
        let input_tx = input_tx.clone();
        let mouse_mode = mouse_mode.clone();
        let input_grab = input_grab.clone();
        move |gesture, _, x, y| {
            if !grab::is_active(&input_grab) {
                activate_grab(gesture.current_event());
            }
            sync_mouse_position(&picture, &ui_state, &input_tx, &mouse_mode, x, y);

            if let Some(button) = gtk_button_to_qemu(gesture.current_button()) {
                let _ = input_tx.send(InputEvent::MousePress(button));
            }
        }
    });
    click.connect_released({
        let picture = picture.clone();
        let ui_state = ui_state.clone();
        let input_tx = input_tx.clone();
        let mouse_mode = mouse_mode.clone();
        let input_grab = input_grab.clone();
        move |gesture, _, x, y| {
            if !grab::is_active(&input_grab) {
                return;
            }
            sync_mouse_position(&picture, &ui_state, &input_tx, &mouse_mode, x, y);

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
        let mouse_mode = mouse_mode.clone();
        let input_grab = input_grab.clone();
        move |_, x, y| {
            if !grab::is_active(&input_grab) {
                return;
            }
            sync_mouse_position(&picture, &ui_state, &input_tx, &mouse_mode, x, y)
        }
    });
    motion.connect_motion({
        let picture = picture.clone();
        let ui_state = ui_state.clone();
        let input_tx = input_tx.clone();
        let mouse_mode = mouse_mode.clone();
        let input_grab = input_grab.clone();
        move |_, x, y| {
            if !grab::is_active(&input_grab) {
                return;
            }
            sync_mouse_position(&picture, &ui_state, &input_tx, &mouse_mode, x, y)
        }
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
        let input_grab = input_grab.clone();
        move |_, dx, dy| {
            if !grab::is_active(&input_grab) {
                return glib::Propagation::Proceed;
            }
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

pub(super) fn input_needs_mouse_mode(input: &InputEvent) -> bool {
    matches!(
        input,
        InputEvent::MouseAbs { .. } | InputEvent::MouseRel { .. }
    )
}

fn sync_mouse_position(
    picture: &gtk::Picture,
    ui_state: &Rc<RefCell<UiState>>,
    input_tx: &tokio_mpsc::UnboundedSender<InputEvent>,
    mouse_mode: &Rc<RefCell<MouseMode>>,
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
    match *mouse_mode.borrow() {
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

/// Convert the pointer coordinates from the GTK widget into guest coordinates,
/// compensating for the letterboxing introduced by `ContentFit::Contain`.
pub(super) fn widget_coords_to_guest_position(
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
