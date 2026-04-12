use std::{cell::RefCell, rc::Rc};

use gtk::{gdk, prelude::*};
use gtk4 as gtk;

use super::{cursor::CursorState, mouse::MouseMode};

pub(super) type SharedInputGrab = Rc<RefCell<InputGrabState>>;

#[derive(Default)]
pub(super) struct InputGrabState {
    active: bool,
}

pub(super) fn new_state() -> SharedInputGrab {
    Rc::new(RefCell::new(InputGrabState::default()))
}

pub(super) fn is_active(state: &SharedInputGrab) -> bool {
    state.borrow().active
}

/// Enter guest-grab mode after an explicit click so the compositor lets the
/// viewer inhibit host shortcuts such as the Super key.
pub(super) fn activate(
    window: &gtk::Window,
    picture: &gtk::Picture,
    state: &SharedInputGrab,
    event: Option<&gdk::Event>,
) -> bool {
    picture.grab_focus();

    let changed = {
        let mut state = state.borrow_mut();
        if state.active {
            false
        } else {
            state.active = true;
            true
        }
    };

    if changed {
        if let Some(toplevel) = window_toplevel(window) {
            toplevel.inhibit_system_shortcuts(event);
        }
    }

    changed
}

pub(super) fn release(window: &gtk::Window, state: &SharedInputGrab) -> bool {
    let changed = {
        let mut state = state.borrow_mut();
        if !state.active {
            false
        } else {
            state.active = false;
            true
        }
    };

    if changed {
        if let Some(toplevel) = window_toplevel(window) {
            toplevel.restore_system_shortcuts();
        }
    }

    changed
}

pub(super) fn sync_cursor_capture(
    picture: &gtk::Picture,
    cursor_state: &Rc<RefCell<CursorState>>,
    input_grab: &SharedInputGrab,
    mouse_mode: &Rc<RefCell<MouseMode>>,
) {
    let capture_hidden = is_active(input_grab) && *mouse_mode.borrow() == MouseMode::Relative;
    let mut cursor_state = cursor_state.borrow_mut();
    cursor_state.set_capture_hidden(capture_hidden);
    cursor_state.apply_to_widget(picture);
}

fn window_toplevel(window: &gtk::Window) -> Option<gdk::Toplevel> {
    window.surface()?.dynamic_cast::<gdk::Toplevel>().ok()
}
