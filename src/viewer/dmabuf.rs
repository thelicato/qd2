use std::{cell::RefCell, convert::TryFrom, rc::Rc};

use anyhow::{Context, Result, bail};
use gtk::{cairo, gdk, prelude::*};
use gtk4 as gtk;
#[cfg(unix)]
use qemu_display::ScanoutDMABUF;
use qemu_display::UpdateDMABUF;

#[cfg(unix)]
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};

use super::UiState;

#[cfg(unix)]
pub(super) struct DmabufPresentation {
    texture: gdk::Texture,
    fds: Vec<OwnedFd>,
    width: u32,
    height: u32,
    offset: [u32; 4],
    stride: [u32; 4],
    fourcc: u32,
    modifier: u64,
    y0_top: bool,
    num_planes: u32,
}

#[cfg(unix)]
#[derive(Copy, Clone, Debug)]
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

    pub(super) fn describe(self) -> String {
        format!(
            "DMABUF transform: rotate={} extra-flip-y={}",
            self.rotation_quarters * 90,
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
        )
    }

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
        })
    }
}

#[cfg(target_os = "linux")]
pub(super) fn build_dmabuf_presentation(
    display: &gdk::Display,
    scanout: DmabufFrame,
) -> Result<DmabufPresentation> {
    DmabufPresentation::new(display, scanout)
}

#[cfg(all(unix, not(target_os = "linux")))]
pub(super) fn build_dmabuf_presentation(
    _display: &gdk::Display,
    _scanout: DmabufFrame,
) -> Result<DmabufPresentation> {
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
    ) -> Result<()> {
        let update_region = dmabuf_update_region(updates, self.width, self.height);
        let previous_texture = self.texture.clone();

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
            Some(&previous_texture),
        )?;

        Ok(())
    }

    fn to_paintable(&self, transform: DmabufViewTransform) -> Result<gdk::Paintable> {
        let needs_vertical_flip = !self.y0_top ^ transform.extra_vertical_flip;
        if !needs_vertical_flip && transform.rotation_quarters == 0 {
            return Ok(self.texture.clone().upcast());
        }

        let snapshot = gtk::Snapshot::new();
        let bounds = gtk::graphene::Rect::new(0.0, 0.0, self.width as f32, self.height as f32);

        snapshot.save();

        match transform.rotation_quarters % 4 {
            0 => {}
            1 => {
                snapshot.translate(&gtk::graphene::Point::new(self.height as f32, 0.0));
                snapshot.rotate(90.0);
            }
            2 => {
                snapshot.translate(&gtk::graphene::Point::new(
                    self.width as f32,
                    self.height as f32,
                ));
                snapshot.rotate(180.0);
            }
            3 => {
                snapshot.translate(&gtk::graphene::Point::new(0.0, self.width as f32));
                snapshot.rotate(270.0);
            }
            _ => unreachable!(),
        }

        if needs_vertical_flip {
            snapshot.translate(&gtk::graphene::Point::new(0.0, self.height as f32));
            snapshot.scale(1.0, -1.0);
        }

        snapshot.append_texture(&self.texture, &bounds);
        snapshot.restore();

        snapshot
            .to_paintable(Some(&gtk::graphene::Size::new(
                self.width as f32,
                self.height as f32,
            )))
            .context("failed to build a GTK paintable for the DMABUF texture")
    }
}

#[cfg(unix)]
pub(super) fn present_dmabuf_paintable(
    picture: &gtk::Picture,
    status_label: &gtk::Label,
    ui_state: &Rc<RefCell<UiState>>,
    window: &gtk::Window,
    window_base_title: &str,
    presentation: &DmabufPresentation,
    transform: DmabufViewTransform,
) -> Result<()> {
    let paintable = presentation.to_paintable(transform)?;
    super::present_paintable(
        picture,
        status_label,
        ui_state,
        window,
        window_base_title,
        &paintable,
        paintable.intrinsic_width().max(0) as u32,
        paintable.intrinsic_height().max(0) as u32,
    );
    Ok(())
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
