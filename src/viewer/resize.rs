use std::{cell::RefCell, rc::Rc, time::Duration};

use gtk::{glib, prelude::*};
use gtk4 as gtk;
use tokio::sync::mpsc as tokio_mpsc;

use super::InputEvent;

const RESIZE_DELAY: Duration = Duration::from_millis(350);

#[derive(Default)]
struct ResizeState {
    pending: Option<glib::SourceId>,
    last_sent: Option<(u32, u32)>,
}

impl ResizeState {
    fn cancel(&mut self) {
        if let Some(source) = self.pending.take() {
            source.remove();
        }
    }

    fn next_size(&mut self, width: i32, height: i32, scale: f64) -> Option<(u32, u32)> {
        if width <= 0 || height <= 0 || !scale.is_finite() || scale <= 0.0 {
            return None;
        }
        // The surface scale may be fractional; scale_factor() rounds it up.
        let width = (f64::from(width) * scale).floor();
        let height = (f64::from(height) * scale).floor();
        if width < 1.0 || height < 1.0 || width > u32::MAX as f64 || height > u32::MAX as f64 {
            return None;
        }
        let size = (width as u32, height as u32);
        if self.last_sent == Some(size) {
            return None;
        }
        self.last_sent = Some(size);
        Some(size)
    }
}

/// Request the settled physical display size without flooding QEMU during a drag.
pub(super) fn install(
    window: &gtk::Window,
    picture: &gtk::Picture,
    input_tx: tokio_mpsc::UnboundedSender<InputEvent>,
) {
    let state = Rc::new(RefCell::new(ResizeState::default()));
    let schedule: Rc<dyn Fn()> = Rc::new({
        let state = state.clone();
        let window = window.downgrade();
        let picture = picture.downgrade();
        move || {
            state.borrow_mut().cancel();
            let state_for_send = state.clone();
            let window = window.clone();
            let picture = picture.clone();
            let input_tx = input_tx.clone();
            let source = glib::timeout_add_local_once(RESIZE_DELAY, move || {
                state_for_send.borrow_mut().pending = None;
                let (Some(window), Some(picture)) = (window.upgrade(), picture.upgrade()) else {
                    return;
                };
                if !window.is_mapped() || !picture.is_visible() {
                    return;
                }
                let scale = window
                    .surface()
                    .map(|surface| surface.scale())
                    .unwrap_or_else(|| f64::from(picture.scale_factor()));
                let size =
                    state_for_send
                        .borrow_mut()
                        .next_size(picture.width(), picture.height(), scale);
                if let Some((width, height)) = size {
                    let _ = input_tx.send(InputEvent::UiInfo { width, height });
                }
            });
            state.borrow_mut().pending = Some(source);
        }
    });

    window.connect_default_width_notify({
        let schedule = schedule.clone();
        move |_| schedule()
    });
    window.connect_default_height_notify({
        let schedule = schedule.clone();
        move |_| schedule()
    });
    window.connect_fullscreened_notify({
        let schedule = schedule.clone();
        move |_| schedule()
    });
    window.connect_maximized_notify({
        let schedule = schedule.clone();
        move |_| schedule()
    });
    window.connect_map({
        let schedule = schedule.clone();
        move |_| schedule()
    });
    picture.connect_visible_notify({
        let schedule = schedule.clone();
        move |_| schedule()
    });
    window.connect_realize({
        let schedule = schedule.clone();
        move |window| {
            if let Some(surface) = window.surface() {
                surface.connect_layout({
                    let schedule = schedule.clone();
                    move |_, _, _| schedule()
                });
                // Fractional scale can change without the integer scale factor changing.
                surface.connect_scale_notify({
                    let schedule = schedule.clone();
                    move |_| schedule()
                });
            }
        }
    });
    window.connect_scale_factor_notify(move |_| schedule());
    window.connect_unmap({
        let state = state.clone();
        move |_| state.borrow_mut().cancel()
    });
    window.connect_destroy(move |_| state.borrow_mut().cancel());
}

#[cfg(test)]
mod tests {
    use super::ResizeState;

    #[test]
    #[ignore = "requires a GTK display; can run with the Broadway backend"]
    fn status_changes_do_not_resize_the_guest_and_unmapping_cancels_requests() {
        use super::*;

        fn settle() {
            let main_loop = glib::MainLoop::new(None, false);
            let quit = main_loop.clone();
            glib::timeout_add_local_once(Duration::from_secs(1), move || {
                quit.quit();
            });
            main_loop.run();
        }

        gtk::init().expect("GTK display required");
        for no_offload in [false, true] {
            let picture = gtk::Picture::new();
            picture.set_hexpand(true);
            picture.set_vexpand(true);
            picture.set_paintable(Some(&gtk::gdk::MemoryTexture::new(
                2,
                2,
                gtk::gdk::MemoryFormat::R8g8b8a8,
                &glib::Bytes::from_owned([255_u8, 0, 0, 255].repeat(4)),
                8,
            )));
            let status = gtk::Label::new(None);
            status.set_wrap(true);
            status.set_can_target(false);
            status.set_visible(false);
            let overlay = super::super::display_overlay(&picture, &status, no_offload);
            let window = gtk::Window::builder()
                .default_width(640)
                .default_height(480)
                .decorated(false)
                .child(&overlay)
                .build();
            let (tx, mut rx) = tokio_mpsc::unbounded_channel();
            install(&window, &picture, tx);
            window.present();
            // Allocate explicitly so a Broadway server needs no browser client.
            overlay.allocate(640, 480, -1, None);
            settle();
            let first = rx.try_recv();
            assert!(
                matches!(first, Ok(InputEvent::UiInfo { width, height }) if width > 0 && height > 0),
                "initial resize: {first:?}; mapped={}, visible={}, allocation={}x{}",
                window.is_mapped(),
                picture.is_visible(),
                picture.width(),
                picture.height()
            );
            assert!(
                rx.try_recv().is_err(),
                "initial layout should coalesce into one request"
            );
            let size = (picture.width(), picture.height());

            // A software texture must remain visible when offload is unavailable.
            let snapshot = gtk::Snapshot::new();
            gtk::WidgetPaintable::new(Some(&overlay)).snapshot(
                &snapshot,
                overlay.width() as f64,
                overlay.height() as f64,
            );
            let node = snapshot
                .to_node()
                .expect("display should produce render content");
            let mut image = gtk::cairo::ImageSurface::create(
                gtk::cairo::Format::ARgb32,
                overlay.width(),
                overlay.height(),
            )
            .unwrap();
            {
                let context = gtk::cairo::Context::new(&image).unwrap();
                node.draw(&context);
            }
            image.flush();
            let offset = (overlay.height() / 2 * image.stride() + overlay.width() / 2 * 4) as usize;
            let pixels = image.data().unwrap();
            assert_eq!(
                u32::from_ne_bytes(pixels[offset..offset + 4].try_into().unwrap()),
                0xffff0000
            );
            drop(pixels);

            status.set_label(&"Guest resize request failed. ".repeat(30));
            status.set_visible(true);
            overlay.allocate(size.0, size.1, -1, None);
            settle();
            assert_eq!((picture.width(), picture.height()), size);
            assert_eq!(
                overlay.pick(100.0, 100.0, gtk::PickFlags::empty()),
                Some(picture.clone().upcast::<gtk::Widget>())
            );
            assert!(
                rx.try_recv().is_err(),
                "status text changed the guest resize target"
            );
            status.set_visible(false);
            overlay.allocate(size.0, size.1, -1, None);
            settle();
            assert_eq!((picture.width(), picture.height()), size);
            assert!(rx.try_recv().is_err());

            window.set_default_size(800, 600);
            window.set_visible(false);
            settle();
            assert!(
                rx.try_recv().is_err(),
                "hidden window sent a pending resize"
            );
            window.destroy();
        }
    }

    #[test]
    fn physical_size_uses_fractional_scale_and_floors_rounding() {
        let mut state = ResizeState::default();
        assert_eq!(state.next_size(853, 479, 1.5), Some((1279, 718)));
        assert_eq!(state.next_size(853, 479, 2.0), Some((1706, 958)));
    }

    #[test]
    fn unchanged_physical_size_is_not_sent_again() {
        let mut state = ResizeState::default();
        assert_eq!(state.next_size(800, 600, 1.0), Some((800, 600)));
        assert_eq!(state.next_size(800, 600, 1.0), None);
        assert_eq!(state.next_size(400, 300, 2.0), None);
        assert_eq!(state.next_size(800, 600, 1.25), Some((1000, 750)));
    }

    #[test]
    fn invalid_allocations_do_not_replace_the_last_size() {
        let mut state = ResizeState::default();
        assert_eq!(state.next_size(640, 480, 1.0), Some((640, 480)));
        for (width, height, scale) in [
            (0, 480, 1.0),
            (640, -1, 1.0),
            (640, 480, 0.0),
            (640, 480, f64::NAN),
            (640, 480, f64::INFINITY),
            (i32::MAX, 480, 4.0),
        ] {
            assert_eq!(state.next_size(width, height, scale), None);
        }
        assert_eq!(state.next_size(640, 480, 1.0), None);
    }
}
