use std::convert::TryFrom;

use anyhow::{Context, Result, bail};
use gtk::{gdk, glib, prelude::*};
use gtk4 as gtk;
use qemu_display::Cursor as QemuCursor;

const CURSOR_BYTES_PER_PIXEL: usize = 4;
const HIDDEN_CURSOR_PIXEL: [u8; CURSOR_BYTES_PER_PIXEL] = [0, 0, 0, 0];

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

    fn to_gdk_cursor(&self) -> gdk::Cursor {
        let bytes = glib::Bytes::from_owned(self.rgba.clone());
        let texture = gdk::MemoryTexture::new(
            self.width,
            self.height,
            gdk::MemoryFormat::R8g8b8a8,
            &bytes,
            self.stride(),
        );
        let fallback = gdk::Cursor::from_name("default", None);
        gdk::Cursor::from_texture(&texture, self.hotspot_x, self.hotspot_y, fallback.as_ref())
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
    active_cursor: Option<gdk::Cursor>,
    hidden_cursor: Option<gdk::Cursor>,
}

impl Default for CursorState {
    fn default() -> Self {
        Self {
            visible: true,
            capture_hidden: false,
            active_cursor: None,
            hidden_cursor: None,
        }
    }
}

impl CursorState {
    pub(super) fn set_shape(&mut self, shape: Option<GuestCursor>) {
        self.active_cursor = shape.as_ref().map(GuestCursor::to_gdk_cursor);
    }

    pub(super) fn set_visible(&mut self, visible: bool) {
        self.visible = visible;
    }

    pub(super) fn set_capture_hidden(&mut self, capture_hidden: bool) {
        self.capture_hidden = capture_hidden;
    }

    pub(super) fn apply_to_widget(&mut self, widget: &impl IsA<gtk::Widget>) {
        if !self.visible || self.capture_hidden {
            let hidden = self.hidden_cursor();
            widget.set_cursor(Some(&hidden));
            return;
        }

        widget.set_cursor(self.active_cursor.as_ref());
    }

    fn hidden_cursor(&mut self) -> gdk::Cursor {
        self.hidden_cursor
            .get_or_insert_with(|| {
                let bytes = glib::Bytes::from_static(&HIDDEN_CURSOR_PIXEL);
                let texture = gdk::MemoryTexture::new(
                    1,
                    1,
                    gdk::MemoryFormat::R8g8b8a8,
                    &bytes,
                    CURSOR_BYTES_PER_PIXEL,
                );
                gdk::Cursor::from_texture(&texture, 0, 0, None)
            })
            .clone()
    }
}

#[cfg(test)]
mod tests {
    use super::{GuestCursor, argb32_words_to_rgba};
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
}
