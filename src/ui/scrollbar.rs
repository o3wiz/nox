use super::theme::Theme;
use gpui::{
    App, Bounds, ColorExt, Corners, CursorStyle, DispatchPhase, HitboxBehavior, MouseButton,
    MouseDownEvent, MouseExitEvent, MouseMoveEvent, PaintQuad, Pixels, Point, Size,
    UniformListScrollHandle, Window, canvas, point, prelude::*, px, size,
};
use std::{cell::Cell, rc::Rc};

pub const DEFAULT_SCROLLBAR_WIDTH: f32 = 10.0;
pub const HORIZONTAL_BAR_HEIGHT: f32 = 8.0;
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
    /// Extracts scrollable metrics for `axis` from a `UniformListScrollHandle` after prepaint.
    /// Returns `None` if the list has not been laid out yet or does not overflow along `axis`.
    pub fn from_uniform_list(handle: &UniformListScrollHandle, axis: ScrollAxis) -> Option<Self> {
        let state = handle.0.borrow();
        let item_size = state.last_item_size?;
        let offset_pt = state.base_handle.offset();
        let (viewport, content, offset) = match axis {
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
        let max_offset = (content - viewport).max(0.0);
        if viewport <= 0.0 || content <= 0.0 || max_offset <= 0.5 {
            return None;
        }
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
    outer_bounds: Bounds<Pixels>,
) -> Option<ScrollbarLayout> {
    let metrics = ScrollMetrics::from_uniform_list(handle, axis)?;
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

/// Applies a positive scroll offset along `axis` to `handle`, preserving the orthogonal axis.
pub fn set_uniform_list_axis_offset(
    handle: &UniformListScrollHandle,
    axis: ScrollAxis,
    positive_offset: f32,
) {
    let state = handle.0.borrow();
    let mut current = state.base_handle.offset();
    let clamped = -positive_offset.max(0.0);
    match axis {
        ScrollAxis::Vertical => current.y = px(clamped),
        ScrollAxis::Horizontal => current.x = px(clamped),
    }
    state.base_handle.set_offset(current);
}

/// Renders a zero-lag scrollbar canvas that reads scroll geometry after sibling `uniform_list` prepaint.
///
/// For `ScrollAxis::Horizontal`, the hitbox uses `HitboxBehavior::BlockMouseExceptScroll` and is
/// only inserted when horizontal overflow exists, so clicks pass cleanly through to rows when no
/// horizontal scrollbar is visible.
pub fn scrollbar_canvas(
    scroll: UniformListScrollHandle,
    axis: ScrollAxis,
    track_state: ScrollbarTrackState,
    dragging: bool,
    theme: Theme,
    on_press: impl Fn(Point<Pixels>, &mut Window, &mut App) + 'static,
) -> gpui::Canvas<(Option<gpui::Hitbox>, Option<ScrollbarLayout>)> {
    let prepaint_state = track_state.clone();
    let paint_state = track_state;
    canvas(
        move |bounds, window, _cx| {
            let layout = compute_scrollbar_layout(&scroll, axis, bounds);
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
}
