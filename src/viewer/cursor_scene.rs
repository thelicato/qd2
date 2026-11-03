//! Draws the absolute-mode guest cursor at the same scale as the framebuffer.

use std::{cell::RefCell, rc::Rc};

use gtk::{gdk, glib, graphene, prelude::*, subclass::prelude::*};
use gtk4 as gtk;

use super::{UiState, mouse::MouseMode};

pub(super) struct SceneState {
    texture: Option<gdk::Texture>,
    hotspot: (i32, i32),
    pointer: Option<(f64, f64)>,
    cursor_visible: bool,
    picture: Option<glib::WeakRef<gtk::Picture>>,
    ui_state: Option<Rc<RefCell<UiState>>>,
    mouse_mode: MouseMode,
}

impl Default for SceneState {
    fn default() -> Self {
        Self {
            texture: None,
            hotspot: (0, 0),
            pointer: None,
            cursor_visible: true,
            picture: None,
            ui_state: None,
            mouse_mode: MouseMode::Disabled,
        }
    }
}

mod cursor_scene_imp {
    use super::*;

    #[derive(Default)]
    pub struct CursorScene {
        pub(super) state: RefCell<SceneState>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for CursorScene {
        const NAME: &'static str = "Qd2CursorScene";
        type Type = super::CursorScene;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for CursorScene {}

    impl WidgetImpl for CursorScene {
        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let state = self.state.borrow();

            if !state.cursor_visible {
                return;
            }
            let Some(texture) = &state.texture else {
                return;
            };
            let Some((pointer_x, pointer_y)) = state.pointer else {
                return;
            };
            let Some(picture) = state.picture.as_ref().and_then(glib::WeakRef::upgrade) else {
                return;
            };
            let Some(ui_state) = &state.ui_state else {
                return;
            };
            if !picture.is_visible() {
                return;
            }
            // In relative mode the local pointer position says nothing about
            // where the guest keeps its cursor, so don't pretend otherwise.
            if state.mouse_mode != MouseMode::Absolute {
                return;
            }
            let Some((frame_width, frame_height)) = ui_state.borrow().frame_size else {
                return;
            };
            let Some(bounds) = picture.compute_bounds(self.obj().upcast_ref::<gtk::Widget>())
            else {
                return;
            };

            let Some(rect) = cursor_rect(
                bounds,
                (frame_width, frame_height),
                (pointer_x, pointer_y),
                (texture.width(), texture.height()),
                state.hotspot,
            ) else {
                return;
            };
            snapshot.push_clip(&bounds);
            snapshot.append_texture(texture, &rect);
            snapshot.pop();
        }
    }
}

glib::wrapper! {
    pub struct CursorScene(ObjectSubclass<cursor_scene_imp::CursorScene>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl CursorScene {
    pub(super) fn new(picture: &gtk::Picture, ui_state: Rc<RefCell<UiState>>) -> Self {
        let scene: Self = glib::Object::builder().build();
        {
            let mut state = scene.imp().state.borrow_mut();
            state.picture = Some(picture.downgrade());
            state.ui_state = Some(ui_state);
        }
        scene.set_can_target(false);
        scene
    }

    pub(super) fn set_mouse_mode(&self, mode: MouseMode) {
        if self.imp().state.borrow().mouse_mode != mode {
            self.imp().state.borrow_mut().mouse_mode = mode;
            self.queue_draw();
        }
    }

    pub(super) fn set_shape(&self, shape: Option<(gdk::Texture, i32, i32)>) {
        {
            let mut state = self.imp().state.borrow_mut();
            match shape {
                Some((texture, hotspot_x, hotspot_y)) => {
                    state.texture = Some(texture);
                    state.hotspot = (hotspot_x, hotspot_y);
                }
                None => state.texture = None,
            }
        }
        self.queue_draw();
    }

    pub(super) fn set_cursor_visible(&self, visible: bool) {
        self.imp().state.borrow_mut().cursor_visible = visible;
        self.queue_draw();
    }

    pub(super) fn set_pointer(&self, pointer: Option<(f64, f64)>) {
        self.imp().state.borrow_mut().pointer = pointer;
        self.queue_draw();
    }
}

/// Match the software cursor to pointer events without retaining its widget tree.
pub(super) fn track_pointer(
    picture: &gtk::Picture,
    cursor_state: &Rc<RefCell<super::cursor::CursorState>>,
) {
    fn update(
        picture: &gtk::Picture,
        state: &std::rc::Weak<RefCell<super::cursor::CursorState>>,
        pointer: Option<(f64, f64)>,
    ) {
        let pointer = pointer.filter(|(x, y)| {
            x.is_finite()
                && y.is_finite()
                && *x >= 0.0
                && *y >= 0.0
                && *x < f64::from(picture.width())
                && *y < f64::from(picture.height())
        });
        if let Some(state) = state.upgrade() {
            let mut state = state.borrow_mut();
            state.set_pointer(pointer);
            state.apply_to_widget(picture);
        }
    }
    let motion = gtk::EventControllerMotion::new();
    motion.connect_enter({
        let picture = picture.downgrade();
        let state = Rc::downgrade(cursor_state);
        move |_, x, y| {
            if let Some(picture) = picture.upgrade() {
                update(&picture, &state, Some((x, y)));
            }
        }
    });
    motion.connect_motion({
        let picture = picture.downgrade();
        let state = Rc::downgrade(cursor_state);
        move |_, x, y| {
            if let Some(picture) = picture.upgrade() {
                update(&picture, &state, Some((x, y)));
            }
        }
    });
    motion.connect_leave({
        let picture = picture.downgrade();
        let state = Rc::downgrade(cursor_state);
        move |_| {
            if let Some(picture) = picture.upgrade() {
                update(&picture, &state, None);
            }
        }
    });
    picture.add_controller(motion);
    picture.connect_unmap({
        let state = Rc::downgrade(cursor_state);
        move |picture| update(picture, &state, None)
    });
}

fn cursor_rect(
    bounds: graphene::Rect,
    frame: (u32, u32),
    pointer: (f64, f64),
    cursor: (i32, i32),
    hotspot: (i32, i32),
) -> Option<graphene::Rect> {
    if frame.0 == 0
        || frame.1 == 0
        || cursor.0 <= 0
        || cursor.1 <= 0
        || !pointer.0.is_finite()
        || !pointer.1.is_finite()
    {
        return None;
    }
    let width = f64::from(bounds.width());
    let height = f64::from(bounds.height());
    if !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0 {
        return None;
    }
    let scale = (width / f64::from(frame.0)).min(height / f64::from(frame.1));
    let x_offset = (width - f64::from(frame.0) * scale) / 2.0;
    let y_offset = (height - f64::from(frame.1) * scale) / 2.0;
    let x = x_offset + (pointer.0 - x_offset).clamp(0.0, f64::from(frame.0 - 1) * scale);
    let y = y_offset + (pointer.1 - y_offset).clamp(0.0, f64::from(frame.1 - 1) * scale);
    Some(graphene::Rect::new(
        (f64::from(bounds.x()) + x - f64::from(hotspot.0) * scale) as f32,
        (f64::from(bounds.y()) + y - f64::from(hotspot.1) * scale) as f32,
        (f64::from(cursor.0) * scale) as f32,
        (f64::from(cursor.1) * scale) as f32,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_pixels_and_hotspot_follow_framebuffer_scale() {
        let rect = cursor_rect(
            graphene::Rect::new(20.0, 30.0, 960.0, 540.0),
            (1920, 1080),
            (100.0, 100.0),
            (32, 16),
            (4, 2),
        )
        .unwrap();
        assert_eq!(
            (rect.x(), rect.y(), rect.width(), rect.height()),
            (118.0, 129.0, 16.0, 8.0)
        );
        let rect = cursor_rect(
            graphene::Rect::new(0.0, 0.0, 600.0, 450.0),
            (800, 600),
            (300.0, 225.0),
            (24, 16),
            (6, 2),
        )
        .unwrap();
        assert_eq!(
            (rect.x(), rect.y(), rect.width(), rect.height()),
            (295.5, 223.5, 18.0, 12.0)
        );
    }

    #[test]
    fn letterbox_margins_pin_the_hotspot_to_the_guest_edge() {
        let bounds = graphene::Rect::new(0.0, 0.0, 800.0, 600.0);
        let top = cursor_rect(bounds, (640, 360), (400.0, 10.0), (8, 8), (0, 0)).unwrap();
        let bottom = cursor_rect(bounds, (640, 360), (400.0, 599.0), (8, 8), (0, 0)).unwrap();
        assert_eq!(top.y(), 75.0);
        assert_eq!(bottom.y(), 523.75);
    }

    #[test]
    fn a_new_frame_size_changes_cursor_scale_without_pointer_motion() {
        let bounds = graphene::Rect::new(0.0, 0.0, 800.0, 600.0);
        let before = cursor_rect(bounds, (800, 600), (400.0, 300.0), (16, 16), (2, 2)).unwrap();
        let after = cursor_rect(bounds, (1600, 1200), (400.0, 300.0), (16, 16), (2, 2)).unwrap();
        assert_eq!(
            (before.x(), before.y(), before.width()),
            (398.0, 298.0, 16.0)
        );
        assert_eq!((after.x(), after.y(), after.width()), (399.0, 299.0, 8.0));
    }

    #[test]
    fn missing_or_invalid_geometry_does_not_draw_a_cursor() {
        let bounds = graphene::Rect::new(0.0, 0.0, 800.0, 600.0);
        assert!(cursor_rect(bounds, (0, 600), (10.0, 10.0), (8, 8), (0, 0)).is_none());
        assert!(cursor_rect(bounds, (800, 600), (f64::NAN, 10.0), (8, 8), (0, 0)).is_none());
        assert!(
            cursor_rect(
                graphene::Rect::new(0.0, 0.0, 0.0, 0.0),
                (800, 600),
                (10.0, 10.0),
                (8, 8),
                (0, 0)
            )
            .is_none()
        );
    }

    #[test]
    #[ignore = "requires a GTK display; can run with the Broadway backend"]
    fn scene_pixels_and_host_cursor_follow_mode_shape_and_resolution_changes() {
        use super::super::cursor::{CursorState, GuestCursor};

        fn settle() {
            let main_loop = glib::MainLoop::new(None, false);
            let quit = main_loop.clone();
            glib::timeout_add_local_once(std::time::Duration::from_secs(1), move || quit.quit());
            main_loop.run();
        }

        fn pixel(overlay: &gtk::Overlay, x: usize, y: usize) -> u32 {
            let snapshot = gtk::Snapshot::new();
            gtk::WidgetPaintable::new(Some(overlay)).snapshot(
                &snapshot,
                overlay.width() as f64,
                overlay.height() as f64,
            );
            let node = snapshot.to_node().unwrap();
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
            let offset = y * image.stride() as usize + x * 4;
            let pixels = image.data().unwrap();
            u32::from_ne_bytes(pixels[offset..offset + 4].try_into().unwrap())
        }

        gtk::init().expect("GTK display required");
        for no_offload in [false, true] {
            let picture = gtk::Picture::new();
            picture.set_hexpand(true);
            picture.set_vexpand(true);
            picture.set_paintable(Some(&gdk::MemoryTexture::new(
                1,
                1,
                gdk::MemoryFormat::R8g8b8a8,
                &glib::Bytes::from_owned(vec![0_u8, 0, 255, 255]),
                4,
            )));
            let ui_state = Rc::new(RefCell::new(UiState {
                frame_size: Some((160, 160)),
                ..Default::default()
            }));
            let scene = CursorScene::new(&picture, ui_state.clone());
            let mut cursor = CursorState::new(scene.clone());
            let shape = GuestCursor::from_qemu(qemu_display::Cursor {
                width: 8,
                height: 8,
                hot_x: 2,
                hot_y: 2,
                data: 0xffff0000_u32.to_ne_bytes().repeat(64),
            })
            .unwrap();
            cursor.set_shape(shape);
            cursor.set_mouse_mode(MouseMode::Absolute);
            cursor.set_pointer(Some((40.0, 40.0)));
            cursor.apply_to_widget(&picture);
            let status = gtk::Label::new(None);
            status.set_visible(false);
            let overlay = super::super::display_overlay(
                &picture,
                &status,
                no_offload,
                Some(scene.upcast_ref()),
            );
            let window = gtk::Window::builder()
                .default_width(80)
                .default_height(80)
                .decorated(false)
                .child(&overlay)
                .build();
            window.present();
            overlay.allocate(80, 80, -1, None);
            settle();
            assert_eq!(pixel(&overlay, 40, 40), 0xffff0000);
            assert_eq!(pixel(&overlay, 42, 40), 0xffff0000);
            assert_eq!(pixel(&overlay, 38, 38), 0xff0000ff);
            assert_eq!(
                picture.cursor().and_then(|cursor| cursor.name()).as_deref(),
                Some("none")
            );
            assert_eq!(
                overlay.pick(40.0, 40.0, gtk::PickFlags::empty()),
                Some(picture.clone().upcast::<gtk::Widget>())
            );

            cursor.set_mouse_mode(MouseMode::Relative);
            cursor.apply_to_widget(&picture);
            assert!(picture.cursor().is_none());
            settle();
            assert_eq!(pixel(&overlay, 40, 40), 0xff0000ff);
            cursor.set_capture_hidden(true);
            cursor.apply_to_widget(&picture);
            assert_eq!(
                picture.cursor().and_then(|cursor| cursor.name()).as_deref(),
                Some("none")
            );
            cursor.set_capture_hidden(false);
            cursor.set_visible(false);
            cursor.apply_to_widget(&picture);
            assert!(
                picture.cursor().is_none(),
                "releasing a relative grab must restore the host cursor"
            );

            cursor.set_visible(true);
            cursor.set_mouse_mode(MouseMode::Absolute);
            ui_state.borrow_mut().frame_size = Some((320, 320));
            cursor.redraw();
            cursor.apply_to_widget(&picture);
            settle();
            assert_eq!(pixel(&overlay, 40, 40), 0xffff0000);
            assert_eq!(pixel(&overlay, 42, 40), 0xff0000ff);
            cursor.set_pointer(None);
            cursor.apply_to_widget(&picture);
            assert!(
                picture.cursor().is_none(),
                "leaving the picture must restore the host pointer"
            );
            cursor.set_pointer(Some((40.0, 40.0)));
            cursor.set_shape(None);
            cursor.apply_to_widget(&picture);
            assert!(
                picture.cursor().is_none(),
                "a missing guest shape must retain a host pointer"
            );
            window.destroy();
        }
    }
}
