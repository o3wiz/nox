use super::theme::Theme;
use gpui::{
    App, Bounds, ColorExt, Corners, CursorStyle, DispatchPhase, HitboxBehavior, MouseButton,
    MouseDownEvent, MouseExitEvent, MouseMoveEvent, PaintQuad, Pixels, Point, ScrollStrategy, Size,
    UniformListScrollHandle, Window, canvas, point, prelude::*, px, size,
};
use std::{cell::Cell, rc::Rc};

pub const DEFAULT_SCROLLBAR_WIDTH: f32 = 10.0;
pub const HORIZONTAL_BAR_HEIGHT: f32 = 8.0;
pub const HORIZONTAL_SCROLL_STEP: f32 = 48.0;
pub const MIN_VERTICAL_THUMB: f32 = 24.0;
pub const MIN_HORIZONTAL_THUMB: f32 = 28.0;
pub const TRACK_END_PADDING: f32 = 2.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollAxis {
    Vertical,
    Horizontal,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScrollMetrics {
    pub viewport: f32,
    pub content: f32,
    pub offset: f32,
    pub max_offset: f32,
}

impl ScrollMetrics {
    /// Extracts scrollable metrics for `axis` from a `UniformListScrollHandle` after prepaint,
    /// subtracting a fixed non-scrolling `leading_inset` (such as a sticky gutter width on the
    /// horizontal axis) from both viewport and content width so thumb proportions match the
    /// visible scrollable region while preserving `max_offset`.
    ///
    /// Returns `None` if the list has not been laid out yet or does not overflow along `axis`.
    pub fn from_uniform_list_with_inset(
        handle: &UniformListScrollHandle,
        axis: ScrollAxis,
        leading_inset: f32,
    ) -> Option<Self> {
        let state = handle.0.borrow();
        let item_size = state.last_item_size?;
        let offset_pt = state.base_handle.offset();
        let (raw_viewport, raw_content, offset) = match axis {
            ScrollAxis::Vertical => (
                f32::from(item_size.item.height),
                f32::from(item_size.contents.height),
                -f32::from(offset_pt.y),
            ),
            ScrollAxis::Horizontal => (
                f32::from(item_size.item.width),
                f32::from(item_size.contents.width),
                -f32::from(offset_pt.x),
            ),
        };
        let max_offset = (raw_content - raw_viewport).max(0.0);
        if raw_viewport <= 0.0 || raw_content <= 0.0 || max_offset <= 0.5 {
            return None;
        }
        let inset = leading_inset.max(0.0);
        let (viewport, content) =
            if inset > 0.0 && raw_viewport > inset + 1.0 && raw_content > inset + 1.0 {
                (raw_viewport - inset, raw_content - inset)
            } else {
                (raw_viewport, raw_content)
            };
        Some(Self {
            viewport,
            content,
            offset: offset.clamp(0.0, max_offset),
            max_offset,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ThumbSpan {
    pub start: f32,
    pub length: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScrollbarLayout {
    pub track_bounds: Bounds<Pixels>,
    pub thumb: ThumbSpan,
    pub max_offset: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TrackPress {
    pub grab_offset: f32,
    pub jump_to: Option<f32>,
}

/// Shared per-scrollbar frame state persisted across renders so mouse events
/// always read the geometry computed on the latest prepaint pass.
#[derive(Clone, Default)]
pub struct ScrollbarTrackState {
    layout: Rc<Cell<Option<ScrollbarLayout>>>,
    hovered: Rc<Cell<bool>>,
}

impl ScrollbarTrackState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn layout(&self) -> Option<ScrollbarLayout> {
        self.layout.get()
    }

    pub fn clear(&self) {
        self.layout.set(None);
        self.hovered.set(false);
    }
}

/// Computes the scrollbar thumb start and length along a track of length `track_length`.
pub fn compute_thumb(
    metrics: ScrollMetrics,
    track_length: f32,
    min_thumb: f32,
) -> Option<ThumbSpan> {
    if track_length <= 0.0
        || metrics.viewport <= 0.0
        || metrics.content <= 0.0
        || metrics.max_offset <= 0.5
    {
        return None;
    }
    let raw_length = track_length * (metrics.viewport / metrics.content);
    let min_clamped = min_thumb.min(track_length);
    let length = raw_length.clamp(min_clamped, track_length);
    let fraction = (metrics.offset / metrics.max_offset).clamp(0.0, 1.0);
    let travel = (track_length - length).max(0.0);
    Some(ThumbSpan {
        start: fraction * travel,
        length,
    })
}

/// Maps a desired thumb start coordinate (relative to track start) back to a positive scroll offset.
pub fn scroll_offset_for_thumb_start(
    thumb_start: f32,
    thumb_length: f32,
    track_length: f32,
    max_offset: f32,
) -> f32 {
    let travel = track_length - thumb_length;
    if travel <= 0.0 || max_offset <= 0.0 {
        return 0.0;
    }
    let fraction = (thumb_start / travel).clamp(0.0, 1.0);
    fraction * max_offset
}

/// Computes the padded track bounds and thumb span for a scrollbar element.
pub fn compute_scrollbar_layout(
    handle: &UniformListScrollHandle,
    axis: ScrollAxis,
    leading_inset: f32,
    outer_bounds: Bounds<Pixels>,
) -> Option<ScrollbarLayout> {
    let metrics = ScrollMetrics::from_uniform_list_with_inset(handle, axis, leading_inset)?;
    let (track_bounds, track_len, min_thumb) = match axis {
        ScrollAxis::Vertical => {
            let h = (f32::from(outer_bounds.size.height) - TRACK_END_PADDING * 2.0).max(0.0);
            let bounds = Bounds::new(
                point(
                    outer_bounds.origin.x,
                    outer_bounds.origin.y + px(TRACK_END_PADDING),
                ),
                size(outer_bounds.size.width, px(h)),
            );
            (bounds, h, MIN_VERTICAL_THUMB)
        }
        ScrollAxis::Horizontal => {
            let w = (f32::from(outer_bounds.size.width) - TRACK_END_PADDING * 2.0).max(0.0);
            let bounds = Bounds::new(
                point(
                    outer_bounds.origin.x + px(TRACK_END_PADDING),
                    outer_bounds.origin.y,
                ),
                size(px(w), outer_bounds.size.height),
            );
            (bounds, w, MIN_HORIZONTAL_THUMB)
        }
    };
    let thumb = compute_thumb(metrics, track_len, min_thumb)?;
    Some(ScrollbarLayout {
        track_bounds,
        thumb,
        max_offset: metrics.max_offset,
    })
}

/// Resolves a mouse press at `cursor` against the current scrollbar layout.
///
/// Clicking inside the thumb preserves the grab position without jumping;
/// clicking on the track outside the thumb jumps so the thumb centers on the pointer.
pub fn resolve_track_press(
    layout: ScrollbarLayout,
    axis: ScrollAxis,
    cursor: Point<Pixels>,
) -> TrackPress {
    let (cursor_rel, track_len) = match axis {
        ScrollAxis::Vertical => (
            f32::from(cursor.y - layout.track_bounds.origin.y),
            f32::from(layout.track_bounds.size.height),
        ),
        ScrollAxis::Horizontal => (
            f32::from(cursor.x - layout.track_bounds.origin.x),
            f32::from(layout.track_bounds.size.width),
        ),
    };
    let thumb_end = layout.thumb.start + layout.thumb.length;
    if cursor_rel >= layout.thumb.start && cursor_rel <= thumb_end {
        TrackPress {
            grab_offset: cursor_rel - layout.thumb.start,
            jump_to: None,
        }
    } else {
        let grab_offset = layout.thumb.length * 0.5;
        let target_start = cursor_rel - grab_offset;
        let offset = scroll_offset_for_thumb_start(
            target_start,
            layout.thumb.length,
            track_len,
            layout.max_offset,
        );
        TrackPress {
            grab_offset,
            jump_to: Some(offset),
        }
    }
}

/// Computes the new positive scroll offset during an active scrollbar drag.
pub fn scroll_offset_for_drag(
    layout: ScrollbarLayout,
    axis: ScrollAxis,
    cursor: Point<Pixels>,
    grab_offset: f32,
) -> f32 {
    let (cursor_rel, track_len) = match axis {
        ScrollAxis::Vertical => (
            f32::from(cursor.y - layout.track_bounds.origin.y),
            f32::from(layout.track_bounds.size.height),
        ),
        ScrollAxis::Horizontal => (
            f32::from(cursor.x - layout.track_bounds.origin.x),
            f32::from(layout.track_bounds.size.width),
        ),
    };
    scroll_offset_for_thumb_start(
        cursor_rel - grab_offset,
        layout.thumb.length,
        track_len,
        layout.max_offset,
    )
}

/// Computes the horizontal keyboard scroll distance in pixels for the current zoom scale and viewport.
pub fn horizontal_scroll_step(viewport: f32, zoom_scale: f32) -> f32 {
    let scaled = (HORIZONTAL_SCROLL_STEP * zoom_scale).max(16.0);
    if viewport > 0.0 {
        scaled.min((viewport * 0.5).max(16.0))
    } else {
        scaled
    }
}

/// Computes the next clamped positive scroll offset when stepping by `delta` (`< 0` left/up, `> 0` right/down).
/// Returns `Some(next_offset)` only when the offset changes meaningfully.
pub fn stepped_scroll_offset(
    raw_offset: f32,
    viewport: f32,
    content: f32,
    delta: f32,
) -> Option<f32> {
    if viewport <= 0.0 || content <= 0.0 {
        return None;
    }
    let max_offset = (content - viewport).max(0.0);
    let next = if max_offset <= 0.5 {
        0.0
    } else {
        (raw_offset.clamp(0.0, max_offset) + delta).clamp(0.0, max_offset)
    };
    if (next - raw_offset).abs() <= 0.01 {
        None
    } else {
        Some(next)
    }
}

/// Steps the scroll offset of a `UniformListScrollHandle` along `axis` by the delta returned by
/// `delta_for_viewport` (`< 0` toward the start, `> 0` toward the end), clamped to `[0.0, max_offset]`.
/// Preserves the orthogonal axis offset and returns `true` if the offset changed.
pub fn step_uniform_list_axis_offset(
    handle: &UniformListScrollHandle,
    axis: ScrollAxis,
    delta_for_viewport: impl FnOnce(f32) -> f32,
) -> bool {
    let state = handle.0.borrow();
    let Some(item_size) = state.last_item_size else {
        return false;
    };
    let mut current = state.base_handle.offset();
    let (viewport, content, raw_offset) = match axis {
        ScrollAxis::Vertical => (
            f32::from(item_size.item.height),
            f32::from(item_size.contents.height),
            -f32::from(current.y),
        ),
        ScrollAxis::Horizontal => (
            f32::from(item_size.item.width),
            f32::from(item_size.contents.width),
            -f32::from(current.x),
        ),
    };
    let delta = delta_for_viewport(viewport);
    let Some(next) = stepped_scroll_offset(raw_offset, viewport, content, delta) else {
        return false;
    };
    let clamped = if next <= 0.0 { 0.0 } else { -next };
    match axis {
        ScrollAxis::Vertical => current.y = px(clamped),
        ScrollAxis::Horizontal => current.x = px(clamped),
    }
    state.base_handle.set_offset(current);
    true
}

/// Applies a positive scroll offset along `axis` to `handle`, preserving the orthogonal axis.
pub fn set_uniform_list_axis_offset(
    handle: &UniformListScrollHandle,
    axis: ScrollAxis,
    positive_offset: f32,
) {
    let mut state = handle.0.borrow_mut();
    if axis == ScrollAxis::Vertical {
        state.deferred_scroll_to_item = None;
    }
    let mut current = state.base_handle.offset();
    let clamped = if positive_offset <= 0.0 {
        0.0
    } else {
        -positive_offset
    };
    match axis {
        ScrollAxis::Vertical => current.y = px(clamped),
        ScrollAxis::Horizontal => current.x = px(clamped),
    }
    state.base_handle.set_offset(current);
}

/// Placement preference when revealing a target row in a uniform list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RevealStrategy {
    /// Scroll the minimum distance needed so the row stays at least `surrounding_lines` rows
    /// inside the viewport's top and bottom edges (clamped by list and viewport bounds).
    Nearest,
    /// If the row is outside the inner band defined by `surrounding_lines`, center it vertically
    /// in the viewport (clamped by list bounds).
    Center,
}

/// Result of attempting to reveal a row in a `UniformListScrollHandle`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RevealOutcome {
    /// Viewport geometry was known and the scroll offset already satisfied the reveal margin.
    Unchanged,
    /// Viewport geometry was known and the vertical scroll offset was updated immediately.
    Scrolled,
    /// The list has not been laid out yet; a deferred scroll was queued and a post-layout pass
    /// should enforce the exact surrounding-lines margin once geometry is available.
    Deferred,
}

/// Computes the new positive vertical scroll offset (`scroll_top >= 0.0`) needed to reveal `row`
/// while keeping at least `surrounding_lines` rows visible above and below it whenever space allows.
///
/// When `surrounding_lines` fills or exceeds the available viewport around the active row, the
/// target row is centered vertically (clamped to the start and end of the list), matching editor
/// `cursorSurroundingLines` / `scrolloff` behavior.
///
/// Returns `None` when the list is empty or row/viewport geometry is non-positive or non-finite.
pub fn reveal_scroll_top(
    row: usize,
    row_count: usize,
    row_height: f32,
    viewport_height: f32,
    current_scroll_top: f32,
    surrounding_lines: usize,
    strategy: RevealStrategy,
) -> Option<f32> {
    if row_count == 0
        || !row_height.is_finite()
        || row_height <= 0.0
        || !viewport_height.is_finite()
        || viewport_height <= 0.0
    {
        return None;
    }

    let row = row.min(row_count - 1);
    let row_h = f64::from(row_height);
    let vp_h = f64::from(viewport_height);
    let content_h = row_count as f64 * row_h;
    let max_scroll = (content_h - vp_h).max(0.0);
    if max_scroll <= 0.0 {
        return Some(0.0);
    }

    let current = if current_scroll_top.is_finite() {
        f64::from(current_scroll_top).clamp(0.0, max_scroll)
    } else {
        0.0
    };

    let row_top = row as f64 * row_h;
    let row_bottom = row_top + row_h;

    if vp_h <= row_h {
        return Some(row_top.clamp(0.0, max_scroll) as f32);
    }

    let max_margin = (vp_h - row_h) * 0.5;
    let desired_margin = surrounding_lines as f64 * row_h;
    let centered = ((row_top + row_h * 0.5) - vp_h * 0.5).clamp(0.0, max_scroll);

    if desired_margin >= max_margin {
        return Some(centered as f32);
    }

    let lower = (row_bottom + desired_margin - vp_h).clamp(0.0, max_scroll);
    let upper = (row_top - desired_margin).clamp(0.0, max_scroll);

    if lower >= upper {
        return Some(lower as f32);
    }

    const EPSILON: f64 = 0.01;
    if current >= lower - EPSILON && current <= upper + EPSILON {
        return Some(current as f32);
    }

    let target = match strategy {
        RevealStrategy::Nearest => current.clamp(lower, upper),
        RevealStrategy::Center => centered,
    };
    Some(target as f32)
}

/// Reveals `row` in `handle` while maintaining `surrounding_lines` rows of margin above and below.
pub fn reveal_uniform_list_row(
    handle: &UniformListScrollHandle,
    row: usize,
    row_count: usize,
    row_height: f32,
    surrounding_lines: usize,
    strategy: RevealStrategy,
) -> RevealOutcome {
    if row_count == 0 {
        return RevealOutcome::Unchanged;
    }
    let row = row.min(row_count - 1);
    let viewport_height = {
        let state = handle.0.borrow();
        state
            .last_item_size
            .map(|item_size| f32::from(item_size.item.height))
            .filter(|height| height.is_finite() && *height > 0.0)
    };

    if let Some(viewport_height) = viewport_height {
        let current_scroll_top = {
            let state = handle.0.borrow();
            -f32::from(state.base_handle.offset().y)
        };
        if let Some(target_scroll_top) = reveal_scroll_top(
            row,
            row_count,
            row_height,
            viewport_height,
            current_scroll_top,
            surrounding_lines,
            strategy,
        ) {
            let mut state = handle.0.borrow_mut();
            let had_deferred = state.deferred_scroll_to_item.take().is_some();
            let current_y = f32::from(state.base_handle.offset().y);
            let target_y = if target_scroll_top <= 0.0 {
                0.0
            } else {
                -target_scroll_top
            };
            if had_deferred || (target_y - current_y).abs() > 1e-4 {
                let mut offset = state.base_handle.offset();
                offset.y = px(target_y);
                state.base_handle.set_offset(offset);
                return RevealOutcome::Scrolled;
            }
            return RevealOutcome::Unchanged;
        }
    }

    let gpui_strategy = match strategy {
        RevealStrategy::Nearest => ScrollStrategy::Nearest,
        RevealStrategy::Center => ScrollStrategy::Center,
    };
    handle.scroll_to_item(row, gpui_strategy);
    if surrounding_lines > 0 {
        RevealOutcome::Deferred
    } else {
        RevealOutcome::Scrolled
    }
}

/// Renders a zero-lag scrollbar canvas that reads scroll geometry after sibling `uniform_list` prepaint.
///
/// For `ScrollAxis::Horizontal`, the hitbox uses `HitboxBehavior::BlockMouseExceptScroll` and is
/// only inserted when horizontal overflow exists, so clicks pass cleanly through to rows when no
/// horizontal scrollbar is visible.
pub fn scrollbar_canvas(
    scroll: UniformListScrollHandle,
    axis: ScrollAxis,
    leading_inset: f32,
    track_state: ScrollbarTrackState,
    dragging: bool,
    theme: Theme,
    on_press: impl Fn(Point<Pixels>, &mut Window, &mut App) + 'static,
) -> gpui::Canvas<(Option<gpui::Hitbox>, Option<ScrollbarLayout>)> {
    let prepaint_state = track_state.clone();
    let paint_state = track_state;
    canvas(
        move |bounds, window, _cx| {
            let layout = compute_scrollbar_layout(&scroll, axis, leading_inset, bounds);
            prepaint_state.layout.set(layout);
            if layout.is_some() {
                let behavior = match axis {
                    ScrollAxis::Vertical => HitboxBehavior::Normal,
                    ScrollAxis::Horizontal => HitboxBehavior::BlockMouseExceptScroll,
                };
                let hitbox = window.insert_hitbox(bounds, behavior);
                (Some(hitbox), layout)
            } else {
                prepaint_state.hovered.set(false);
                (None, None)
            }
        },
        move |bounds, (hitbox, layout), window, _cx| {
            let (Some(hitbox), Some(layout)) = (hitbox, layout) else {
                return;
            };
            let is_hovered = hitbox.is_hovered(window);
            paint_state.hovered.set(is_hovered);
            if is_hovered || dragging {
                window.set_cursor_style(CursorStyle::PointingHand, &hitbox);
            }

            let active = is_hovered || dragging;
            let thumb_color = if dragging {
                theme.accent.opacity(0.76)
            } else if is_hovered {
                theme.muted.opacity(0.56)
            } else {
                match axis {
                    ScrollAxis::Vertical => theme.muted.opacity(0.30),
                    ScrollAxis::Horizontal => theme.muted.opacity(0.32),
                }
            };

            match axis {
                ScrollAxis::Vertical => {
                    let rail_w = f32::from(bounds.size.width);
                    let target_w = if active { 6.0f32 } else { 5.0f32 };
                    let thumb_w = target_w.min((rail_w - 2.0).max(3.0)).min(rail_w);
                    let thumb_x = f32::from(bounds.origin.x) + (rail_w - thumb_w) * 0.5;
                    let thumb_bounds = Bounds::new(
                        point(
                            px(thumb_x),
                            layout.track_bounds.origin.y + px(layout.thumb.start),
                        ),
                        Size {
                            width: px(thumb_w),
                            height: px(layout.thumb.length),
                        },
                    );
                    let quad = PaintQuad::new(thumb_bounds)
                        .background(thumb_color)
                        .corner_radii(Corners::all(px(thumb_w * 0.5)));
                    window.paint_quad(quad);
                }
                ScrollAxis::Horizontal => {
                    if active {
                        let track_quad =
                            PaintQuad::new(bounds).background(theme.panel.opacity(0.80));
                        window.paint_quad(track_quad);
                        let top_border =
                            Bounds::new(bounds.origin, size(bounds.size.width, px(1.0)));
                        window.paint_quad(
                            PaintQuad::new(top_border).background(theme.border.opacity(0.60)),
                        );
                    }
                    let bar_h = f32::from(bounds.size.height);
                    let thumb_h = if active { 5.5f32 } else { 4.0f32 }.min(bar_h.max(1.0));
                    let thumb_y = if active {
                        f32::from(bounds.origin.y) + (bar_h - thumb_h) * 0.5
                    } else {
                        f32::from(bounds.bottom()) - thumb_h - 2.0
                    };
                    let thumb_bounds = Bounds::new(
                        point(
                            layout.track_bounds.origin.x + px(layout.thumb.start),
                            px(thumb_y),
                        ),
                        Size {
                            width: px(layout.thumb.length),
                            height: px(thumb_h),
                        },
                    );
                    let quad = PaintQuad::new(thumb_bounds)
                        .background(thumb_color)
                        .corner_radii(Corners::all(px(thumb_h * 0.5)));
                    window.paint_quad(quad);
                }
            }

            let current_view = window.current_view();
            let hovered_for_move = paint_state.hovered.clone();
            let hitbox_for_move = hitbox.clone();
            window.on_mouse_event(move |_event: &MouseMoveEvent, phase, window, cx| {
                if phase == DispatchPhase::Bubble {
                    let now_hovered = hitbox_for_move.is_hovered(window);
                    if hovered_for_move.get() != now_hovered {
                        hovered_for_move.set(now_hovered);
                        cx.notify(current_view);
                    }
                }
            });

            let hovered_for_exit = paint_state.hovered.clone();
            window.on_mouse_event(move |_event: &MouseExitEvent, phase, _window, cx| {
                if phase == DispatchPhase::Bubble && hovered_for_exit.replace(false) {
                    cx.notify(current_view);
                }
            });

            let hitbox_for_down = hitbox;
            window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
                if phase == DispatchPhase::Bubble
                    && event.button == MouseButton::Left
                    && hitbox_for_down.is_hovered(window)
                {
                    on_press(event.position, window, cx);
                    cx.stop_propagation();
                }
            });
        },
    )
    .size_full()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compute_thumb_returns_none_when_content_fits() {
        let metrics = ScrollMetrics {
            viewport: 400.0,
            content: 300.0,
            offset: 0.0,
            max_offset: 0.0,
        };
        assert_eq!(compute_thumb(metrics, 400.0, MIN_VERTICAL_THUMB), None);
    }

    #[test]
    fn compute_thumb_scales_proportionally_and_tracks_scroll_position() {
        let top = ScrollMetrics {
            viewport: 200.0,
            content: 1000.0,
            offset: 0.0,
            max_offset: 800.0,
        };
        assert_eq!(
            compute_thumb(top, 500.0, MIN_VERTICAL_THUMB),
            Some(ThumbSpan {
                start: 0.0,
                length: 100.0,
            })
        );

        let mid = ScrollMetrics {
            offset: 400.0,
            ..top
        };
        assert_eq!(
            compute_thumb(mid, 500.0, MIN_VERTICAL_THUMB),
            Some(ThumbSpan {
                start: 200.0,
                length: 100.0,
            })
        );

        let bottom = ScrollMetrics {
            offset: 800.0,
            ..top
        };
        assert_eq!(
            compute_thumb(bottom, 500.0, MIN_VERTICAL_THUMB),
            Some(ThumbSpan {
                start: 400.0,
                length: 100.0,
            })
        );
    }

    #[test]
    fn compute_thumb_enforces_minimum_size_for_very_large_logs() {
        let huge = ScrollMetrics {
            viewport: 600.0,
            content: 24_000_000.0,
            offset: 12_000_000.0,
            max_offset: 23_999_400.0,
        };
        let thumb = compute_thumb(huge, 600.0, MIN_VERTICAL_THUMB).unwrap();
        assert_eq!(thumb.length, MIN_VERTICAL_THUMB);
        let expected_mid = (600.0 - MIN_VERTICAL_THUMB) * (12_000_000.0 / 23_999_400.0);
        assert!((thumb.start - expected_mid).abs() < 0.01);
    }

    #[test]
    fn track_press_grabs_thumb_without_jumping_and_jumps_on_track_click() {
        let layout = ScrollbarLayout {
            track_bounds: Bounds::new(point(px(100.0), px(10.0)), size(px(16.0), px(500.0))),
            thumb: ThumbSpan {
                start: 100.0,
                length: 100.0,
            },
            max_offset: 800.0,
        };

        // Click inside thumb (y = 10 + 130 = 140) -> grab_offset = 30, no jump.
        let on_thumb =
            resolve_track_press(layout, ScrollAxis::Vertical, point(px(108.0), px(140.0)));
        assert_eq!(
            on_thumb,
            TrackPress {
                grab_offset: 30.0,
                jump_to: None,
            }
        );

        // Click on track near bottom (y = 10 + 450 = 460) -> centers thumb at 450 (start = 400 -> max_offset).
        let on_track =
            resolve_track_press(layout, ScrollAxis::Vertical, point(px(108.0), px(460.0)));
        assert_eq!(on_track.grab_offset, 50.0);
        assert_eq!(on_track.jump_to, Some(800.0));
    }

    #[test]
    fn drag_offset_clamps_to_bounds() {
        let layout = ScrollbarLayout {
            track_bounds: Bounds::new(point(px(0.0), px(20.0)), size(px(400.0), px(10.0))),
            thumb: ThumbSpan {
                start: 0.0,
                length: 80.0,
            },
            max_offset: 1000.0,
        };
        // Drag past left edge -> 0.0
        assert_eq!(
            scroll_offset_for_drag(
                layout,
                ScrollAxis::Horizontal,
                point(px(-50.0), px(25.0)),
                20.0
            ),
            0.0
        );
        // Drag to midpoint (thumb_start = 160 out of 320 travel -> 500.0)
        assert_eq!(
            scroll_offset_for_drag(
                layout,
                ScrollAxis::Horizontal,
                point(px(180.0), px(25.0)),
                20.0
            ),
            500.0
        );
        // Drag past right edge -> 1000.0
        assert_eq!(
            scroll_offset_for_drag(
                layout,
                ScrollAxis::Horizontal,
                point(px(900.0), px(25.0)),
                20.0
            ),
            1000.0
        );
    }

    #[test]
    fn horizontal_scroll_step_scales_with_zoom_and_caps_to_half_viewport() {
        assert_eq!(horizontal_scroll_step(800.0, 1.0), HORIZONTAL_SCROLL_STEP);
        assert_eq!(
            horizontal_scroll_step(800.0, 2.0),
            HORIZONTAL_SCROLL_STEP * 2.0
        );
        assert_eq!(
            horizontal_scroll_step(800.0, 0.5),
            HORIZONTAL_SCROLL_STEP * 0.5
        );
        assert_eq!(horizontal_scroll_step(60.0, 2.0), 30.0);
        assert_eq!(horizontal_scroll_step(10.0, 1.0), 16.0);
    }

    #[test]
    fn stepped_scroll_offset_clamps_to_bounds_and_skips_noops() {
        // Content fits within viewport -> no horizontal scrolling, stale offset resets to 0.
        assert_eq!(stepped_scroll_offset(0.0, 400.0, 300.0, 48.0), None);
        assert_eq!(stepped_scroll_offset(0.0, 400.0, 300.0, -48.0), None);
        assert_eq!(stepped_scroll_offset(50.0, 400.0, 300.0, -48.0), Some(0.0));
        assert_eq!(stepped_scroll_offset(0.0, 0.0, 300.0, 48.0), None);

        // Overflowing content (max_offset = 600.0).
        assert_eq!(stepped_scroll_offset(0.0, 400.0, 1000.0, -48.0), None);
        assert_eq!(stepped_scroll_offset(0.0, 400.0, 1000.0, 48.0), Some(48.0));
        assert_eq!(stepped_scroll_offset(30.0, 400.0, 1000.0, -48.0), Some(0.0));
        assert_eq!(
            stepped_scroll_offset(580.0, 400.0, 1000.0, 48.0),
            Some(600.0)
        );
        assert_eq!(stepped_scroll_offset(600.0, 400.0, 1000.0, 48.0), None);
    }

    #[test]
    fn reveal_scroll_top_with_zero_surrounding_matches_standard_nearest_and_center() {
        // 100 rows * 24px = 2400px, viewport = 240px (10 rows), max_scroll = 2160px.
        // Visible rows at scroll_top = 0 are 0..10.
        assert_eq!(
            reveal_scroll_top(0, 100, 24.0, 240.0, 0.0, 0, RevealStrategy::Nearest),
            Some(0.0)
        );
        assert_eq!(
            reveal_scroll_top(9, 100, 24.0, 240.0, 0.0, 0, RevealStrategy::Nearest),
            Some(0.0)
        );
        // Row 10 is just below viewport -> scrolls down 1 row (24px).
        assert_eq!(
            reveal_scroll_top(10, 100, 24.0, 240.0, 0.0, 0, RevealStrategy::Nearest),
            Some(24.0)
        );
        // Row 4 at scroll_top = 120 (showing 5..15) -> scrolls up to row 4 top (96px).
        assert_eq!(
            reveal_scroll_top(4, 100, 24.0, 240.0, 120.0, 0, RevealStrategy::Nearest),
            Some(96.0)
        );
        // Center strategy stays put when visible and centers when off-screen.
        assert_eq!(
            reveal_scroll_top(9, 100, 24.0, 240.0, 0.0, 0, RevealStrategy::Center),
            Some(0.0)
        );
        // Row 50 center = 50*24 + 12 = 1212, viewport/2 = 120 -> 1092.
        assert_eq!(
            reveal_scroll_top(50, 100, 24.0, 240.0, 0.0, 0, RevealStrategy::Center),
            Some(1092.0)
        );
    }

    #[test]
    fn reveal_scroll_top_keeps_exact_surrounding_rows_above_and_below() {
        // 100 rows * 24px = 2400px, viewport = 240px (10 rows), surrounding = 3 (72px margin).
        // Moving down from row 0 at scroll_top = 0:
        // Row 6 still has 3 full rows below (7, 8, 9), so no scroll yet.
        assert_eq!(
            reveal_scroll_top(6, 100, 24.0, 240.0, 0.0, 3, RevealStrategy::Nearest),
            Some(0.0)
        );
        // Row 7 needs 3 full rows below (8, 9, 10) -> scroll_top = 24px (rows 1..11 visible).
        assert_eq!(
            reveal_scroll_top(7, 100, 24.0, 240.0, 0.0, 3, RevealStrategy::Nearest),
            Some(24.0)
        );
        // Row 8 -> scroll_top = 48px (rows 2..12 visible).
        assert_eq!(
            reveal_scroll_top(8, 100, 24.0, 240.0, 24.0, 3, RevealStrategy::Nearest),
            Some(48.0)
        );

        // Moving back up from row 8 at scroll_top = 48 (rows 2..12 visible):
        // Rows 7, 6, 5 all have >= 3 rows above (row 5 has rows 2, 3, 4 above) -> no scroll!
        for row in [7, 6, 5] {
            assert_eq!(
                reveal_scroll_top(row, 100, 24.0, 240.0, 48.0, 3, RevealStrategy::Nearest),
                Some(48.0)
            );
        }
        // Row 4 only has rows 2, 3 above -> scrolls up to 24px so rows 1, 2, 3 are above.
        assert_eq!(
            reveal_scroll_top(4, 100, 24.0, 240.0, 48.0, 3, RevealStrategy::Nearest),
            Some(24.0)
        );
        // Row 3 -> scrolls to 0px (rows 0, 1, 2 above).
        assert_eq!(
            reveal_scroll_top(3, 100, 24.0, 240.0, 24.0, 3, RevealStrategy::Nearest),
            Some(0.0)
        );

        // Clamping near the end of the file: max_scroll = 2160 (rows 90..100 visible).
        assert_eq!(
            reveal_scroll_top(95, 100, 24.0, 240.0, 2112.0, 3, RevealStrategy::Nearest),
            Some(2136.0)
        );
        for row in 96..100 {
            assert_eq!(
                reveal_scroll_top(row, 100, 24.0, 240.0, 2136.0, 3, RevealStrategy::Nearest),
                Some(2160.0)
            );
        }
        // Moving back up from row 99 at bottom: row 93 has 3 rows above (90, 91, 92) -> stays at 2160.
        assert_eq!(
            reveal_scroll_top(93, 100, 24.0, 240.0, 2160.0, 3, RevealStrategy::Nearest),
            Some(2160.0)
        );
        // Row 92 scrolls up to 2136 so rows 89, 90, 91 are above.
        assert_eq!(
            reveal_scroll_top(92, 100, 24.0, 240.0, 2160.0, 3, RevealStrategy::Nearest),
            Some(2136.0)
        );
    }

    #[test]
    fn reveal_scroll_top_handles_fractional_viewports_large_margins_center_and_edge_cases() {
        // Fractional viewport (250px = 10.416 rows of 24px), surrounding = 3 (72px):
        // Row 7 bottom is 192px; keeping 72px below requires viewport bottom >= 264 -> scroll_top = 14px.
        assert_eq!(
            reveal_scroll_top(7, 100, 24.0, 250.0, 0.0, 3, RevealStrategy::Nearest),
            Some(14.0)
        );

        // Surrounding lines exceeding half the viewport (e.g. 500) centers the cursor (clamped at ends).
        assert_eq!(
            reveal_scroll_top(50, 100, 24.0, 240.0, 0.0, 500, RevealStrategy::Nearest),
            Some(1092.0)
        );
        assert_eq!(
            reveal_scroll_top(0, 100, 24.0, 240.0, 500.0, 500, RevealStrategy::Nearest),
            Some(0.0)
        );
        assert_eq!(
            reveal_scroll_top(99, 100, 24.0, 240.0, 500.0, 500, RevealStrategy::Nearest),
            Some(2160.0)
        );

        // Center strategy triggers when a visible row encroaches on the surrounding margin,
        // and stays put when the row is already comfortably inside the inner safe band.
        assert_eq!(
            reveal_scroll_top(8, 100, 24.0, 240.0, 0.0, 3, RevealStrategy::Center),
            Some(84.0) // row 8 center = 204 - 120 = 84
        );
        assert_eq!(
            reveal_scroll_top(5, 100, 24.0, 240.0, 0.0, 3, RevealStrategy::Center),
            Some(0.0) // row 5 already has 5 above and 4 below -> unchanged
        );

        // Short file that fits inside the viewport never scrolls past 0.
        assert_eq!(
            reveal_scroll_top(4, 5, 24.0, 240.0, 0.0, 3, RevealStrategy::Nearest),
            Some(0.0)
        );

        // Tiny viewport shorter than one row aligns row top safely without panicking.
        assert_eq!(
            reveal_scroll_top(10, 100, 24.0, 12.0, 0.0, 5, RevealStrategy::Nearest),
            Some(240.0)
        );

        // Invalid or empty inputs return None.
        assert_eq!(
            reveal_scroll_top(0, 0, 24.0, 240.0, 0.0, 3, RevealStrategy::Nearest),
            None
        );
        assert_eq!(
            reveal_scroll_top(0, 10, 0.0, 240.0, 0.0, 3, RevealStrategy::Nearest),
            None
        );
        assert_eq!(
            reveal_scroll_top(0, 10, 24.0, f32::NAN, 0.0, 3, RevealStrategy::Nearest),
            None
        );
    }

    #[test]
    fn scroll_metrics_with_leading_inset_tracks_scrollable_region_and_preserves_max_offset() {
        let handle = UniformListScrollHandle::new();
        {
            let mut state = handle.0.borrow_mut();
            state.last_item_size = Some(gpui::ItemSize {
                item: size(px(800.0), px(400.0)),
                contents: size(px(1600.0), px(2000.0)),
            });
            state.base_handle.set_offset(point(px(-200.0), px(-100.0)));
        }

        let inset =
            ScrollMetrics::from_uniform_list_with_inset(&handle, ScrollAxis::Horizontal, 102.0)
                .expect("horizontal metrics with sticky gutter inset");
        assert_eq!(inset.viewport, 698.0);
        assert_eq!(inset.content, 1498.0);
        assert_eq!(inset.max_offset, 800.0);
        assert_eq!(inset.offset, 200.0);

        // When the viewport is narrower than the leading inset, fall back safely without dropping metrics.
        let narrow_fallback =
            ScrollMetrics::from_uniform_list_with_inset(&handle, ScrollAxis::Horizontal, 900.0)
                .expect("fallback metrics when viewport is narrower than inset");
        assert_eq!(narrow_fallback.viewport, 800.0);
        assert_eq!(narrow_fallback.content, 1600.0);
        assert_eq!(narrow_fallback.max_offset, 800.0);
    }
}
