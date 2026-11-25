//! Pure zoom-level stepping, scaled log-row layout metrics, and scroll preservation math.

pub const DEFAULT_ZOOM_PERCENT: u16 = 100;
pub const MIN_ZOOM_PERCENT: u16 = 50;
pub const MAX_ZOOM_PERCENT: u16 = 300;
pub const ZOOM_STEP_PERCENT: u16 = 10;

pub const MIN_SCALED_FONT_SIZE: f32 = 6.0;
pub const MIN_SCALED_LINE_HEIGHT: f32 = 8.0;

const BASE_LINE_NUMBER_WIDTH: f32 = 72.0;
const BASE_LINE_NUMBER_PADDING: f32 = 12.0;
const BASE_MARK_GUTTER_WIDTH: f32 = 28.0;
const BASE_BOOKMARK_ICON_SIZE: f32 = 12.0;
const BASE_MATCH_DOT_SIZE: f32 = 9.0;

pub fn clamp_zoom_percent(percent: u16) -> u16 {
    percent.clamp(MIN_ZOOM_PERCENT, MAX_ZOOM_PERCENT)
}

pub fn zoom_in_percent(percent: u16) -> u16 {
    clamp_zoom_percent(percent)
        .saturating_add(ZOOM_STEP_PERCENT)
        .clamp(MIN_ZOOM_PERCENT, MAX_ZOOM_PERCENT)
}

pub fn zoom_out_percent(percent: u16) -> u16 {
    clamp_zoom_percent(percent)
        .saturating_sub(ZOOM_STEP_PERCENT)
        .clamp(MIN_ZOOM_PERCENT, MAX_ZOOM_PERCENT)
}

pub fn zoom_scale(percent: u16) -> f32 {
    f32::from(clamp_zoom_percent(percent)) / 100.0
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ZoomMetrics {
    pub percent: u16,
    pub scale: f32,
    pub font_size: f32,
    pub line_height: f32,
    pub line_number_size: f32,
    pub line_number_width: f32,
    pub line_number_padding: f32,
    pub mark_gutter_width: f32,
    pub bookmark_icon_size: f32,
    pub match_dot_size: f32,
}

impl ZoomMetrics {
    pub fn compute(
        base_font_size: f32,
        base_line_height: f32,
        base_small_size: f32,
        percent: u16,
    ) -> Self {
        let percent = clamp_zoom_percent(percent);
        if percent == DEFAULT_ZOOM_PERCENT {
            return Self {
                percent,
                scale: 1.0,
                font_size: base_font_size,
                line_height: base_line_height,
                line_number_size: base_small_size,
                line_number_width: BASE_LINE_NUMBER_WIDTH,
                line_number_padding: BASE_LINE_NUMBER_PADDING,
                mark_gutter_width: BASE_MARK_GUTTER_WIDTH,
                bookmark_icon_size: BASE_BOOKMARK_ICON_SIZE,
                match_dot_size: BASE_MATCH_DOT_SIZE,
            };
        }

        let scale = zoom_scale(percent);
        Self {
            percent,
            scale,
            font_size: (base_font_size * scale).max(MIN_SCALED_FONT_SIZE),
            line_height: (base_line_height * scale)
                .round()
                .max(MIN_SCALED_LINE_HEIGHT),
            line_number_size: (base_small_size * scale).max(MIN_SCALED_FONT_SIZE),
            line_number_width: (BASE_LINE_NUMBER_WIDTH * scale).round().max(32.0),
            line_number_padding: (BASE_LINE_NUMBER_PADDING * scale).round().max(4.0),
            mark_gutter_width: (BASE_MARK_GUTTER_WIDTH * scale).round().max(14.0),
            bookmark_icon_size: (BASE_BOOKMARK_ICON_SIZE * scale).round().max(6.0),
            match_dot_size: (BASE_MATCH_DOT_SIZE * scale).round().max(5.0),
        }
    }
}

/// Scales a GPUI scroll offset `(x, y)` (where negative values represent scrolled distance)
/// so the top visible row and horizontal reading position stay anchored across zoom changes.
///
/// When `viewport_height` is known, vertical scroll is also clamped to the new content height
/// so zooming out never leaves a scroll offset past the bottom or introduces a positive offset.
#[allow(clippy::too_many_arguments)]
pub fn scaled_scroll_offset(
    current_offset_x: f32,
    current_offset_y: f32,
    old_line_height: f32,
    new_line_height: f32,
    old_scale: f32,
    new_scale: f32,
    viewport_height: Option<f32>,
    row_count: usize,
) -> (f32, f32) {
    let old_pos_y = (-current_offset_y).max(0.0);
    let mut new_pos_y = if old_line_height > 0.0 && new_line_height > 0.0 {
        old_pos_y * (new_line_height / old_line_height)
    } else {
        old_pos_y
    };
    if let Some(vp_h) = viewport_height {
        let new_max_y = (new_line_height * row_count as f32 - vp_h).max(0.0);
        new_pos_y = new_pos_y.clamp(0.0, new_max_y);
    }
    let new_offset_y = if new_pos_y <= 0.0 { 0.0 } else { -new_pos_y };

    let old_pos_x = (-current_offset_x).max(0.0);
    let new_pos_x = if old_scale > 0.0 && new_scale > 0.0 {
        old_pos_x * (new_scale / old_scale)
    } else {
        old_pos_x
    };
    let new_offset_x = if new_pos_x <= 0.0 { 0.0 } else { -new_pos_x };

    (new_offset_x, new_offset_y)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stepping_zoom_in_and_out_is_exact_and_clamps_to_bounds() {
        assert_eq!(zoom_in_percent(100), 110);
        assert_eq!(zoom_out_percent(100), 90);
        assert_eq!(zoom_out_percent(zoom_in_percent(100)), 100);

        let mut z = DEFAULT_ZOOM_PERCENT;
        for _ in 0..50 {
            z = zoom_in_percent(z);
        }
        assert_eq!(z, MAX_ZOOM_PERCENT);

        for _ in 0..50 {
            z = zoom_out_percent(z);
        }
        assert_eq!(z, MIN_ZOOM_PERCENT);
    }

    #[test]
    fn default_zoom_preserves_exact_base_metrics_and_scales_monotonically() {
        let base = ZoomMetrics::compute(13.0, 24.0, 11.5, DEFAULT_ZOOM_PERCENT);
        assert_eq!(base.scale, 1.0);
        assert_eq!(base.font_size, 13.0);
        assert_eq!(base.line_height, 24.0);
        assert_eq!(base.line_number_size, 11.5);
        assert_eq!(base.line_number_width, 72.0);
        assert_eq!(base.line_number_padding, 12.0);
        assert_eq!(base.mark_gutter_width, 28.0);
        assert_eq!(base.bookmark_icon_size, 12.0);
        assert_eq!(base.match_dot_size, 9.0);

        let mut prev = ZoomMetrics::compute(13.0, 24.0, 11.5, MIN_ZOOM_PERCENT);
        let mut p = MIN_ZOOM_PERCENT + ZOOM_STEP_PERCENT;
        while p <= MAX_ZOOM_PERCENT {
            let curr = ZoomMetrics::compute(13.0, 24.0, 11.5, p);
            assert!(curr.font_size > prev.font_size, "font_size at {p}%");
            assert!(curr.line_height > prev.line_height, "line_height at {p}%");
            assert!(
                curr.line_number_width > prev.line_number_width,
                "line_number_width at {p}%"
            );
            prev = curr;
            p += ZOOM_STEP_PERCENT;
        }
    }

    #[test]
    fn scaled_scroll_offset_keeps_top_row_anchored_and_clamps_on_zoom_out() {
        // At row 500 with 24px rows (-12000), zooming to 26px keeps row 500 at top (-13000).
        let (x, y) =
            scaled_scroll_offset(-100.0, -12_000.0, 24.0, 26.0, 1.0, 1.1, Some(400.0), 1000);
        assert!((x - -110.0).abs() < 0.01);
        assert!((y - -13_000.0).abs() < 0.01);

        // At top (0.0), zooming keeps offset at exact 0.0.
        let (x0, y0) = scaled_scroll_offset(0.0, 0.0, 24.0, 36.0, 1.0, 1.5, Some(400.0), 100);
        assert_eq!((x0, y0), (0.0, 0.0));

        // Zooming out when content becomes shorter than viewport clamps cleanly to 0.0 (never positive).
        let (_, y_clamped) =
            scaled_scroll_offset(0.0, -120.0, 24.0, 12.0, 1.0, 0.5, Some(400.0), 20);
        assert_eq!(y_clamped, 0.0);

        // Zooming out near the end clamps to the new max scroll offset.
        // 50 rows * 12px = 600px, viewport 400px -> max scroll is 200px (-200.0).
        let (_, y_end) = scaled_scroll_offset(0.0, -800.0, 24.0, 12.0, 1.0, 0.5, Some(400.0), 50);
        assert_eq!(y_end, -200.0);
    }
}
