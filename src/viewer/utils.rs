use anyhow::{Context, Result};
use gtk::{gdk, glib, prelude::*};
use gtk4 as gtk;

const APP_ICON_PNG: &[u8] = include_bytes!("../../logo.png");

pub(super) fn load_app_icon() -> Result<gdk::Texture> {
    let bytes = glib::Bytes::from_static(APP_ICON_PNG);
    gdk::Texture::from_bytes(&bytes).context("failed to decode embedded app icon")
}

pub(super) fn apply_window_icon(window: &gtk::Window, icon: &gdk::Texture) -> Result<()> {
    let native = window
        .native()
        .context("GTK window does not expose a native surface yet")?;
    let surface = native
        .surface()
        .context("GTK window does not have a GDK surface yet")?;
    let toplevel = surface
        .dynamic_cast_ref::<gdk::Toplevel>()
        .context("GTK surface is not a toplevel window")?;
    toplevel.set_icon_list(std::slice::from_ref(icon));
    Ok(())
}

/// Keep the initial window in a sensible desktop-sized range without letting a
/// large guest immediately open an oversized toplevel.
pub(super) fn suggested_window_size(width: u32, height: u32) -> (i32, i32) {
    let width = width.clamp(640, 1280);
    let height = height.clamp(480, 960);
    (
        i32::try_from(width).unwrap_or(1280),
        i32::try_from(height).unwrap_or(960),
    )
}
