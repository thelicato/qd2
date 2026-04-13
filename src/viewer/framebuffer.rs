use std::{cell::RefCell, convert::TryFrom};

use anyhow::{Context, Result, bail};
use gtk::{cairo, gdk, glib, prelude::*};
use gtk4 as gtk;
use qemu_display::{Scanout, Update};
#[cfg(unix)]
use qemu_display::{ScanoutDMABUF, ScanoutMap, ScanoutMmap, UpdateDMABUF, UpdateMap};

use gdk::subclass::prelude::*;

use super::{ViewerEvent, cursor::GuestCursor, dmabuf::DmabufFrame, events::EventSender};

const PIXMAN_A8B8G8R8: u32 = pixman_sys::pixman_format_code_t_PIXMAN_a8b8g8r8;
const PIXMAN_A8R8G8B8: u32 = pixman_sys::pixman_format_code_t_PIXMAN_a8r8g8b8;
const PIXMAN_B8G8R8A8: u32 = pixman_sys::pixman_format_code_t_PIXMAN_b8g8r8a8;
const PIXMAN_B8G8R8X8: u32 = pixman_sys::pixman_format_code_t_PIXMAN_b8g8r8x8;
const PIXMAN_R8G8B8A8: u32 = pixman_sys::pixman_format_code_t_PIXMAN_r8g8b8a8;
const PIXMAN_R8G8B8X8: u32 = pixman_sys::pixman_format_code_t_PIXMAN_r8g8b8x8;
const PIXMAN_X8B8G8R8: u32 = pixman_sys::pixman_format_code_t_PIXMAN_x8b8g8r8;
const PIXMAN_X8R8G8B8: u32 = pixman_sys::pixman_format_code_t_PIXMAN_x8r8g8b8;
const RGBA_BYTES_PER_PIXEL: usize = 4;

#[derive(Clone)]
pub(super) struct FrameSnapshot {
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) stride: usize,
    pub(super) data: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct FramePatch {
    pub(super) x: u32,
    pub(super) y: u32,
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) stride: usize,
    pub(super) data: Vec<u8>,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct FrameRect {
    x: usize,
    y: usize,
    width: usize,
    height: usize,
}

#[cfg(unix)]
struct MappedFramebuffer {
    mmap: ScanoutMmap,
}

pub(super) struct Framebuffer {
    width: u32,
    height: u32,
    source_stride: usize,
    format: PixelFormat,
    rgba: Vec<u8>,
    #[cfg(unix)]
    mapped: Option<MappedFramebuffer>,
}

impl Framebuffer {
    pub(super) fn from_scanout(scanout: Scanout) -> Result<Self> {
        let format = PixelFormat::try_from(scanout.format)?;
        let source_stride =
            usize::try_from(scanout.stride).context("invalid framebuffer stride")?;
        let expected_len = usize::try_from(scanout.height)
            .context("invalid framebuffer height")?
            .checked_mul(source_stride)
            .context("framebuffer size overflow")?;

        if scanout.data.len() < expected_len {
            bail!(
                "scanout payload is too short for {}x{} stride {}",
                scanout.width,
                scanout.height,
                scanout.stride
            );
        }

        let rgba_len = usize::try_from(scanout.width)
            .context("invalid framebuffer width")?
            .checked_mul(usize::try_from(scanout.height).context("invalid framebuffer height")?)
            .and_then(|pixels| pixels.checked_mul(RGBA_BYTES_PER_PIXEL))
            .context("RGBA framebuffer size overflow")?;
        let mut framebuffer = Self {
            width: scanout.width,
            height: scanout.height,
            source_stride,
            format,
            rgba: vec![0; rgba_len],
            #[cfg(unix)]
            mapped: None,
        };
        framebuffer.blit_frame_from_linear(&scanout.data)?;
        Ok(framebuffer)
    }

    #[cfg(unix)]
    fn from_map(scanout: ScanoutMap) -> Result<Self> {
        let format = PixelFormat::try_from(scanout.format)?;
        let source_stride =
            usize::try_from(scanout.stride).context("invalid mapped framebuffer stride")?;
        let expected_len = usize::try_from(scanout.height)
            .context("invalid mapped framebuffer height")?
            .checked_mul(source_stride)
            .context("mapped framebuffer size overflow")?;
        let rgba_len = usize::try_from(scanout.width)
            .context("invalid mapped framebuffer width")?
            .checked_mul(
                usize::try_from(scanout.height).context("invalid mapped framebuffer height")?,
            )
            .and_then(|pixels| pixels.checked_mul(RGBA_BYTES_PER_PIXEL))
            .context("mapped RGBA framebuffer size overflow")?;

        let width = scanout.width;
        let height = scanout.height;
        let mmap = scanout
            .mmap()
            .context("failed to map the shared framebuffer")?;

        if mmap.as_ref().len() < expected_len {
            bail!(
                "mapped framebuffer is too short for {}x{} stride {}",
                width,
                height,
                source_stride
            );
        }

        let mut framebuffer = Self {
            width,
            height,
            source_stride,
            format,
            rgba: vec![0; rgba_len],
            mapped: Some(MappedFramebuffer { mmap }),
        };
        framebuffer.refresh_full_mapped_frame()?;
        Ok(framebuffer)
    }

    fn apply_update(&mut self, update: Update) -> Result<FrameRect> {
        let update_format = PixelFormat::try_from(update.format)?;
        if update_format != self.format {
            bail!(
                "update format {:#x} does not match the current framebuffer format {:#x}",
                update.format,
                self.format.pixman_code()
            );
        }

        let rect = self.validate_rect(update.x, update.y, update.w, update.h)?;
        let src_stride = usize::try_from(update.stride).context("invalid update stride")?;
        self.blit_update_from_linear(rect, src_stride, &update.data)?;
        Ok(rect)
    }

    #[cfg(unix)]
    fn refresh_mapped_region(&mut self, update: UpdateMap) -> Result<FrameRect> {
        let rect = self.validate_rect(update.x, update.y, update.w, update.h)?;
        let mapped = self
            .mapped
            .as_ref()
            .context("received a shared-memory update without a shared-memory scanout")?;
        let format = self.format;
        let source_stride = self.source_stride;
        let rgba_stride = self.rgba_stride();
        let rgba = &mut self.rgba;

        Self::blit_update_from_frame_data(
            format,
            source_stride,
            rgba_stride,
            rect,
            mapped.mmap.as_ref(),
            rgba,
        )?;

        Ok(rect)
    }

    #[cfg(unix)]
    fn refresh_full_mapped_frame(&mut self) -> Result<()> {
        let rect = FrameRect {
            x: 0,
            y: 0,
            width: usize::try_from(self.width).context("invalid framebuffer width")?,
            height: usize::try_from(self.height).context("invalid framebuffer height")?,
        };
        let mapped = self
            .mapped
            .as_ref()
            .context("received a shared-memory scanout without a mapping")?;
        let format = self.format;
        let source_stride = self.source_stride;
        let rgba_stride = self.rgba_stride();
        let rgba = &mut self.rgba;

        Self::blit_update_from_frame_data(
            format,
            source_stride,
            rgba_stride,
            rect,
            mapped.mmap.as_ref(),
            rgba,
        )
    }

    pub(super) fn snapshot(&self) -> FrameSnapshot {
        FrameSnapshot {
            width: self.width,
            height: self.height,
            stride: self.rgba_stride(),
            data: self.rgba.clone(),
        }
    }

    fn patch(&self, rect: FrameRect) -> FramePatch {
        let stride = rect.width * RGBA_BYTES_PER_PIXEL;
        let mut data = vec![0; stride * rect.height];

        for row in 0..rect.height {
            let src_start = (rect.y + row) * self.rgba_stride() + rect.x * RGBA_BYTES_PER_PIXEL;
            let src_end = src_start + stride;
            let dst_start = row * stride;
            let dst_end = dst_start + stride;
            data[dst_start..dst_end].copy_from_slice(&self.rgba[src_start..src_end]);
        }

        FramePatch {
            x: rect.x as u32,
            y: rect.y as u32,
            width: rect.width as u32,
            height: rect.height as u32,
            stride,
            data,
        }
    }

    fn rgba_stride(&self) -> usize {
        usize::try_from(self.width).unwrap_or(0) * RGBA_BYTES_PER_PIXEL
    }

    fn validate_rect(&self, x: i32, y: i32, width: i32, height: i32) -> Result<FrameRect> {
        let x = usize::try_from(x).context("negative update x coordinate")?;
        let y = usize::try_from(y).context("negative update y coordinate")?;
        let width = usize::try_from(width).context("negative update width")?;
        let height = usize::try_from(height).context("negative update height")?;
        let framebuffer_width = usize::try_from(self.width).context("invalid framebuffer width")?;
        let framebuffer_height =
            usize::try_from(self.height).context("invalid framebuffer height")?;

        if x.checked_add(width).context("update width overflow")? > framebuffer_width
            || y.checked_add(height).context("update height overflow")? > framebuffer_height
        {
            bail!("update rectangle falls outside the framebuffer bounds");
        }

        Ok(FrameRect {
            x,
            y,
            width,
            height,
        })
    }

    fn blit_frame_from_linear(&mut self, data: &[u8]) -> Result<()> {
        let rect = FrameRect {
            x: 0,
            y: 0,
            width: usize::try_from(self.width).context("invalid framebuffer width")?,
            height: usize::try_from(self.height).context("invalid framebuffer height")?,
        };
        self.blit_update_from_linear(rect, self.source_stride, data)
    }

    fn blit_update_from_linear(
        &mut self,
        rect: FrameRect,
        src_stride: usize,
        data: &[u8],
    ) -> Result<()> {
        let row_len = rect
            .width
            .checked_mul(self.format.bytes_per_pixel())
            .context("update row size overflow")?;

        for row in 0..rect.height {
            let src_start = row
                .checked_mul(src_stride)
                .context("update source offset overflow")?;
            let src_end = src_start
                .checked_add(row_len)
                .context("update source range overflow")?;
            if src_end > data.len() {
                bail!("update payload is too short for the advertised rectangle");
            }

            let dst_start = rect
                .y
                .checked_add(row)
                .context("update destination y overflow")?
                .checked_mul(self.rgba_stride())
                .context("update destination row overflow")?
                .checked_add(
                    rect.x
                        .checked_mul(RGBA_BYTES_PER_PIXEL)
                        .context("update destination x overflow")?,
                )
                .context("update destination offset overflow")?;
            let dst_end = dst_start
                .checked_add(
                    rect.width
                        .checked_mul(RGBA_BYTES_PER_PIXEL)
                        .context("update destination width overflow")?,
                )
                .context("update destination range overflow")?;

            self.format.write_rgba_row(
                &data[src_start..src_end],
                &mut self.rgba[dst_start..dst_end],
            );
        }

        Ok(())
    }

    fn blit_update_from_frame_data(
        format: PixelFormat,
        source_stride: usize,
        rgba_stride: usize,
        rect: FrameRect,
        data: &[u8],
        rgba: &mut [u8],
    ) -> Result<()> {
        let row_len = rect
            .width
            .checked_mul(format.bytes_per_pixel())
            .context("mapped update row size overflow")?;

        for row in 0..rect.height {
            let src_start = rect
                .y
                .checked_add(row)
                .context("mapped update y overflow")?
                .checked_mul(source_stride)
                .context("mapped update row overflow")?
                .checked_add(
                    rect.x
                        .checked_mul(format.bytes_per_pixel())
                        .context("mapped update x overflow")?,
                )
                .context("mapped update source offset overflow")?;
            let src_end = src_start
                .checked_add(row_len)
                .context("mapped update source range overflow")?;
            if src_end > data.len() {
                bail!("mapped update rectangle falls outside the shared framebuffer");
            }

            let dst_start = rect
                .y
                .checked_add(row)
                .context("mapped update destination y overflow")?
                .checked_mul(rgba_stride)
                .context("mapped update destination row overflow")?
                .checked_add(
                    rect.x
                        .checked_mul(RGBA_BYTES_PER_PIXEL)
                        .context("mapped update destination x overflow")?,
                )
                .context("mapped update destination offset overflow")?;
            let dst_end = dst_start
                .checked_add(
                    rect.width
                        .checked_mul(RGBA_BYTES_PER_PIXEL)
                        .context("mapped update destination width overflow")?,
                )
                .context("mapped update destination range overflow")?;

            format.write_rgba_row(&data[src_start..src_end], &mut rgba[dst_start..dst_end]);
        }

        Ok(())
    }
}

pub(super) struct SoftwarePresenter {
    paintable: SoftwarePaintable,
}

#[derive(Default)]
struct SoftwarePaintableState {
    surface: Option<cairo::ImageSurface>,
    width: u32,
    height: u32,
}

impl SoftwarePaintableState {
    fn update_from_snapshot(&mut self, snapshot: &FrameSnapshot) -> Result<bool> {
        let size_changed = self.width != snapshot.width || self.height != snapshot.height;

        if size_changed || self.surface.is_none() {
            let width = i32::try_from(snapshot.width).context("invalid framebuffer width")?;
            let height = i32::try_from(snapshot.height).context("invalid framebuffer height")?;
            let stride = cairo::Format::ARgb32
                .stride_for_width(snapshot.width)
                .context("invalid cairo stride for the framebuffer")?;
            let surface = cairo::ImageSurface::create_for_data(
                vec![0; usize::try_from(stride).unwrap_or(0) * snapshot.height as usize],
                cairo::Format::ARgb32,
                width,
                height,
                stride,
            )
            .context("failed to allocate the software framebuffer surface")?;

            self.surface = Some(surface);
            self.width = snapshot.width;
            self.height = snapshot.height;
        }

        let patch = FramePatch {
            x: 0,
            y: 0,
            width: snapshot.width,
            height: snapshot.height,
            stride: snapshot.stride,
            data: snapshot.data.clone(),
        };
        self.apply_patch(&patch)?;
        Ok(size_changed)
    }

    fn apply_patch(&mut self, patch: &FramePatch) -> Result<()> {
        let surface = self
            .surface
            .as_mut()
            .context("software presenter patch arrived before the initial frame")?;
        let surface_width = self.width as usize;
        let surface_height = self.height as usize;
        let patch_x = usize::try_from(patch.x).context("invalid patch x")?;
        let patch_y = usize::try_from(patch.y).context("invalid patch y")?;
        let patch_width = usize::try_from(patch.width).context("invalid patch width")?;
        let patch_height = usize::try_from(patch.height).context("invalid patch height")?;

        if patch_x
            .checked_add(patch_width)
            .context("patch width overflow")?
            > surface_width
            || patch_y
                .checked_add(patch_height)
                .context("patch height overflow")?
                > surface_height
        {
            bail!("software framebuffer patch falls outside the presenter bounds");
        }

        {
            let dst_stride =
                usize::try_from(surface.stride()).context("invalid software framebuffer stride")?;
            let mut surface_data = surface
                .data()
                .context("failed to access the software framebuffer surface data")?;
            apply_rgba_patch_to_cairo_argb32(
                &mut surface_data,
                dst_stride,
                patch_x,
                patch_y,
                patch_width,
                patch_height,
                &patch.data,
                patch.stride,
            )?;
        }

        surface.mark_dirty_rectangle(
            patch.x as i32,
            patch.y as i32,
            patch.width as i32,
            patch.height as i32,
        );
        Ok(())
    }

    fn intrinsic_width(&self) -> i32 {
        i32::try_from(self.width).unwrap_or(i32::MAX)
    }

    fn intrinsic_height(&self) -> i32 {
        i32::try_from(self.height).unwrap_or(i32::MAX)
    }

    fn intrinsic_aspect_ratio(&self) -> f64 {
        if self.height == 0 {
            0.0
        } else {
            f64::from(self.width) / f64::from(self.height)
        }
    }

    fn snapshot(&self, snapshot: &gdk::Snapshot, width: f64, height: f64) {
        let Some(surface) = self.surface.as_ref().cloned() else {
            return;
        };
        if width <= 0.0 || height <= 0.0 || self.width == 0 || self.height == 0 {
            return;
        }

        let bounds = gtk::graphene::Rect::new(0.0, 0.0, width as f32, height as f32);
        let cr = snapshot.append_cairo(&bounds);
        cr.scale(
            width / f64::from(self.width),
            height / f64::from(self.height),
        );
        let _ = cr.set_source_surface(&surface, 0.0, 0.0);
        let _ = cr.paint();
    }
}

mod software_paintable_imp {
    use super::*;

    #[derive(Default)]
    pub struct SoftwarePaintable {
        pub(super) state: RefCell<SoftwarePaintableState>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for SoftwarePaintable {
        const NAME: &'static str = "Qd2SoftwarePaintable";
        type Type = super::SoftwarePaintable;
        type Interfaces = (gdk::Paintable,);
    }

    impl ObjectImpl for SoftwarePaintable {}

    impl PaintableImpl for SoftwarePaintable {
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

glib::wrapper! {
    pub struct SoftwarePaintable(ObjectSubclass<software_paintable_imp::SoftwarePaintable>)
        @implements gdk::Paintable;
}

impl SoftwarePaintable {
    fn new(snapshot: &FrameSnapshot) -> Result<Self> {
        let paintable: Self = glib::Object::new();
        paintable.update_from_snapshot(snapshot)?;
        Ok(paintable)
    }

    fn update_from_snapshot(&self, snapshot: &FrameSnapshot) -> Result<()> {
        let size_changed = self
            .imp()
            .state
            .borrow_mut()
            .update_from_snapshot(snapshot)?;

        if size_changed {
            self.invalidate_size();
        }
        self.invalidate_contents();
        Ok(())
    }

    fn apply_patch(&self, patch: &FramePatch) -> Result<()> {
        self.imp().state.borrow_mut().apply_patch(patch)?;
        self.invalidate_contents();
        Ok(())
    }
}

impl SoftwarePresenter {
    pub(super) fn new(snapshot: &FrameSnapshot) -> Result<Self> {
        Ok(Self {
            paintable: SoftwarePaintable::new(snapshot)?,
        })
    }

    pub(super) fn update_frame(&mut self, snapshot: &FrameSnapshot) -> Result<()> {
        self.paintable.update_from_snapshot(snapshot)
    }

    pub(super) fn apply_patch(&mut self, patch: &FramePatch) -> Result<()> {
        self.paintable.apply_patch(patch)
    }

    pub(super) fn paintable(&self) -> &gdk::Paintable {
        self.paintable.upcast_ref()
    }

    pub(super) fn width(&self) -> u32 {
        self.paintable.imp().state.borrow().width
    }

    pub(super) fn height(&self) -> u32 {
        self.paintable.imp().state.borrow().height
    }
}

fn apply_rgba_patch_to_cairo_argb32(
    dst: &mut [u8],
    dst_stride: usize,
    dst_x: usize,
    dst_y: usize,
    width: usize,
    height: usize,
    src: &[u8],
    src_stride: usize,
) -> Result<()> {
    let src_row_len = width
        .checked_mul(RGBA_BYTES_PER_PIXEL)
        .context("software patch row size overflow")?;

    for row in 0..height {
        let src_start = row
            .checked_mul(src_stride)
            .context("software patch source offset overflow")?;
        let src_end = src_start
            .checked_add(src_row_len)
            .context("software patch source range overflow")?;
        if src_end > src.len() {
            bail!("software patch payload is too short for the advertised rectangle");
        }

        let dst_start = (dst_y + row)
            .checked_mul(dst_stride)
            .context("software patch destination row overflow")?
            .checked_add(
                dst_x
                    .checked_mul(RGBA_BYTES_PER_PIXEL)
                    .context("software patch destination x overflow")?,
            )
            .context("software patch destination offset overflow")?;
        let dst_end = dst_start
            .checked_add(src_row_len)
            .context("software patch destination range overflow")?;
        if dst_end > dst.len() {
            bail!("software patch rectangle falls outside the presenter surface");
        }

        rgba_row_to_cairo_argb32(&src[src_start..src_end], &mut dst[dst_start..dst_end]);
    }

    Ok(())
}

fn rgba_row_to_cairo_argb32(src_row: &[u8], dst_row: &mut [u8]) {
    for (src, dst) in src_row
        .chunks_exact(RGBA_BYTES_PER_PIXEL)
        .zip(dst_row.chunks_exact_mut(RGBA_BYTES_PER_PIXEL))
    {
        let (r, g, b, a) = (src[0], src[1], src[2], src[3]);
        let pr = premultiply(r, a);
        let pg = premultiply(g, a);
        let pb = premultiply(b, a);

        #[cfg(target_endian = "little")]
        {
            dst.copy_from_slice(&[pb, pg, pr, a]);
        }

        #[cfg(target_endian = "big")]
        {
            dst.copy_from_slice(&[a, pr, pg, pb]);
        }
    }
}

fn premultiply(channel: u8, alpha: u8) -> u8 {
    ((u16::from(channel) * u16::from(alpha) + 127) / 255) as u8
}

pub(super) struct FrameStreamHandler {
    event_tx: EventSender,
    framebuffer: Option<Framebuffer>,
}

impl FrameStreamHandler {
    pub(super) fn new(event_tx: EventSender) -> Self {
        Self {
            event_tx,
            framebuffer: None,
        }
    }

    pub(super) fn send_status(&self, message: impl Into<String>) {
        let _ = self.event_tx.send(ViewerEvent::Status(message.into()));
    }

    fn send_current_frame(&self) {
        if let Some(framebuffer) = &self.framebuffer {
            let _ = self
                .event_tx
                .send(ViewerEvent::Frame(framebuffer.snapshot()));
        }
    }

    fn send_frame_patch(&self, rect: FrameRect) {
        if let Some(framebuffer) = &self.framebuffer {
            let _ = self
                .event_tx
                .send(ViewerEvent::FramePatch(framebuffer.patch(rect)));
        }
    }

    pub(super) fn scanout(&mut self, scanout: Scanout) {
        match Framebuffer::from_scanout(scanout) {
            Ok(framebuffer) => {
                self.framebuffer = Some(framebuffer);
                self.send_current_frame();
            }
            Err(error) => self.send_status(format!("Unsupported framebuffer: {error:#}")),
        }
    }

    pub(super) fn update(&mut self, update: Update) {
        let Some(framebuffer) = &mut self.framebuffer else {
            return;
        };

        match framebuffer.apply_update(update) {
            Ok(rect) => self.send_frame_patch(rect),
            Err(error) => {
                self.send_status(format!("Failed to apply framebuffer update: {error:#}"))
            }
        }
    }

    #[cfg(unix)]
    pub(super) fn scanout_map(&mut self, scanout: ScanoutMap) {
        match Framebuffer::from_map(scanout) {
            Ok(framebuffer) => {
                self.framebuffer = Some(framebuffer);
                self.send_current_frame();
            }
            Err(error) => {
                self.send_status(format!("Could not map the shared framebuffer: {error:#}"))
            }
        }
    }

    #[cfg(unix)]
    pub(super) fn update_map(&mut self, update: UpdateMap) {
        let Some(framebuffer) = &mut self.framebuffer else {
            return;
        };

        match framebuffer.refresh_mapped_region(update) {
            Ok(rect) => self.send_frame_patch(rect),
            Err(error) => self.send_status(format!(
                "Failed to refresh the shared framebuffer update: {error:#}"
            )),
        }
    }

    #[cfg(unix)]
    pub(super) fn scanout_dmabuf(&mut self, scanout: ScanoutDMABUF) {
        match DmabufFrame::try_from_scanout(scanout) {
            Ok(scanout) => self.emit_dmabuf_scanout(scanout),
            Err(error) => self.send_status(format!("Unsupported DMABUF scanout: {error:#}")),
        }
    }

    #[cfg(unix)]
    pub(super) fn emit_dmabuf_scanout(&mut self, scanout: DmabufFrame) {
        self.framebuffer = None;
        let _ = self.event_tx.send(ViewerEvent::Dmabuf(scanout));
    }

    #[cfg(unix)]
    pub(super) fn update_dmabuf(&mut self, update: UpdateDMABUF) {
        let _ = self.event_tx.send(ViewerEvent::DmabufUpdate(update));
    }

    pub(super) fn disable(&mut self) {
        self.framebuffer = None;
        self.send_status("The guest display was disabled.");
    }

    pub(super) fn mouse_set(&mut self, set: qemu_display::MouseSet) {
        // The host cursor already tracks the local pointer position, so the
        // useful part of MouseSet for this viewer is whether the guest wants
        // the cursor visible at all.
        let _ = self
            .event_tx
            .send(ViewerEvent::CursorVisibilityChanged(set.on != 0));
    }

    pub(super) fn cursor_define(&mut self, cursor: qemu_display::Cursor) {
        match GuestCursor::from_qemu(cursor) {
            Ok(shape) => {
                let _ = self.event_tx.send(ViewerEvent::CursorShapeChanged(shape));
            }
            Err(error) => self.send_status(format!("Could not decode guest cursor: {error:#}")),
        }
    }

    pub(super) fn disconnected(&mut self) {
        let _ = self.event_tx.send(ViewerEvent::Disconnected);
    }

    pub(super) fn interfaces(&self) -> Vec<String> {
        #[cfg(unix)]
        {
            return vec![
                "org.qemu.Display1.Listener.Unix.Map".to_owned(),
                "org.qemu.Display1.Listener.Unix.ScanoutDMABUF2".to_owned(),
            ];
        }

        #[cfg(not(unix))]
        Vec::new()
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(super) enum PixelFormat {
    A8b8g8r8,
    A8r8g8b8,
    B8g8r8a8,
    B8g8r8x8,
    R8g8b8a8,
    R8g8b8x8,
    X8b8g8r8,
    X8r8g8b8,
}

impl PixelFormat {
    fn bytes_per_pixel(self) -> usize {
        RGBA_BYTES_PER_PIXEL
    }

    pub(super) fn pixman_code(self) -> u32 {
        match self {
            Self::A8b8g8r8 => PIXMAN_A8B8G8R8,
            Self::A8r8g8b8 => PIXMAN_A8R8G8B8,
            Self::B8g8r8a8 => PIXMAN_B8G8R8A8,
            Self::B8g8r8x8 => PIXMAN_B8G8R8X8,
            Self::R8g8b8a8 => PIXMAN_R8G8B8A8,
            Self::R8g8b8x8 => PIXMAN_R8G8B8X8,
            Self::X8b8g8r8 => PIXMAN_X8B8G8R8,
            Self::X8r8g8b8 => PIXMAN_X8R8G8B8,
        }
    }

    fn write_rgba_row(self, src_row: &[u8], dst_row: &mut [u8]) {
        for (src, dst) in src_row
            .chunks_exact(self.bytes_per_pixel())
            .zip(dst_row.chunks_exact_mut(RGBA_BYTES_PER_PIXEL))
        {
            dst.copy_from_slice(&self.pixel_to_rgba(src));
        }
    }

    fn pixel_to_rgba(self, src: &[u8]) -> [u8; RGBA_BYTES_PER_PIXEL] {
        debug_assert_eq!(src.len(), self.bytes_per_pixel());

        #[cfg(target_endian = "little")]
        {
            match self {
                Self::A8b8g8r8 => [src[0], src[1], src[2], src[3]],
                Self::A8r8g8b8 => [src[2], src[1], src[0], src[3]],
                Self::B8g8r8a8 => [src[1], src[2], src[3], src[0]],
                Self::B8g8r8x8 => [src[1], src[2], src[3], u8::MAX],
                Self::R8g8b8a8 => [src[3], src[2], src[1], src[0]],
                Self::R8g8b8x8 => [src[3], src[2], src[1], u8::MAX],
                Self::X8b8g8r8 => [src[0], src[1], src[2], u8::MAX],
                Self::X8r8g8b8 => [src[2], src[1], src[0], u8::MAX],
            }
        }

        #[cfg(target_endian = "big")]
        {
            match self {
                Self::A8b8g8r8 => [src[3], src[2], src[1], src[0]],
                Self::A8r8g8b8 => [src[1], src[2], src[3], src[0]],
                Self::B8g8r8a8 => [src[2], src[1], src[0], src[3]],
                Self::B8g8r8x8 => [src[2], src[1], src[0], u8::MAX],
                Self::R8g8b8a8 => [src[0], src[1], src[2], src[3]],
                Self::R8g8b8x8 => [src[0], src[1], src[2], u8::MAX],
                Self::X8b8g8r8 => [src[3], src[2], src[1], u8::MAX],
                Self::X8r8g8b8 => [src[1], src[2], src[3], u8::MAX],
            }
        }
    }
}

impl TryFrom<u32> for PixelFormat {
    type Error = anyhow::Error;

    fn try_from(value: u32) -> Result<Self> {
        match value {
            PIXMAN_A8B8G8R8 => Ok(Self::A8b8g8r8),
            PIXMAN_A8R8G8B8 => Ok(Self::A8r8g8b8),
            PIXMAN_B8G8R8A8 => Ok(Self::B8g8r8a8),
            PIXMAN_B8G8R8X8 => Ok(Self::B8g8r8x8),
            PIXMAN_R8G8B8A8 => Ok(Self::R8g8b8a8),
            PIXMAN_R8G8B8X8 => Ok(Self::R8g8b8x8),
            PIXMAN_X8B8G8R8 => Ok(Self::X8b8g8r8),
            PIXMAN_X8R8G8B8 => Ok(Self::X8r8g8b8),
            _ => bail!("pixel format {value:#x} is not supported yet"),
        }
    }
}
