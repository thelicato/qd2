use std::{cell::RefCell, collections::HashSet, rc::Rc};

use gtk::{gdk, glib, prelude::*};
use gtk4 as gtk;
use tokio::sync::mpsc as tokio_mpsc;

use super::{InputEvent, grab, hotkeys::Hotkey};

#[derive(Clone)]
pub(super) struct KeyboardControllerHandle {
    state: Rc<RefCell<PressedKeyState>>,
    input_tx: tokio_mpsc::UnboundedSender<InputEvent>,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(super) enum GuestShortcut {
    CtrlAltBackspace,
    CtrlAltDelete,
    CtrlAltF1,
    CtrlAltF2,
    CtrlAltF3,
    CtrlAltF4,
    CtrlAltF5,
    CtrlAltF6,
    CtrlAltF7,
    CtrlAltF8,
    CtrlAltF9,
}

impl GuestShortcut {
    pub(super) fn all() -> &'static [Self] {
        &[
            Self::CtrlAltBackspace,
            Self::CtrlAltDelete,
            Self::CtrlAltF1,
            Self::CtrlAltF2,
            Self::CtrlAltF3,
            Self::CtrlAltF4,
            Self::CtrlAltF5,
            Self::CtrlAltF6,
            Self::CtrlAltF7,
            Self::CtrlAltF8,
            Self::CtrlAltF9,
        ]
    }

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::CtrlAltBackspace => "Ctrl + Alt + BackSpace",
            Self::CtrlAltDelete => "Ctrl + Alt + Delete",
            Self::CtrlAltF1 => "Ctrl + Alt + F1",
            Self::CtrlAltF2 => "Ctrl + Alt + F2",
            Self::CtrlAltF3 => "Ctrl + Alt + F3",
            Self::CtrlAltF4 => "Ctrl + Alt + F4",
            Self::CtrlAltF5 => "Ctrl + Alt + F5",
            Self::CtrlAltF6 => "Ctrl + Alt + F6",
            Self::CtrlAltF7 => "Ctrl + Alt + F7",
            Self::CtrlAltF8 => "Ctrl + Alt + F8",
            Self::CtrlAltF9 => "Ctrl + Alt + F9",
        }
    }

    fn qnums(self) -> &'static [u32] {
        match self {
            Self::CtrlAltBackspace => &[29, 56, 14],
            Self::CtrlAltDelete => &[29, 56, 211],
            Self::CtrlAltF1 => &[29, 56, 59],
            Self::CtrlAltF2 => &[29, 56, 60],
            Self::CtrlAltF3 => &[29, 56, 61],
            Self::CtrlAltF4 => &[29, 56, 62],
            Self::CtrlAltF5 => &[29, 56, 63],
            Self::CtrlAltF6 => &[29, 56, 64],
            Self::CtrlAltF7 => &[29, 56, 65],
            Self::CtrlAltF8 => &[29, 56, 66],
            Self::CtrlAltF9 => &[29, 56, 67],
        }
    }
}

impl KeyboardControllerHandle {
    pub(super) fn force_release(&self) {
        self.state.borrow_mut().release_all(&self.input_tx);
    }
}

pub(super) fn install_keyboard_controller(
    picture: &gtk::Picture,
    input_tx: tokio_mpsc::UnboundedSender<InputEvent>,
    input_grab: grab::SharedInputGrab,
    release_hotkey: Hotkey,
    release_grab: impl Fn() + 'static,
) -> KeyboardControllerHandle {
    let state = Rc::new(RefCell::new(PressedKeyState::default()));
    let release_grab = Rc::new(release_grab);

    let key_controller = gtk::EventControllerKey::new();
    key_controller.set_propagation_phase(gtk::PropagationPhase::Capture);
    key_controller.connect_key_pressed({
        let input_tx = input_tx.clone();
        let input_grab = input_grab.clone();
        let release_hotkey = release_hotkey.clone();
        let release_grab = release_grab.clone();
        let state = state.clone();
        move |_, keyval, keycode, modifiers| {
            if !grab::is_active(&input_grab) {
                return glib::Propagation::Proceed;
            }

            let Some(qnum) = gdk_keycode_to_qnum(keycode) else {
                state.borrow_mut().cancel_release_chord();
                return glib::Propagation::Proceed;
            };

            let release = state.borrow_mut().handle_press(
                qnum,
                keyval,
                modifiers,
                &release_hotkey,
                &input_tx,
            );
            if release {
                release_grab();
            }
            glib::Propagation::Stop
        }
    });
    key_controller.connect_key_released({
        let input_tx = input_tx.clone();
        let release_grab = release_grab.clone();
        let state = state.clone();
        move |_, _, keycode, _| {
            let Some(qnum) = gdk_keycode_to_qnum(keycode) else {
                return;
            };
            let release = state.borrow_mut().handle_release(qnum, &input_tx);
            if release {
                release_grab();
            }
        }
    });
    picture.add_controller(key_controller);

    KeyboardControllerHandle { state, input_tx }
}

pub(super) fn send_guest_shortcut(
    input_tx: &tokio_mpsc::UnboundedSender<InputEvent>,
    shortcut: GuestShortcut,
) {
    for &qnum in shortcut.qnums() {
        let _ = input_tx.send(InputEvent::KeyPress(qnum));
    }

    for &qnum in shortcut.qnums().iter().rev() {
        let _ = input_tx.send(InputEvent::KeyRelease(qnum));
    }
}

#[derive(Default)]
struct PressedKeyState {
    // Track physical press/release events. Host modifier masks do not reliably
    // identify held AltGr or keys remapped to different logical modifiers.
    pressed: HashSet<u32>,
    suppressed_releases: HashSet<u32>,
    chord_keys: HashSet<u32>,
    release_chord_armed: bool,
    release_chord_used: bool,
}

impl PressedKeyState {
    /// Return whether the viewer grab should be released, after dropping the state borrow.
    fn handle_press(
        &mut self,
        qnum: u32,
        keyval: gdk::Key,
        modifiers: gdk::ModifierType,
        hotkey: &Hotkey,
        input_tx: &tokio_mpsc::UnboundedSender<InputEvent>,
    ) -> bool {
        // Repeats must not re-arm a chord canceled by a guest shortcut.
        if self.pressed.contains(&qnum) {
            return false;
        }
        if !hotkey.is_modifier_chord() && hotkey.matches(keyval, modifiers) {
            self.release_all(input_tx);
            self.suppress_next_release(qnum);
            return true;
        }

        if hotkey.chord_member(keyval) {
            self.chord_keys.insert(qnum);
        } else if !self.chord_keys.is_empty() {
            self.cancel_release_chord();
        }
        if hotkey.is_modifier_chord() && hotkey.matches(keyval, modifiers) {
            // A bare chord releases on key-up; Ctrl+Alt+<key> belongs to the guest.
            self.release_chord_armed = !self.release_chord_used
                && self.pressed.iter().all(|key| self.chord_keys.contains(key));
        }
        if self.press(qnum) {
            let _ = input_tx.send(InputEvent::KeyPress(qnum));
        }
        false
    }

    fn cancel_release_chord(&mut self) {
        self.release_chord_armed = false;
        if !self.chord_keys.is_empty() {
            self.release_chord_used = true;
        }
    }

    fn handle_release(
        &mut self,
        qnum: u32,
        input_tx: &tokio_mpsc::UnboundedSender<InputEvent>,
    ) -> bool {
        // Use the recorded physical key even if its keysym changed since press.
        let chord_member = self.chord_keys.remove(&qnum);
        if self.release_chord_armed && chord_member && self.pressed.contains(&qnum) {
            self.release_all(input_tx);
            self.take_suppressed_release(qnum);
            return true;
        }
        if !self.take_suppressed_release(qnum) && self.release(qnum) {
            let _ = input_tx.send(InputEvent::KeyRelease(qnum));
        }
        if self.chord_keys.is_empty() {
            self.release_chord_armed = false;
            self.release_chord_used = false;
        }
        false
    }

    fn press(&mut self, qnum: u32) -> bool {
        // A previous physical release may have gone to the host after focus loss.
        self.suppressed_releases.remove(&qnum);
        self.pressed.insert(qnum)
    }

    fn release(&mut self, qnum: u32) -> bool {
        self.pressed.remove(&qnum)
    }

    fn suppress_next_release(&mut self, qnum: u32) {
        self.suppressed_releases.insert(qnum);
    }

    fn take_suppressed_release(&mut self, qnum: u32) -> bool {
        self.suppressed_releases.remove(&qnum)
    }

    fn release_all(&mut self, input_tx: &tokio_mpsc::UnboundedSender<InputEvent>) {
        self.chord_keys.clear();
        self.release_chord_armed = false;
        self.release_chord_used = false;
        for qnum in self.pressed.drain() {
            let _ = input_tx.send(InputEvent::KeyRelease(qnum));
            self.suppressed_releases.insert(qnum);
        }
    }
}

pub(super) fn gdk_keycode_to_qnum(keycode: u32) -> Option<u32> {
    keycode.checked_sub(8).and_then(linux_keycode_to_qnum)
}

/// GTK gives us Linux evdev keycodes, but QEMU's D-Bus API expects "qnum"
/// codes, so we keep the translation table in one place.
pub(super) fn linux_keycode_to_qnum(linux_keycode: u32) -> Option<u32> {
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

#[cfg(test)]
mod tests {
    use gtk::gdk;
    use gtk4 as gtk;
    use tokio::sync::mpsc as tokio_mpsc;

    use super::{GuestShortcut, PressedKeyState, linux_keycode_to_qnum, send_guest_shortcut};
    use crate::viewer::hotkeys::ViewerHotkeys;

    #[test]
    fn extended_linux_keycodes_are_translated_to_qnum() {
        assert_eq!(linux_keycode_to_qnum(97), Some(157));
        assert_eq!(linux_keycode_to_qnum(100), Some(184));
        assert_eq!(linux_keycode_to_qnum(125), Some(219));
    }

    #[test]
    fn default_release_hotkey_matches_ctrl_alt() {
        let hotkeys = ViewerHotkeys::default();

        assert!(
            hotkeys
                .release_cursor()
                .matches(gdk::Key::Alt_L, gdk::ModifierType::CONTROL_MASK)
        );
        assert!(
            hotkeys
                .release_cursor()
                .matches(gdk::Key::Control_L, gdk::ModifierType::ALT_MASK)
        );
        assert!(
            !hotkeys
                .release_cursor()
                .matches(gdk::Key::Control_L, gdk::ModifierType::empty())
        );
    }

    #[test]
    fn force_release_sends_each_pressed_key_once_and_suppresses_follow_up_releases() {
        let (input_tx, mut input_rx) = tokio_mpsc::unbounded_channel();
        let mut state = PressedKeyState::default();

        assert!(state.press(29));
        assert!(state.press(56));
        state.release_all(&input_tx);

        let mut released = Vec::new();
        while let Ok(event) = input_rx.try_recv() {
            if let super::InputEvent::KeyRelease(qnum) = event {
                released.push(qnum);
            }
        }
        released.sort_unstable();

        assert_eq!(released, vec![29, 56]);
        assert!(state.take_suppressed_release(29));
        assert!(state.take_suppressed_release(56));
        assert!(!state.take_suppressed_release(29));
    }

    #[test]
    fn bare_release_chord_fires_on_release_in_either_modifier_order() {
        for [(first, first_key, first_mask), (second, second_key, _)] in [
            [
                (29, gdk::Key::Control_L, gdk::ModifierType::CONTROL_MASK),
                (56, gdk::Key::Alt_L, gdk::ModifierType::ALT_MASK),
            ],
            [
                (56, gdk::Key::Alt_L, gdk::ModifierType::ALT_MASK),
                (29, gdk::Key::Control_L, gdk::ModifierType::CONTROL_MASK),
            ],
        ] {
            let (tx, mut rx) = tokio_mpsc::unbounded_channel();
            let mut state = PressedKeyState::default();
            let hotkeys = ViewerHotkeys::default();
            let hotkey = hotkeys.release_cursor();
            assert!(!state.handle_press(first, first_key, gdk::ModifierType::empty(), hotkey, &tx));
            assert!(!state.handle_press(second, second_key, first_mask, hotkey, &tx));
            assert_eq!(rx.try_recv().unwrap(), super::InputEvent::KeyPress(first));
            assert_eq!(rx.try_recv().unwrap(), super::InputEvent::KeyPress(second));
            assert!(rx.try_recv().is_err());

            assert!(state.handle_release(second, &tx));
            let mut releases = vec![rx.try_recv().unwrap(), rx.try_recv().unwrap()];
            releases.sort_by_key(|event| match event {
                super::InputEvent::KeyRelease(key) => *key,
                _ => 0,
            });
            assert_eq!(
                releases,
                vec![
                    super::InputEvent::KeyRelease(29),
                    super::InputEvent::KeyRelease(56)
                ]
            );
            assert!(!state.handle_release(first, &tx));
            assert!(rx.try_recv().is_err());
        }
    }

    #[test]
    fn guest_shortcut_and_modifier_repeats_do_not_release_the_grab() {
        let (tx, mut rx) = tokio_mpsc::unbounded_channel();
        let mut state = PressedKeyState::default();
        let hotkeys = ViewerHotkeys::default();
        let hotkey = hotkeys.release_cursor();
        let ctrl_alt = gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::ALT_MASK;
        for (qnum, key, mask) in [
            (29, gdk::Key::Control_L, gdk::ModifierType::empty()),
            (56, gdk::Key::Alt_L, gdk::ModifierType::CONTROL_MASK),
            (203, gdk::Key::Left, ctrl_alt),
            (56, gdk::Key::Alt_L, ctrl_alt),
        ] {
            assert!(!state.handle_press(qnum, key, mask, hotkey, &tx));
        }
        for qnum in [203, 56, 29] {
            assert!(!state.handle_release(qnum, &tx));
        }
        let events: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert_eq!(
            events,
            vec![
                super::InputEvent::KeyPress(29),
                super::InputEvent::KeyPress(56),
                super::InputEvent::KeyPress(203),
                super::InputEvent::KeyRelease(203),
                super::InputEvent::KeyRelease(56),
                super::InputEvent::KeyRelease(29),
            ]
        );
        // A fresh bare chord still works after the shortcut finishes.
        assert!(!state.handle_press(
            29,
            gdk::Key::Control_L,
            gdk::ModifierType::empty(),
            hotkey,
            &tx
        ));
        assert!(!state.handle_press(
            56,
            gdk::Key::Alt_L,
            gdk::ModifierType::CONTROL_MASK,
            hotkey,
            &tx
        ));
        assert!(state.handle_release(29, &tx));
    }

    #[test]
    fn altgr_and_remapped_keys_follow_physical_press_release_events() {
        let (tx, mut rx) = tokio_mpsc::unbounded_channel();
        let mut state = PressedKeyState::default();
        let hotkeys = ViewerHotkeys::default();
        let hotkey = hotkeys.release_cursor();
        for (qnum, key) in [(184, gdk::Key::ISO_Level3_Shift), (58, gdk::Key::Control_L)] {
            assert!(!state.handle_press(qnum, key, gdk::ModifierType::empty(), hotkey, &tx));
        }
        // The next event's modifier mask need not contain ALT for held AltGr.
        assert!(!state.handle_press(
            16,
            gdk::Key::at,
            gdk::ModifierType::CONTROL_MASK,
            hotkey,
            &tx
        ));
        assert_eq!(rx.try_recv().unwrap(), super::InputEvent::KeyPress(184));
        assert_eq!(rx.try_recv().unwrap(), super::InputEvent::KeyPress(58));
        assert_eq!(rx.try_recv().unwrap(), super::InputEvent::KeyPress(16));
        assert!(rx.try_recv().is_err());
        for qnum in [16, 58, 184] {
            assert!(!state.handle_release(qnum, &tx));
            assert_eq!(rx.try_recv().unwrap(), super::InputEvent::KeyRelease(qnum));
        }
    }

    #[test]
    fn key_binding_still_releases_on_press_and_consumes_its_release() {
        let (tx, mut rx) = tokio_mpsc::unbounded_channel();
        let mut state = PressedKeyState::default();
        let hotkeys = ViewerHotkeys::parse(Some("release-cursor=ctrl+q")).unwrap();
        let hotkey = hotkeys.release_cursor();
        assert!(!state.handle_press(
            29,
            gdk::Key::Control_L,
            gdk::ModifierType::empty(),
            hotkey,
            &tx
        ));
        assert!(state.handle_press(
            16,
            gdk::Key::q,
            gdk::ModifierType::CONTROL_MASK,
            hotkey,
            &tx
        ));
        assert_eq!(rx.try_recv().unwrap(), super::InputEvent::KeyPress(29));
        assert_eq!(rx.try_recv().unwrap(), super::InputEvent::KeyRelease(29));
        assert!(!state.handle_release(16, &tx));
        assert!(!state.handle_release(29, &tx));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn focus_loss_clears_chord_and_does_not_swallow_the_next_key_release() {
        let (tx, mut rx) = tokio_mpsc::unbounded_channel();
        let mut state = PressedKeyState::default();
        let hotkeys = ViewerHotkeys::default();
        let hotkey = hotkeys.release_cursor();
        state.handle_press(
            29,
            gdk::Key::Control_L,
            gdk::ModifierType::empty(),
            hotkey,
            &tx,
        );
        state.handle_press(
            56,
            gdk::Key::Alt_L,
            gdk::ModifierType::CONTROL_MASK,
            hotkey,
            &tx,
        );
        state.release_all(&tx);
        while rx.try_recv().is_ok() {}
        // Physical releases happened on the host while the viewer lacked focus.
        assert!(!state.handle_press(
            29,
            gdk::Key::Control_L,
            gdk::ModifierType::empty(),
            hotkey,
            &tx
        ));
        assert!(!state.handle_release(29, &tx));
        assert_eq!(rx.try_recv().unwrap(), super::InputEvent::KeyPress(29));
        assert_eq!(rx.try_recv().unwrap(), super::InputEvent::KeyRelease(29));
        assert!(!state.handle_release(56, &tx));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn a_key_held_before_the_chord_prevents_a_bare_chord_release() {
        let (tx, _rx) = tokio_mpsc::unbounded_channel();
        let mut state = PressedKeyState::default();
        let hotkeys = ViewerHotkeys::default();
        let hotkey = hotkeys.release_cursor();
        for (qnum, key, mask) in [
            (30, gdk::Key::a, gdk::ModifierType::empty()),
            (29, gdk::Key::Control_L, gdk::ModifierType::empty()),
            (56, gdk::Key::Alt_L, gdk::ModifierType::CONTROL_MASK),
        ] {
            assert!(!state.handle_press(qnum, key, mask, hotkey, &tx));
        }
        assert!(!state.handle_release(56, &tx));
        assert!(!state.handle_release(29, &tx));
    }

    #[test]
    fn guest_shortcut_sends_press_and_reverse_release_sequence() {
        let (input_tx, mut input_rx) = tokio_mpsc::unbounded_channel();

        send_guest_shortcut(&input_tx, GuestShortcut::CtrlAltDelete);

        assert_eq!(
            input_rx.try_recv().ok(),
            Some(super::InputEvent::KeyPress(29))
        );
        assert_eq!(
            input_rx.try_recv().ok(),
            Some(super::InputEvent::KeyPress(56))
        );
        assert_eq!(
            input_rx.try_recv().ok(),
            Some(super::InputEvent::KeyPress(211))
        );
        assert_eq!(
            input_rx.try_recv().ok(),
            Some(super::InputEvent::KeyRelease(211))
        );
        assert_eq!(
            input_rx.try_recv().ok(),
            Some(super::InputEvent::KeyRelease(56))
        );
        assert_eq!(
            input_rx.try_recv().ok(),
            Some(super::InputEvent::KeyRelease(29))
        );
    }
}
