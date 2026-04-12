use std::{convert::TryFrom, sync::mpsc::Sender};

use anyhow::{Context, Result, bail};
use qemu_display::{Scanout, Update};
#[cfg(unix)]
use qemu_display::{ScanoutDMABUF, ScanoutMap, ScanoutMmap, UpdateDMABUF, UpdateMap};

use super::{ViewerEvent, cursor::GuestCursor, dmabuf::DmabufFrame};

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

#[derive(Copy, Clone)]
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

    fn apply_update(&mut self, update: Update) -> Result<()> {
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
        self.blit_update_from_linear(rect, src_stride, &update.data)
    }

    #[cfg(unix)]
    fn refresh_mapped_region(&mut self, update: UpdateMap) -> Result<()> {
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
        )
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

pub(super) struct FrameStreamHandler {
    event_tx: Sender<ViewerEvent>,
    framebuffer: Option<Framebuffer>,
}

impl FrameStreamHandler {
    pub(super) fn new(event_tx: Sender<ViewerEvent>) -> Self {
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
            Ok(()) => self.send_current_frame(),
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
            Ok(()) => self.send_current_frame(),
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
