use std::{cell::RefCell, convert::TryFrom, rc::Rc};

use anyhow::{Context, Result, bail};
use gtk::{cairo, gdk, glib, gsk, prelude::*};
use gtk4 as gtk;
#[cfg(unix)]
use qemu_display::ScanoutDMABUF;
use qemu_display::UpdateDMABUF;

#[cfg(unix)]
use gdk::subclass::prelude::*;
#[cfg(unix)]
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};

use super::UiState;

/// The part of a DMABUF that a console actually shows, in texture pixels.
///
/// X11 guests render every output into one framebuffer and scan each head out
/// of a sub-rectangle of it, so QEMU hands us the whole backing buffer plus
/// this window into it (`ScanoutDMABUF2`). The texture must be imported at its
/// real backing size — describing a tiled buffer with the sub-rectangle's
/// width but the backing stride makes Mesa reject the import (EGL_BAD_ALLOC).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(super) struct DmabufView {
    pub(super) x: u32,
    pub(super) y: u32,
    pub(super) width: u32,
    pub(super) height: u32,
}

impl DmabufView {
    pub(super) fn full(width: u32, height: u32) -> Self {
        Self {
            x: 0,
            y: 0,
            width,
            height,
        }
    }

    /// Clamp a view to the backing buffer; a nonsensical view degrades to the
    /// whole buffer instead of an empty picture.
    pub(super) fn clamped(self, backing_width: u32, backing_height: u32) -> Self {
        if self.width == 0
            || self.height == 0
            || self.x >= backing_width
            || self.y >= backing_height
        {
            return Self::full(backing_width, backing_height);
        }
        Self {
            x: self.x,
            y: self.y,
            width: self.width.min(backing_width - self.x),
            height: self.height.min(backing_height - self.y),
        }
    }
}

pub(super) fn scanout_geometry(
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    backing_width: u32,
    backing_height: u32,
) -> (u32, u32, DmabufView) {
    if backing_width == 0 || backing_height == 0 {
        // Without backing dimensions the legacy scanout is already the full view.
        (width, height, DmabufView::full(width, height))
    } else {
        (
            backing_width,
            backing_height,
            DmabufView {
                x,
                y,
                width,
                height,
            }
            .clamped(backing_width, backing_height),
        )
    }
}

#[cfg(unix)]
pub(super) struct DmabufPresentation {
    texture: gdk::Texture,
    fds: Vec<OwnedFd>,
    width: u32,
    height: u32,
    view: DmabufView,
    offset: [u32; 4],
    stride: [u32; 4],
    fourcc: u32,
    modifier: u64,
    y0_top: bool,
    num_planes: u32,
}

#[cfg(unix)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(super) struct DmabufViewTransform {
    rotation_quarters: u8,
    extra_vertical_flip: bool,
}

#[cfg(unix)]
impl Default for DmabufViewTransform {
    fn default() -> Self {
        // Start with a rotated + flipped-friendly orientation for the current
        // Linux/GTK DMABUF path; the runtime shortcuts can still override it.
        Self {
            rotation_quarters: 0,
            extra_vertical_flip: true,
        }
    }
}

#[cfg(unix)]
impl DmabufViewTransform {
    pub(super) fn rotate_clockwise(&mut self) {
        self.rotation_quarters = (self.rotation_quarters + 1) % 4;
    }

    pub(super) fn toggle_vertical_flip(&mut self) {
        self.extra_vertical_flip = !self.extra_vertical_flip;
    }

    pub(super) fn reset(&mut self) {
        *self = Self::default();
    }

    fn rotation_degrees(self) -> u16 {
        u16::from(self.rotation_quarters) * 90
    }

    pub(super) fn describe(self) -> String {
        format!(
            "DMABUF transform: rotate={} extra-flip-y={}",
            self.rotation_degrees(),
            if self.extra_vertical_flip {
                "on"
            } else {
                "off"
            }
        )
    }
}

#[cfg(unix)]
pub(super) struct DmabufFrame {
    pub(super) fds: Vec<OwnedFd>,
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) offset: [u32; 4],
    pub(super) stride: [u32; 4],
    pub(super) fourcc: u32,
    pub(super) modifier: u64,
    pub(super) y0_top: bool,
    pub(super) num_planes: u32,
    /// Sub-rectangle of the (`width` x `height`) texture this console shows.
    pub(super) view: DmabufView,
}

#[cfg(unix)]
impl DmabufFrame {
    pub(super) fn try_from_scanout(scanout: ScanoutDMABUF) -> Result<Self> {
        let width = scanout.width;
        let height = scanout.height;
        let offset = scanout.offset;
        let stride = scanout.stride;
        let fourcc = scanout.fourcc;
        let modifier = scanout.modifier;
        let y0_top = scanout.y0_top;
        let num_planes = scanout.num_planes;

        Self::try_from_raw_parts(
            scanout.into_raw_fds(),
            width,
            height,
            offset,
            stride,
            fourcc,
            modifier,
            y0_top,
            num_planes,
            DmabufView::full(width, height),
        )
    }

    /// `width`/`height` are the backing buffer's dimensions; `view` selects the
    /// part of it this console scans out (clamped to the buffer).
    #[allow(clippy::too_many_arguments)]
    pub(super) fn try_from_raw_parts(
        raw_fds: [i32; 4],
        width: u32,
        height: u32,
        offset: [u32; 4],
        stride: [u32; 4],
        fourcc: u32,
        modifier: u64,
        y0_top: bool,
        num_planes: u32,
        view: DmabufView,
    ) -> Result<Self> {
        let plane_count = usize::try_from(num_planes).context("invalid DMABUF plane count")?;
        if plane_count == 0 || plane_count > 4 {
            bail!("DMABUF plane count {} is not supported", num_planes);
        }

        let mut fds = Vec::with_capacity(plane_count);
        for (index, raw_fd) in raw_fds.into_iter().take(plane_count).enumerate() {
            if raw_fd < 0 {
                bail!("DMABUF plane {index} did not provide a valid file descriptor");
            }

            // SAFETY: QEMU passed ownership of the duplicated DMABUF FDs to us.
            fds.push(unsafe { OwnedFd::from_raw_fd(raw_fd) });
        }

        Ok(Self {
            fds,
            width,
            height,
            offset,
            stride,
            fourcc,
            modifier,
            y0_top,
            num_planes,
            view: view.clamped(width, height),
        })
    }
}

#[cfg(unix)]
pub(super) struct DmabufPresenter {
    presentation: DmabufPresentation,
    paintable: DmabufPaintable,
    transform: DmabufViewTransform,
}

#[cfg(unix)]
#[derive(Clone)]
struct PaintableState {
    texture: Option<gdk::Texture>,
    width: u32,
    height: u32,
    view: DmabufView,
    y0_top: bool,
    transform: DmabufViewTransform,
}

#[cfg(unix)]
impl Default for PaintableState {
    fn default() -> Self {
        Self {
            texture: None,
            width: 0,
            height: 0,
            view: DmabufView::full(0, 0),
            y0_top: true,
            transform: DmabufViewTransform::default(),
        }
    }
}

#[cfg(unix)]
impl PaintableState {
    fn update_from_presentation(
        &mut self,
        presentation: &DmabufPresentation,
        transform: DmabufViewTransform,
    ) -> bool {
        let size_changed = self.view.width != presentation.view.width
            || self.view.height != presentation.view.height;
        self.texture = Some(presentation.texture.clone());
        self.width = presentation.width;
        self.height = presentation.height;
        self.view = presentation.view;
        self.y0_top = presentation.y0_top;
        self.transform = transform;
        size_changed
    }

    fn intrinsic_width(&self) -> i32 {
        i32::try_from(self.view.width).unwrap_or(i32::MAX)
    }

    fn intrinsic_height(&self) -> i32 {
        i32::try_from(self.view.height).unwrap_or(i32::MAX)
    }

    fn intrinsic_aspect_ratio(&self) -> f64 {
        if self.view.height == 0 {
            0.0
        } else {
            f64::from(self.view.width) / f64::from(self.view.height)
        }
    }

    fn snapshot(&self, snapshot: &gdk::Snapshot, width: f64, height: f64) {
        if let Some(node) = self.render_node(width as f32, height as f32) {
            snapshot.append_node(&node);
        }
    }

    fn render_node(&self, width: f32, height: f32) -> Option<gsk::RenderNode> {
        let texture = self.texture.as_ref()?;
        if !width.is_finite()
            || !height.is_finite()
            || width <= 0.0
            || height <= 0.0
            || self.view.width == 0
            || self.view.height == 0
        {
            return None;
        }
        let rotation = self.transform.rotation_quarters % 4;
        let (view_width, view_height) = if rotation % 2 == 0 {
            (width, height)
        } else {
            (height, width)
        };
        let (full, shift) =
            view_placement(self.view, self.width, self.height, view_width, view_height);
        let bounds = gtk::graphene::Rect::new(0.0, 0.0, view_width, view_height);
        let texture_bounds =
            gtk::graphene::Rect::new(shift.x(), shift.y(), full.width(), full.height());
        let texture_node = gsk::TextureNode::new(texture, &texture_bounds);
        // Crop first. Flipping the backing buffer before cropping selects a
        // different head when its view is not vertically centered.
        let mut node: gsk::RenderNode = gsk::ClipNode::new(&texture_node, &bounds).upcast();
        if dmabuf_needs_vertical_flip(self.y0_top, self.transform) {
            let flip = gsk::Transform::new()
                .translate(&gtk::graphene::Point::new(0.0, view_height))
                .scale(1.0, -1.0);
            node = gsk::TransformNode::new(&node, &flip).upcast();
        }
        let transform = match rotation {
            0 => return Some(node),
            1 => gsk::Transform::new()
                .translate(&gtk::graphene::Point::new(width, 0.0))
                .rotate(90.0),
            2 => gsk::Transform::new()
                .translate(&gtk::graphene::Point::new(width, height))
                .rotate(180.0),
            3 => gsk::Transform::new()
                .translate(&gtk::graphene::Point::new(0.0, height))
                .rotate(270.0),
            _ => unreachable!(),
        };
        Some(gsk::TransformNode::new(&node, &transform).upcast())
    }
}

/// Where the full backing texture goes so that `view` lands exactly on a
/// `dest_width` x `dest_height` destination: the scaled backing rectangle and
/// the translation to apply before drawing it.
#[cfg(unix)]
fn view_placement(
    view: DmabufView,
    backing_width: u32,
    backing_height: u32,
    dest_width: f32,
    dest_height: f32,
) -> (gtk::graphene::Rect, gtk::graphene::Point) {
    let (scale_x, scale_y) = view_scale(view, dest_width, dest_height);
    let full = gtk::graphene::Rect::new(
        0.0,
        0.0,
        backing_width as f32 * scale_x,
        backing_height as f32 * scale_y,
    );
    let shift = gtk::graphene::Point::new(-(view.x as f32) * scale_x, -(view.y as f32) * scale_y);
    (full, shift)
}

fn view_scale(view: DmabufView, dest_width: f32, dest_height: f32) -> (f32, f32) {
    let scale_x = if view.width == 0 {
        1.0
    } else {
        dest_width / view.width as f32
    };
    let scale_y = if view.height == 0 {
        1.0
    } else {
        dest_height / view.height as f32
    };
    (scale_x, scale_y)
}

/// Console-relative damage rectangles arrive in view coordinates; the GTK
/// update region is in texture coordinates.
fn shift_update_into_backing(update: UpdateDMABUF, view: DmabufView) -> UpdateDMABUF {
    UpdateDMABUF {
        x: update
            .x
            .saturating_add(i32::try_from(view.x).unwrap_or(i32::MAX)),
        y: update
            .y
            .saturating_add(i32::try_from(view.y).unwrap_or(i32::MAX)),
        w: update.w,
        h: update.h,
    }
}

#[cfg(unix)]
mod paintable_imp {
    use super::*;

    #[derive(Default)]
    pub struct DmabufPaintable {
        pub(super) state: RefCell<PaintableState>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for DmabufPaintable {
        const NAME: &'static str = "Qd2DmabufPaintable";
        type Type = super::DmabufPaintable;
        type Interfaces = (gdk::Paintable,);
    }

    impl ObjectImpl for DmabufPaintable {}

    impl PaintableImpl for DmabufPaintable {
        fn current_image(&self) -> gdk::Paintable {
            self.obj().clone().upcast()
        }

        fn flags(&self) -> gdk::PaintableFlags {
            gdk::PaintableFlags::empty()
        }

        fn intrinsic_width(&self) -> i32 {
            self.state.borrow().intrinsic_width()
        }

        fn intrinsic_height(&self) -> i32 {
            self.state.borrow().intrinsic_height()
        }

        fn intrinsic_aspect_ratio(&self) -> f64 {
            self.state.borrow().intrinsic_aspect_ratio()
        }

        fn snapshot(&self, snapshot: &gdk::Snapshot, width: f64, height: f64) {
            self.state.borrow().snapshot(snapshot, width, height);
        }
    }
}

#[cfg(unix)]
glib::wrapper! {
    pub struct DmabufPaintable(ObjectSubclass<paintable_imp::DmabufPaintable>)
        @implements gdk::Paintable;
}

#[cfg(unix)]
impl DmabufPaintable {
    fn new(presentation: &DmabufPresentation, transform: DmabufViewTransform) -> Self {
        let paintable: Self = glib::Object::new();
        paintable.update_from_presentation(presentation, transform);
        paintable
    }

    fn update_from_presentation(
        &self,
        presentation: &DmabufPresentation,
        transform: DmabufViewTransform,
    ) {
        let size_changed = self
            .imp()
            .state
            .borrow_mut()
            .update_from_presentation(presentation, transform);

        if size_changed {
            self.invalidate_size();
        }
        self.invalidate_contents();
    }
}

#[cfg(target_os = "linux")]
pub(super) fn build_dmabuf_presenter(
    display: &gdk::Display,
    scanout: DmabufFrame,
    transform: DmabufViewTransform,
) -> Result<DmabufPresenter> {
    DmabufPresenter::new(display, scanout, transform)
}

#[cfg(all(unix, not(target_os = "linux")))]
pub(super) fn build_dmabuf_presenter(
    _display: &gdk::Display,
    _scanout: DmabufFrame,
    _transform: DmabufViewTransform,
) -> Result<DmabufPresenter> {
    bail!("DMABUF import is currently supported only on Linux GTK builds")
}

#[cfg(unix)]
impl DmabufPresentation {
    fn new(display: &gdk::Display, scanout: DmabufFrame) -> Result<Self> {
        let texture = build_dmabuf_texture(
            display,
            scanout.width,
            scanout.height,
            &scanout.fds,
            &scanout.offset,
            &scanout.stride,
            scanout.fourcc,
            scanout.modifier,
            scanout.num_planes,
            None,
            None,
        )?;

        Ok(Self {
            texture,
            fds: scanout.fds,
            width: scanout.width,
            height: scanout.height,
            view: scanout.view,
            offset: scanout.offset,
            stride: scanout.stride,
            fourcc: scanout.fourcc,
            modifier: scanout.modifier,
            y0_top: scanout.y0_top,
            num_planes: scanout.num_planes,
        })
    }

    pub(super) fn refresh(
        &mut self,
        display: &gdk::Display,
        updates: &[UpdateDMABUF],
        partial_updates: bool,
    ) -> Result<()> {
        let update_region = partial_updates
            .then(|| {
                let shifted = updates
                    .iter()
                    .map(|update| shift_update_into_backing(*update, self.view))
                    .collect::<Vec<_>>();
                dmabuf_update_region(&shifted, self.width, self.height)
            })
            .flatten();
        let previous_texture = partial_updates.then(|| self.texture.clone());

        self.texture = build_dmabuf_texture(
            display,
            self.width,
            self.height,
            &self.fds,
            &self.offset,
            &self.stride,
            self.fourcc,
            self.modifier,
            self.num_planes,
            update_region.as_ref(),
            previous_texture.as_ref(),
        )?;

        Ok(())
    }
}

#[cfg(unix)]
impl DmabufPresenter {
    fn new(
        display: &gdk::Display,
        scanout: DmabufFrame,
        transform: DmabufViewTransform,
    ) -> Result<Self> {
        let presentation = DmabufPresentation::new(display, scanout)?;
        let paintable = DmabufPaintable::new(&presentation, transform);

        Ok(Self {
            presentation,
            paintable,
            transform,
        })
    }

    pub(super) fn refresh(
        &mut self,
        display: &gdk::Display,
        updates: &[UpdateDMABUF],
        partial_updates: bool,
    ) -> Result<()> {
        self.presentation
            .refresh(display, updates, partial_updates)?;
        self.paintable
            .update_from_presentation(&self.presentation, self.transform);
        Ok(())
    }

    pub(super) fn set_transform(&mut self, transform: DmabufViewTransform) {
        self.transform = transform;
        self.paintable
            .update_from_presentation(&self.presentation, self.transform);
    }

    fn paintable(&self) -> &gdk::Paintable {
        self.paintable.upcast_ref()
    }

    /// Size of what the console shows (the view), not of the backing texture.
    fn width(&self) -> u32 {
        self.presentation.view.width
    }

    fn height(&self) -> u32 {
        self.presentation.view.height
    }
}

#[cfg(unix)]
pub(super) fn present_dmabuf_presenter(
    picture: &gtk::Picture,
    status_label: &gtk::Label,
    ui_state: &Rc<RefCell<UiState>>,
    window: &gtk::Window,
    window_base_title: &str,
    presenter: &DmabufPresenter,
) {
    if picture.paintable().as_ref() != Some(presenter.paintable()) {
        picture.set_paintable(Some(presenter.paintable()));
    }
    super::present_paintable(
        picture,
        status_label,
        ui_state,
        window,
        window_base_title,
        presenter.paintable(),
        presenter.width(),
        presenter.height(),
    );
}

#[cfg(unix)]
fn dmabuf_needs_vertical_flip(y0_top: bool, transform: DmabufViewTransform) -> bool {
    !y0_top ^ transform.extra_vertical_flip
}

#[cfg(target_os = "linux")]
fn build_dmabuf_texture(
    display: &gdk::Display,
    width: u32,
    height: u32,
    fds: &[OwnedFd],
    offset: &[u32; 4],
    stride: &[u32; 4],
    fourcc: u32,
    modifier: u64,
    num_planes: u32,
    update_region: Option<&cairo::Region>,
    update_texture: Option<&gdk::Texture>,
) -> Result<gdk::Texture> {
    if !display.dmabuf_formats().contains(fourcc, modifier) {
        bail!(
            "GTK does not support DMABUF fourcc {:#x} with modifier {:#x}",
            fourcc,
            modifier
        );
    }

    let plane_count = usize::try_from(num_planes).context("invalid DMABUF plane count")?;
    if plane_count != fds.len() {
        bail!(
            "DMABUF reported {} planes but provided {} file descriptors",
            num_planes,
            fds.len()
        );
    }

    let mut duplicated_fds = Vec::with_capacity(plane_count);
    for fd in fds.iter().take(plane_count) {
        duplicated_fds.push(
            fd.as_fd()
                .try_clone_to_owned()
                .context("failed to duplicate the DMABUF plane file descriptor")?,
        );
    }

    let mut builder = gdk::DmabufTextureBuilder::new()
        .set_display(display)
        .set_width(width)
        .set_height(height)
        .set_fourcc(fourcc)
        .set_modifier(modifier)
        .set_n_planes(num_planes);

    if let Some(region) = update_region {
        builder = builder.set_update_region(Some(region));
    }
    if let Some(texture) = update_texture {
        builder = builder.set_update_texture(Some(texture));
    }

    for plane in 0..plane_count {
        builder = builder
            .set_offset(plane as u32, offset[plane])
            .set_stride(plane as u32, stride[plane]);

        // SAFETY: the duplicated OwnedFds stay alive until GTK releases the imported texture.
        builder = unsafe { builder.set_fd(plane as u32, duplicated_fds[plane].as_raw_fd()) };
    }

    let texture = unsafe { builder.build_with_release_func(move || drop(duplicated_fds)) }
        .context("GTK rejected the DMABUF scanout")?;

    Ok(texture)
}

#[cfg(unix)]
fn dmabuf_update_region(
    updates: &[UpdateDMABUF],
    width: u32,
    height: u32,
) -> Option<cairo::Region> {
    let rectangles = updates
        .iter()
        .filter_map(|update| dmabuf_update_rectangle(*update, width, height))
        .collect::<Vec<_>>();

    if rectangles.is_empty() {
        None
    } else {
        Some(cairo::Region::create_rectangles(&rectangles))
    }
}

#[cfg(unix)]
pub(super) fn dmabuf_update_rectangle(
    update: UpdateDMABUF,
    width: u32,
    height: u32,
) -> Option<cairo::RectangleInt> {
    if update.w <= 0 || update.h <= 0 {
        return None;
    }

    let x0 = update.x.clamp(0, i32::try_from(width).unwrap_or(i32::MAX));
    let y0 = update.y.clamp(0, i32::try_from(height).unwrap_or(i32::MAX));
    let x1 = (i64::from(update.x) + i64::from(update.w)).clamp(0, i64::from(width)) as i32;
    let y1 = (i64::from(update.y) + i64::from(update.h)).clamp(0, i64::from(height)) as i32;

    if x1 <= x0 || y1 <= y0 {
        return None;
    }

    Some(cairo::RectangleInt::new(x0, y0, x1 - x0, y1 - y0))
}

#[cfg(test)]
mod view_tests {
    use super::{DmabufView, scanout_geometry, shift_update_into_backing, view_scale};
    use qemu_display::UpdateDMABUF;

    #[test]
    fn full_view_is_identity() {
        let view = DmabufView::full(2560, 1440);
        assert_eq!(view_scale(view, 2560.0, 1440.0), (1.0, 1.0));
        assert_eq!(view_scale(view, 1280.0, 720.0), (0.5, 0.5));
        let update = UpdateDMABUF {
            x: 10,
            y: 20,
            w: 30,
            h: 40,
        };
        let shifted = shift_update_into_backing(update, view);
        assert_eq!((shifted.x, shifted.y), (10, 20));
    }

    #[test]
    fn second_head_of_an_x11_screen() {
        // 4480x1440 X screen: head 1 shows 1920x1080 at (2560, 360).
        let view = DmabufView {
            x: 2560,
            y: 360,
            width: 1920,
            height: 1080,
        }
        .clamped(4480, 1440);
        assert_eq!(
            (view.x, view.y, view.width, view.height),
            (2560, 360, 1920, 1080)
        );
        let shifted = shift_update_into_backing(
            UpdateDMABUF {
                x: 0,
                y: 0,
                w: 1920,
                h: 1080,
            },
            view,
        );
        assert_eq!((shifted.x, shifted.y), (2560, 360));
    }

    #[test]
    fn nonsense_views_fall_back_to_the_whole_buffer() {
        let full = DmabufView::full(4480, 1440);
        assert_eq!(
            DmabufView {
                x: 5000,
                y: 0,
                width: 10,
                height: 10
            }
            .clamped(4480, 1440),
            full
        );
        assert_eq!(
            DmabufView {
                x: 0,
                y: 0,
                width: 0,
                height: 10
            }
            .clamped(4480, 1440),
            full
        );
        let oversize = DmabufView {
            x: 2560,
            y: 360,
            width: 5000,
            height: 5000,
        }
        .clamped(4480, 1440);
        assert_eq!((oversize.width, oversize.height), (1920, 1080));
    }

    #[test]
    fn legacy_scanouts_ignore_offsets_without_backing_dimensions() {
        assert_eq!(
            scanout_geometry(100, 50, 640, 480, 0, 0),
            (640, 480, DmabufView::full(640, 480))
        );
        assert_eq!(
            scanout_geometry(100, 50, 640, 480, 1024, 0),
            (640, 480, DmabufView::full(640, 480))
        );
        assert_eq!(
            scanout_geometry(2560, 360, 1920, 1080, 4480, 1440),
            (
                4480,
                1440,
                DmabufView {
                    x: 2560,
                    y: 360,
                    width: 1920,
                    height: 1080
                },
            )
        );
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "requires GTK initialization; can run with the Broadway backend"]
    fn rendered_pixels_stay_in_the_selected_view_for_every_flip_and_rotation() {
        use super::{DmabufViewTransform, PaintableState};
        use gtk::{cairo, gdk, glib, prelude::*};
        use gtk4 as gtk;

        gtk::init().expect("GTK display required");

        fn pixel(x: u32, y: u32) -> [u8; 4] {
            [(x * 24) as u8, (y * 24) as u8, 64, 255]
        }

        let bytes: Vec<_> = (0..8)
            .flat_map(|y| (0..8).flat_map(move |x| pixel(x, y)))
            .collect();
        let texture = gdk::MemoryTexture::new(
            8,
            8,
            gdk::MemoryFormat::R8g8b8a8,
            &glib::Bytes::from_owned(bytes),
            8 * 4,
        )
        .upcast::<gdk::Texture>();
        // Full-buffer, top-half and offset views all use the production render path.
        for view in [
            DmabufView::full(8, 8),
            DmabufView::full(8, 4),
            DmabufView {
                x: 2,
                y: 1,
                width: 3,
                height: 2,
            },
        ] {
            for y0_top in [true, false] {
                for extra_vertical_flip in [false, true] {
                    for rotation_quarters in 0..4 {
                        let (width, height) = if rotation_quarters % 2 == 0 {
                            (view.width, view.height)
                        } else {
                            (view.height, view.width)
                        };
                        let state = PaintableState {
                            texture: Some(texture.clone()),
                            width: 8,
                            height: 8,
                            view,
                            y0_top,
                            transform: DmabufViewTransform {
                                rotation_quarters,
                                extra_vertical_flip,
                            },
                        };
                        let node = state.render_node(width as f32, height as f32).unwrap();
                        let mut image = cairo::ImageSurface::create(
                            cairo::Format::ARgb32,
                            width as i32,
                            height as i32,
                        )
                        .unwrap();
                        {
                            let cr = cairo::Context::new(&image).unwrap();
                            node.draw(&cr);
                        }
                        image.flush();
                        let stride = image.stride() as usize;
                        let data = image.data().unwrap();
                        for y in 0..view.height {
                            for x in 0..view.width {
                                let flipped_y = if !y0_top ^ extra_vertical_flip {
                                    view.height - 1 - y
                                } else {
                                    y
                                };
                                let (dx, dy) = match rotation_quarters {
                                    0 => (x, flipped_y),
                                    1 => (view.height - 1 - flipped_y, x),
                                    2 => (view.width - 1 - x, view.height - 1 - flipped_y),
                                    3 => (flipped_y, view.width - 1 - x),
                                    _ => unreachable!(),
                                };
                                let offset = dy as usize * stride + dx as usize * 4;
                                let actual = u32::from_ne_bytes(
                                    data[offset..offset + 4].try_into().unwrap(),
                                );
                                let [r, g, b, _] = pixel(view.x + x, view.y + y);
                                let expected =
                                    0xff000000 | (r as u32) << 16 | (g as u32) << 8 | b as u32;
                                assert_eq!(
                                    actual, expected,
                                    "view={view:?} y0_top={y0_top} flip={extra_vertical_flip} rotation={rotation_quarters} pixel=({dx},{dy})"
                                );
                            }
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use super::DmabufViewTransform;

    #[cfg(unix)]
    #[test]
    fn dmabuf_transform_describe_handles_all_quarter_turns() {
        let mut transform = DmabufViewTransform::default();

        assert!(transform.describe().contains("rotate=0"));

        transform.rotate_clockwise();
        assert!(transform.describe().contains("rotate=90"));

        transform.rotate_clockwise();
        assert!(transform.describe().contains("rotate=180"));

        transform.rotate_clockwise();
        assert!(transform.describe().contains("rotate=270"));

        transform.rotate_clockwise();
        assert!(transform.describe().contains("rotate=0"));
    }
}
