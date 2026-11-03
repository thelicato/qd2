/// Where this console sits inside the guest's whole desktop, in guest pixels.
///
/// QEMU scales an absolute position by the *console's* own size, but the
/// guest's single absolute pointing device spans the *whole* desktop. For a
/// multi-head guest we therefore translate a console-local position into the
/// global one and pre-scale it so QEMU's per-console scaling lands exactly:
/// `x' = (offset_x + x) * console_w / total_w`. Single head = identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct HeadMap {
    offset_x: i64,
    offset_y: i64,
    width: i64,
    height: i64,
    total_width: i64,
    total_height: i64,
}

/// Build the head map from `(id, width, height)` of every console. Explicit
/// `(id, x, y)` offsets win; otherwise heads are laid out left-to-right in id
/// order (the default hot-plug arrangement of KDE and GNOME).
pub(super) fn compute_head_map(
    sizes: &[(u32, u32, u32)],
    explicit: &[(u32, i32, i32)],
    self_id: u32,
) -> Option<HeadMap> {
    let mut sizes: Vec<_> = sizes
        .iter()
        .copied()
        .filter(|(_, w, h)| *w > 0 && *h > 0)
        .collect();
    if sizes.len() < 2 {
        return None;
    }
    sizes.sort_by_key(|(id, _, _)| *id);
    let mut placed = Vec::with_capacity(sizes.len());
    let mut cursor_x: i64 = 0;
    for (id, w, h) in &sizes {
        let (x, y) = match explicit.iter().find(|(eid, _, _)| eid == id) {
            Some((_, x, y)) => (i64::from(*x), i64::from(*y)),
            None => (cursor_x, 0),
        };
        cursor_x = cursor_x.max(x + i64::from(*w));
        placed.push((*id, x, y, i64::from(*w), i64::from(*h)));
    }
    let min_x = placed.iter().map(|(_, x, _, _, _)| *x).min()?;
    let min_y = placed.iter().map(|(_, _, y, _, _)| *y).min()?;
    let total_width = placed.iter().map(|(_, x, _, w, _)| x + w).max()? - min_x;
    let total_height = placed.iter().map(|(_, _, y, _, h)| y + h).max()? - min_y;
    let (_, offset_x, offset_y, width, height) = *placed.iter().find(|(id, ..)| *id == self_id)?;
    Some(HeadMap {
        offset_x: offset_x - min_x,
        offset_y: offset_y - min_y,
        width,
        height,
        total_width,
        total_height,
    })
}

impl HeadMap {
    /// Head map from a scanout that shows the `(x, y, width, height)` view of
    /// a `backing_width` x `backing_height` buffer. `None` when the view is
    /// the whole buffer (single head, or a guest with one buffer per output),
    /// so the layout falls back to console sizes / `--head-layout`.
    pub(super) fn from_scanout(
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        backing_width: u32,
        backing_height: u32,
    ) -> Option<Self> {
        if width == 0
            || height == 0
            || backing_width == 0
            || backing_height == 0
            || x >= backing_width
            || y >= backing_height
            || width > backing_width - x
            || height > backing_height - y
        {
            return None;
        }
        if x == 0 && y == 0 && width == backing_width && height == backing_height {
            return None;
        }
        Some(Self {
            offset_x: i64::from(x),
            offset_y: i64::from(y),
            width: i64::from(width),
            height: i64::from(height),
            total_width: i64::from(backing_width),
            total_height: i64::from(backing_height),
        })
    }
}

/// Translate a console-local absolute position for QEMU (see [`HeadMap`]).
/// The position may lie outside this head (negative, or past its edge): it
/// is clamped to the guest's WHOLE desktop, so a pointer dragged past the
/// window edge keeps moving onto the neighboring head.
pub(super) fn map_abs_position(map: &HeadMap, x: i64, y: i64) -> (u32, u32) {
    if map.width <= 0 || map.height <= 0 || map.total_width <= 0 || map.total_height <= 0 {
        return clamp_to_console(None, x, y);
    }
    let global_x = map.offset_x.saturating_add(x).clamp(0, map.total_width - 1);
    let global_y = map
        .offset_y
        .saturating_add(y)
        .clamp(0, map.total_height - 1);
    // Console dimensions are u32; multiplying two axes can exceed i64.
    let sx = i128::from(global_x) * i128::from(map.width) / i128::from(map.total_width);
    let sy = i128::from(global_y) * i128::from(map.height) / i128::from(map.total_height);
    (
        sx.clamp(0, i128::from(map.width - 1)) as u32,
        sy.clamp(0, i128::from(map.height - 1)) as u32,
    )
}

/// Single-head clamp: keep the position inside the console.
pub(super) fn clamp_to_console(size: Option<(u32, u32)>, x: i64, y: i64) -> (u32, u32) {
    match size {
        Some((w, h)) if w > 0 && h > 0 => (
            x.clamp(0, i64::from(w) - 1) as u32,
            y.clamp(0, i64::from(h) - 1) as u32,
        ),
        _ => (
            x.clamp(0, u32::MAX as i64) as u32,
            y.clamp(0, u32::MAX as i64) as u32,
        ),
    }
}

#[cfg(test)]
mod scanout_map_tests {
    use super::{HeadMap, map_abs_position};

    #[test]
    fn whole_buffer_views_yield_no_map() {
        assert_eq!(HeadMap::from_scanout(0, 0, 2560, 1440, 2560, 1440), None);
        assert_eq!(HeadMap::from_scanout(0, 0, 2560, 1440, 0, 0), None);
        assert_eq!(HeadMap::from_scanout(0, 0, 0, 0, 4480, 1440), None);
    }

    #[test]
    fn x11_second_head_maps_into_the_shared_framebuffer() {
        // 4480x1440 X screen, head 1 = 1920x1080 at (2560, 360).
        let map = HeadMap::from_scanout(2560, 360, 1920, 1080, 4480, 1440).unwrap();
        // Top-left of head 1 -> global (2560, 360), pre-scaled by 1920/4480 and 1080/1440.
        assert_eq!(map_abs_position(&map, 0, 0), (1097, 270));
        let (x, y) = map_abs_position(&map, 1919, 1079);
        assert!(x < 1920 && y < 1080, "{x},{y}");
        // Dragging left past the seam keeps moving onto head 0.
        assert_eq!(map_abs_position(&map, -2560, -360), (0, 0));
    }

    #[test]
    fn x11_first_head_keeps_its_origin() {
        let map = HeadMap::from_scanout(0, 0, 2560, 1440, 4480, 1440).unwrap();
        assert_eq!(map_abs_position(&map, 0, 0), (0, 0));
        assert_eq!(
            map_abs_position(&map, 2559, 1439),
            (2559 * 2560 / 4480, 1439)
        );
    }
}

#[cfg(test)]
mod head_map_tests {
    use super::{clamp_to_console, compute_head_map, map_abs_position};

    #[test]
    fn single_head_has_no_map() {
        assert!(compute_head_map(&[(0, 1920, 1080)], &[], 0).is_none());
    }

    #[test]
    fn auto_layout_places_heads_left_to_right() {
        let sizes = [(1, 1280, 1024), (0, 2560, 1440)];
        let head0 = compute_head_map(&sizes, &[], 0).unwrap();
        let head1 = compute_head_map(&sizes, &[], 1).unwrap();
        // head 0 at origin, head 1 to its right; total spans both
        assert_eq!((head0.offset_x, head0.offset_y), (0, 0));
        assert_eq!((head1.offset_x, head1.offset_y), (2560, 0));
        assert_eq!((head0.total_width, head0.total_height), (3840, 1440));
        // clicking the right edge of head 1 lands on the right edge of the desktop
        let (x, _) = map_abs_position(&head1, 1279, 0);
        assert_eq!((2560 + 1279) * 1280 / 3840, i64::from(x));
        // clicking the middle of head 0 stays in head 0's half
        let (x, y) = map_abs_position(&head0, 1280, 720);
        assert_eq!((x, y), (1280 * 2560 / 3840, 720 * 1440 / 1440));
    }

    #[test]
    fn explicit_layout_overrides_auto_placement() {
        let sizes = [(0, 1920, 1080), (1, 1920, 1080)];
        let head1 = compute_head_map(&sizes, &[(1, 0, 1080)], 1).unwrap(); // stacked below
        assert_eq!((head1.offset_x, head1.offset_y), (0, 1080));
        assert_eq!((head1.total_width, head1.total_height), (1920, 2160));
        let (x, y) = map_abs_position(&head1, 100, 100);
        assert_eq!((x, y), (100, (1080 + 100) * 1080 / 2160));
    }

    #[test]
    fn positions_beyond_a_head_continue_onto_the_neighbor() {
        let sizes = [(0, 2560, 1440), (1, 1280, 1024)];
        let head0 = compute_head_map(&sizes, &[], 0).unwrap();
        // 100px past head 0's right edge = 100px into head 1 (global 2660 of 3840)
        let (x, _) = map_abs_position(&head0, 2660, 100);
        assert_eq!(i64::from(x), 2660 * 2560 / 3840);
        assert!(
            x < 2560,
            "pre-scaled position must satisfy QEMU's range check"
        );
        // far left of head 1 = clamped to the desktop's left edge
        let head1 = compute_head_map(&sizes, &[], 1).unwrap();
        assert_eq!(map_abs_position(&head1, -5000, 10).0, 0);
    }

    #[test]
    fn single_head_positions_are_clamped_to_the_console() {
        assert_eq!(clamp_to_console(Some((640, 480)), -7, 500), (0, 479));
        assert_eq!(clamp_to_console(Some((640, 480)), 100, 100), (100, 100));
        assert_eq!(clamp_to_console(None, -3, 9), (0, 9));
    }

    #[test]
    fn mapped_positions_stay_inside_the_console() {
        let sizes = [(0, 1000, 500), (1, 1000, 500)];
        let head1 = compute_head_map(&sizes, &[], 1).unwrap();
        let (x, y) = map_abs_position(&head1, 999, 499);
        assert!(x < 1000 && y < 500);
    }

    #[test]
    fn negative_offsets_normalize_to_the_actual_desktop_bounds() {
        let sizes = [(0, 1920, 1080), (1, 1920, 1080)];
        let layout = [(0, 0, 0), (1, -1920, -1080)];
        let left = compute_head_map(&sizes, &layout, 1).unwrap();
        let right = compute_head_map(&sizes, &layout, 0).unwrap();
        assert_eq!((left.offset_x, left.offset_y), (0, 0));
        assert_eq!((right.offset_x, right.offset_y), (1920, 1080));
        assert_eq!((left.total_width, left.total_height), (3840, 2160));
        assert_eq!(map_abs_position(&left, 960, 540), (480, 270));
        assert_eq!(map_abs_position(&right, 960, 540), (1440, 810));
    }

    #[test]
    fn translating_the_entire_layout_does_not_change_pointer_mapping() {
        let sizes = [(0, 1000, 800), (1, 500, 400)];
        let base = compute_head_map(&sizes, &[(0, 0, 0), (1, 1000, 100)], 1).unwrap();
        for (dx, dy) in [(-5000, -2000), (2500, 3000)] {
            let shifted =
                compute_head_map(&sizes, &[(0, dx, dy), (1, 1000 + dx, 100 + dy)], 1).unwrap();
            for (x, y) in [(-100, -50), (0, 0), (250, 200), (600, 500)] {
                assert_eq!(
                    map_abs_position(&base, x, y),
                    map_abs_position(&shifted, x, y)
                );
            }
        }
    }

    #[test]
    fn disabled_heads_do_not_distort_the_active_desktop() {
        assert!(compute_head_map(&[(0, 640, 480), (1, 0, 0)], &[], 0).is_none());
        let sizes = [(0, 640, 480), (1, 1920, 0), (2, 640, 480)];
        let head = compute_head_map(&sizes, &[], 2).unwrap();
        assert_eq!(
            (head.offset_x, head.total_width, head.total_height),
            (640, 1280, 480)
        );
        assert!(compute_head_map(&sizes, &[], 1).is_none());
    }

    #[test]
    fn extreme_positions_and_sizes_are_clamped_without_overflow() {
        let map =
            compute_head_map(&[(0, u32::MAX, u32::MAX), (1, u32::MAX, u32::MAX)], &[], 1).unwrap();
        assert_eq!(
            map_abs_position(&map, i64::MAX, i64::MAX),
            (u32::MAX - 1, u32::MAX - 1)
        );
        assert_eq!(map_abs_position(&map, i64::MIN, i64::MIN), (0, 0));
        assert_eq!(clamp_to_console(None, i64::MAX, -1), (u32::MAX, 0));
    }

    #[test]
    fn invalid_scanout_views_do_not_override_the_layout_fallback() {
        use super::HeadMap;
        assert!(HeadMap::from_scanout(100, 0, 640, 480, 640, 480).is_none());
        assert!(HeadMap::from_scanout(u32::MAX, 0, 640, 480, 640, 480).is_none());
        assert!(HeadMap::from_scanout(0, 400, 640, 480, 640, 480).is_none());
    }
}
