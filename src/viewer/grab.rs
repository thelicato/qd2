use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::Duration,
};

use gtk::{gdk, glib, prelude::*};
use gtk4 as gtk;

use super::{cursor::CursorState, mouse::MouseMode};

pub(super) type SharedInputGrab = Rc<RefCell<InputGrabState>>;

#[derive(Default)]
pub(super) struct InputGrabState {
    active: bool,
    focus: FocusGrab,
    attempts_left: u8,
    retry: Option<glib::SourceId>,
}

#[derive(Default)]
struct FocusGrab {
    enabled: bool,
    focused: bool,
    pending: bool,
}

impl FocusGrab {
    fn request(&mut self, focused: bool, display_ready: bool) -> bool {
        if self.focused != focused {
            self.pending = focused && self.enabled;
            self.focused = focused;
        }
        if self.pending && display_ready {
            self.pending = false;
            return true;
        }
        false
    }
}

impl InputGrabState {
    fn cancel_retry(&mut self) {
        self.attempts_left = 0;
        if let Some(source) = self.retry.take() {
            source.remove();
        }
    }

    fn release(&mut self) -> bool {
        self.focus.pending = false;
        self.cancel_retry();
        std::mem::replace(&mut self.active, false)
    }
}

impl Drop for InputGrabState {
    fn drop(&mut self) {
        self.cancel_retry();
    }
}

pub(super) fn new_state(grab_on_focus: bool) -> SharedInputGrab {
    let mut state = InputGrabState::default();
    state.focus.enabled = grab_on_focus;
    Rc::new(RefCell::new(state))
}

pub(super) fn is_active(state: &SharedInputGrab) -> bool {
    state.borrow().active
}

pub(super) fn activate_on_focus(
    window: &gtk::Window,
    picture: &gtk::Picture,
    state: &SharedInputGrab,
) -> bool {
    let requested = state.borrow_mut().focus.request(
        window.is_active(),
        picture.is_visible() && picture.paintable().is_some(),
    );
    requested && activate(window, picture, state, None)
}

/// Enter guest-grab mode and ask the compositor to inhibit host shortcuts.
pub(super) fn activate(
    window: &gtk::Window,
    picture: &gtk::Picture,
    state: &SharedInputGrab,
    event: Option<&gdk::Event>,
) -> bool {
    picture.grab_focus();

    let changed = {
        let mut state = state.borrow_mut();
        state.focus.pending = false;
        if state.active {
            false
        } else {
            state.active = true;
            true
        }
    };

    restart_recovery(window, picture, state, event);

    changed
}

pub(super) fn release(window: &gtk::Window, state: &SharedInputGrab) -> bool {
    let changed = state.borrow_mut().release();

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
    cursor_state.set_mouse_mode(*mouse_mode.borrow());
    cursor_state.set_capture_hidden(capture_hidden);
    cursor_state.apply_to_widget(picture);
}

fn window_toplevel(window: &gtk::Window) -> Option<gdk::Toplevel> {
    window.surface()?.dynamic_cast::<gdk::Toplevel>().ok()
}

// Retry only a small number of times after activation or a real compositor
// state change. A denied request must not leave a perpetual polling source.
const INHIBIT_ATTEMPTS: u8 = 3;
const RETRY_DELAY: Duration = Duration::from_millis(250);

fn restart_recovery(
    window: &gtk::Window,
    picture: &gtk::Picture,
    state: &SharedInputGrab,
    event: Option<&gdk::Event>,
) {
    {
        let mut state = state.borrow_mut();
        state.cancel_retry();
        if !state.active {
            return;
        }
        state.attempts_left = INHIBIT_ATTEMPTS;
    }
    try_inhibit(window, picture, state, event);
}

fn try_inhibit(
    window: &gtk::Window,
    picture: &gtk::Picture,
    state: &SharedInputGrab,
    event: Option<&gdk::Event>,
) {
    {
        let mut state = state.borrow_mut();
        if !state.active || state.attempts_left == 0 {
            return;
        }
        state.attempts_left -= 1;
    }
    if let Some(toplevel) = window_toplevel(window) {
        if toplevel.is_shortcuts_inhibited() {
            state.borrow_mut().cancel_retry();
            return;
        }
        if window.is_active()
            && picture.has_focus()
            && toplevel.state().contains(gdk::ToplevelState::FOCUSED)
        {
            // No RefCell borrow may span a GDK call: notifications can run
            // synchronously and cancel this recovery sequence.
            toplevel.inhibit_system_shortcuts(event);
        }
    }
    schedule_retry(window, picture, state);
}

fn schedule_retry(window: &gtk::Window, picture: &gtk::Picture, state: &SharedInputGrab) {
    if state.borrow().attempts_left == 0 || state.borrow().retry.is_some() {
        return;
    }
    let source = glib::timeout_add_local_once(RETRY_DELAY, {
        let window = window.downgrade();
        let picture = picture.downgrade();
        let state = Rc::downgrade(state);
        move || {
            let Some(state) = state.upgrade() else {
                return;
            };
            state.borrow_mut().retry.take();
            if let (Some(window), Some(picture)) = (window.upgrade(), picture.upgrade()) {
                try_inhibit(&window, &picture, &state, None);
            }
        }
    });
    state.borrow_mut().retry = Some(source);
}

pub(super) fn install_shortcut_recovery(
    window: &gtk::Window,
    picture: &gtk::Picture,
    state: &SharedInputGrab,
) {
    window.connect_realize({
        let picture = picture.downgrade();
        let state = Rc::downgrade(state);
        move |window| {
            let Some(toplevel) = window_toplevel(window) else {
                return;
            };
            toplevel.connect_state_notify({
                let focused = Cell::new(toplevel.state().contains(gdk::ToplevelState::FOCUSED));
                let window = window.downgrade();
                let picture = picture.clone();
                let state = state.clone();
                move |toplevel| {
                    let now_focused = toplevel.state().contains(gdk::ToplevelState::FOCUSED);
                    if focused.replace(now_focused) || !now_focused {
                        return;
                    }
                    if let (Some(window), Some(picture), Some(state)) =
                        (window.upgrade(), picture.upgrade(), state.upgrade())
                    {
                        restart_recovery(&window, &picture, &state, None);
                    }
                }
            });
            toplevel.connect_shortcuts_inhibited_notify({
                let inhibited = Cell::new(toplevel.is_shortcuts_inhibited());
                let window = window.downgrade();
                let picture = picture.clone();
                let state = state.clone();
                move |toplevel| {
                    let now_inhibited = toplevel.is_shortcuts_inhibited();
                    if inhibited.replace(now_inhibited) == now_inhibited {
                        return;
                    }
                    let Some(state) = state.upgrade() else {
                        return;
                    };
                    if now_inhibited {
                        state.borrow_mut().cancel_retry();
                    } else if let (Some(window), Some(picture)) =
                        (window.upgrade(), picture.upgrade())
                    {
                        {
                            let mut state = state.borrow_mut();
                            state.cancel_retry();
                            if !state.active {
                                return;
                            }
                            state.attempts_left = INHIBIT_ATTEMPTS;
                        }
                        // Retry outside the GDK notification, which may be
                        // emitted synchronously while handling a request.
                        schedule_retry(&window, &picture, &state);
                    }
                }
            });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn focus_grab_is_opt_in() {
        let mut focus = FocusGrab::default();
        assert!(!focus.request(true, true));
        assert!(!focus.request(false, true));
        assert!(!focus.request(true, true));
    }

    #[test]
    fn focus_before_first_frame_waits_and_only_grabs_once() {
        let mut focus = FocusGrab {
            enabled: true,
            ..Default::default()
        };
        assert!(!focus.request(true, false));
        assert!(!focus.request(true, false));
        assert!(focus.request(true, true));
        assert!(!focus.request(true, true));
        assert!(!focus.request(false, true));
        assert!(focus.request(true, true));
    }

    #[test]
    fn release_cancels_pending_focus_and_recovery_until_refocus() {
        let mut state = InputGrabState::default();
        state.focus.enabled = true;
        assert!(!state.focus.request(true, false));
        state.attempts_left = INHIBIT_ATTEMPTS;
        assert!(!state.release());
        assert_eq!(state.attempts_left, 0);
        assert!(!state.focus.request(true, true));
        assert!(!state.focus.request(false, true));
        assert!(state.focus.request(true, true));
        state.active = true;
        assert!(state.release());
        assert!(!state.active);
        assert!(!state.focus.request(true, true));
    }

    #[test]
    fn losing_focus_cancels_a_pending_first_frame_grab() {
        let mut focus = FocusGrab {
            enabled: true,
            ..Default::default()
        };
        assert!(!focus.request(true, false));
        assert!(!focus.request(false, false));
        assert!(!focus.request(false, true));
    }

    #[test]
    #[ignore = "requires a GTK display; run in a separate test process"]
    fn recovery_expires_and_release_cancels_pending_retries() {
        gtk::init().unwrap();
        let window = gtk::Window::new();
        let picture = gtk::Picture::new();
        window.set_child(Some(&picture));
        let state = new_state(true);
        install_shortcut_recovery(&window, &picture, &state);
        // Without native focus, no inhibition can succeed. Even then the
        // recovery source must stop after its finite attempt budget.
        state.borrow_mut().active = true;
        restart_recovery(&window, &picture, &state, None);
        assert!(state.borrow().retry.is_some());
        let main_loop = glib::MainLoop::new(None, false);
        glib::timeout_add_local_once(Duration::from_millis(900), {
            let main_loop = main_loop.clone();
            move || main_loop.quit()
        });
        main_loop.run();
        assert_eq!(state.borrow().attempts_left, 0);
        assert!(state.borrow().retry.is_none());

        restart_recovery(&window, &picture, &state, None);
        assert!(state.borrow().retry.is_some());
        assert!(release(&window, &state));
        assert!(state.borrow().retry.is_none());
        // A delayed compositor notification cannot resurrect a released grab.
        restart_recovery(&window, &picture, &state, None);
        assert!(!is_active(&state));
        assert_eq!(state.borrow().attempts_left, 0);
        assert!(state.borrow().retry.is_none());
        window.destroy();
    }
}
