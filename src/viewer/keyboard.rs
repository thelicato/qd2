use gtk::{glib, prelude::*};
use gtk4 as gtk;
use tokio::sync::mpsc as tokio_mpsc;

use super::InputEvent;

pub(super) fn install_keyboard_controller(
    picture: &gtk::Picture,
    input_tx: tokio_mpsc::UnboundedSender<InputEvent>,
) {
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
