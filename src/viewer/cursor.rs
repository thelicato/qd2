use std::convert::TryFrom;

use anyhow::{Context, Result, bail};
use gtk::{gdk, glib, prelude::*};
use gtk4 as gtk;
use qemu_display::Cursor as QemuCursor;

const CURSOR_BYTES_PER_PIXEL: usize = 4;

#[derive(Clone, Debug)]
pub(super) struct GuestCursor {
    width: i32,
    height: i32,
    hotspot_x: i32,
    hotspot_y: i32,
    rgba: Vec<u8>,
}

impl GuestCursor {
    /// QEMU sends cursor pixels as ARGB32 words. On little-endian Linux the
    /// byte order in memory is BGRA, so we normalize to plain RGBA before
    /// handing the texture to GTK.
    pub(super) fn from_qemu(cursor: QemuCursor) -> Result<Option<Self>> {
        let width = u32::try_from(cursor.width).context("negative guest cursor width")?;
        let height = u32::try_from(cursor.height).context("negative guest cursor height")?;

        if width == 0 || height == 0 {
            return Ok(None);
        }

        let expected_len = usize::try_from(width)
            .context("invalid guest cursor width")?
            .checked_mul(usize::try_from(height).context("invalid guest cursor height")?)
            .and_then(|pixels| pixels.checked_mul(CURSOR_BYTES_PER_PIXEL))
            .context("guest cursor size overflow")?;

        if cursor.data.len() < expected_len {
            bail!(
                "guest cursor payload is too short for {}x{} pixels",
                cursor.width,
                cursor.height
            );
        }

        let max_hotspot_x = cursor.width.saturating_sub(1);
        let max_hotspot_y = cursor.height.saturating_sub(1);

        Ok(Some(Self {
            width: cursor.width,
            height: cursor.height,
            hotspot_x: cursor.hot_x.clamp(0, max_hotspot_x),
            hotspot_y: cursor.hot_y.clamp(0, max_hotspot_y),
            rgba: argb32_words_to_rgba(&cursor.data[..expected_len]),
        }))
    }

    fn to_texture(&self) -> (gdk::Texture, i32, i32) {
        let bytes = glib::Bytes::from_owned(self.rgba.clone());
        let texture = gdk::MemoryTexture::new(
            self.width,
            self.height,
            gdk::MemoryFormat::R8g8b8a8,
            &bytes,
            self.stride(),
        );
        (texture.upcast(), self.hotspot_x, self.hotspot_y)
    }

    fn stride(&self) -> usize {
        usize::try_from(self.width).unwrap_or(0) * CURSOR_BYTES_PER_PIXEL
    }
}

fn argb32_words_to_rgba(src: &[u8]) -> Vec<u8> {
    let mut rgba = Vec::with_capacity(src.len());

    for pixel in src.chunks_exact(CURSOR_BYTES_PER_PIXEL) {
        #[cfg(target_endian = "little")]
        rgba.extend_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]);

        #[cfg(target_endian = "big")]
        rgba.extend_from_slice(&[pixel[1], pixel[2], pixel[3], pixel[0]]);
    }

    rgba
}

pub(super) struct CursorState {
    visible: bool,
    capture_hidden: bool,
    has_shape: bool,
    pointer_inside: bool,
    mouse_mode: super::mouse::MouseMode,
    scene: super::cursor_scene::CursorScene,
}

impl CursorState {
    pub(super) fn new(scene: super::cursor_scene::CursorScene) -> Self {
        Self {
            visible: true,
            capture_hidden: false,
            has_shape: false,
            pointer_inside: false,
            mouse_mode: super::mouse::MouseMode::Disabled,
            scene,
        }
    }

    pub(super) fn set_shape(&mut self, shape: Option<GuestCursor>) {
        self.has_shape = shape.is_some();
        self.scene
            .set_shape(shape.as_ref().map(GuestCursor::to_texture));
    }

    pub(super) fn set_visible(&mut self, visible: bool) {
        self.visible = visible;
        self.scene.set_cursor_visible(visible);
    }

    pub(super) fn set_capture_hidden(&mut self, capture_hidden: bool) {
        self.capture_hidden = capture_hidden;
    }

    pub(super) fn set_mouse_mode(&mut self, mode: super::mouse::MouseMode) {
        self.mouse_mode = mode;
        self.scene.set_mouse_mode(mode);
    }

    pub(super) fn set_pointer(&mut self, pointer: Option<(f64, f64)>) {
        self.pointer_inside = pointer.is_some();
        self.scene.set_pointer(pointer);
    }

    pub(super) fn redraw(&self) {
        self.scene.queue_draw();
    }

    pub(super) fn apply_to_widget(&self, widget: &impl IsA<gtk::Widget>) {
        if hide_host_cursor(
            self.mouse_mode,
            self.capture_hidden,
            self.visible,
            self.has_shape,
            self.pointer_inside,
        ) {
            if widget.cursor().and_then(|cursor| cursor.name()).as_deref() != Some("none") {
                widget.set_cursor_from_name(Some("none"));
            }
        } else if widget.cursor().is_some() {
            widget.set_cursor(None::<&gdk::Cursor>);
        }
    }
}

fn hide_host_cursor(
    mode: super::mouse::MouseMode,
    capture_hidden: bool,
    visible: bool,
    has_shape: bool,
    pointer_inside: bool,
) -> bool {
    use super::mouse::MouseMode;
    match mode {
        MouseMode::Absolute => pointer_inside && (has_shape || !visible),
        MouseMode::Relative => capture_hidden,
        MouseMode::Disabled => false,
    }
}

#[cfg(test)]
mod tests {
    use super::{GuestCursor, argb32_words_to_rgba, hide_host_cursor};
    use crate::viewer::mouse::MouseMode;
    use qemu_display::Cursor as QemuCursor;

    #[test]
    fn empty_guest_cursor_clears_the_shape() {
        let cursor = GuestCursor::from_qemu(QemuCursor {
            width: 0,
            height: 16,
            hot_x: 0,
            hot_y: 0,
            data: Vec::new(),
        })
        .expect("empty guest cursor should be accepted");

        assert!(cursor.is_none());
    }

    #[test]
    fn guest_cursor_requires_enough_argb_bytes() {
        let error = GuestCursor::from_qemu(QemuCursor {
            width: 2,
            height: 2,
            hot_x: 0,
            hot_y: 0,
            data: vec![0; 15],
        })
        .expect_err("short guest cursor payload should be rejected");

        assert!(error.to_string().contains("too short"));
    }

    #[test]
    fn argb32_black_stays_opaque_black() {
        let rgba = argb32_words_to_rgba(&[0x00, 0x00, 0x00, 0xff]);

        assert_eq!(rgba, vec![0x00, 0x00, 0x00, 0xff]);
    }

    #[test]
    fn argb32_color_channels_are_swizzled_to_rgba() {
        let rgba = argb32_words_to_rgba(&[0x12, 0x34, 0x56, 0x78]);

        #[cfg(target_endian = "little")]
        assert_eq!(rgba, vec![0x56, 0x34, 0x12, 0x78]);

        #[cfg(target_endian = "big")]
        assert_eq!(rgba, vec![0x34, 0x56, 0x78, 0x12]);
    }

    #[test]
    fn relative_release_restores_host_pointer_regardless_of_guest_shape_or_visibility() {
        for visible in [true, false] {
            for shape in [true, false] {
                assert!(!hide_host_cursor(
                    MouseMode::Relative,
                    false,
                    visible,
                    shape,
                    true
                ));
                assert!(hide_host_cursor(
                    MouseMode::Relative,
                    true,
                    visible,
                    shape,
                    true
                ));
                assert!(!hide_host_cursor(
                    MouseMode::Disabled,
                    true,
                    visible,
                    shape,
                    true
                ));
            }
        }
    }

    #[test]
    fn absolute_cursor_hides_host_only_over_the_picture_when_replaced_or_guest_hidden() {
        assert!(hide_host_cursor(
            MouseMode::Absolute,
            false,
            true,
            true,
            true
        ));
        assert!(hide_host_cursor(
            MouseMode::Absolute,
            false,
            false,
            false,
            true
        ));
        assert!(!hide_host_cursor(
            MouseMode::Absolute,
            false,
            true,
            false,
            true
        ));
        assert!(!hide_host_cursor(
            MouseMode::Absolute,
            false,
            true,
            true,
            false
        ));
        assert!(!hide_host_cursor(
            MouseMode::Absolute,
            false,
            false,
            true,
            false
        ));
    }
}
