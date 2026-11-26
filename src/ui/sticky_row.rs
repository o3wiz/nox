use gpui::{
    AlignItems, AnyElement, App, Bounds, ContentMask, Display, Element, ElementId, FlexDirection,
    GlobalElementId, InspectorElementId, IntoElement, LayoutId, Length, Pixels, Size, Style,
    Window, point, px,
};

/// Computes the horizontal offset needed to pin a row's gutter at the visible left edge of a
/// horizontally-scrolled list viewport, along with the clipped text bounds that exclude the gutter.
pub fn compute_sticky_geometry(
    row_left: Pixels,
    viewport_mask: Bounds<Pixels>,
    gutter_width: Pixels,
) -> (Pixels, Bounds<Pixels>) {
    let sticky_left = viewport_mask.origin.x.max(row_left);
    let sticky_shift_x = (sticky_left - row_left).max(px(0.));
    let clip_left = (sticky_left + gutter_width).max(viewport_mask.origin.x);
    let clip_right = viewport_mask.right().max(clip_left);
    let text_clip = Bounds::from_corners(
        point(clip_left, viewport_mask.origin.y),
        point(clip_right, viewport_mask.bottom()),
    );
    (sticky_shift_x, text_clip)
}

pub struct StickyRowLayoutState {
    gutter_layout_id: LayoutId,
}

/// Lays out a log/filter row's gutter and text side-by-side so intrinsic width measurement matches
/// the full row width, while pinning the gutter to the left viewport edge and clipping text behind
/// it during prepaint and paint.
pub struct StickyRowContent {
    gutter: AnyElement,
    text: AnyElement,
    row_height: Pixels,
}

impl StickyRowContent {
    pub fn new(gutter: AnyElement, text: AnyElement, row_height: Pixels) -> Self {
        Self {
            gutter,
            text,
            row_height,
        }
    }
}

impl Element for StickyRowContent {
    type RequestLayoutState = StickyRowLayoutState;
    type PrepaintState = ContentMask<Pixels>;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let gutter_layout_id = self.gutter.request_layout(window, cx);
        let text_layout_id = self.text.request_layout(window, cx);
        let style = Style {
            display: Display::Flex,
            flex_direction: FlexDirection::Row,
            align_items: Some(AlignItems::Center),
            flex_shrink: 0.0,
            size: Size {
                width: Length::Auto,
                height: self.row_height.into(),
            },
            ..Default::default()
        };
        let layout_id = window.request_layout(style, [gutter_layout_id, text_layout_id], cx);
        (layout_id, StickyRowLayoutState { gutter_layout_id })
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        state: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let current_mask = window.content_mask();
        let gutter_width = window.layout_bounds(state.gutter_layout_id).size.width;
        let (sticky_shift_x, text_clip) =
            compute_sticky_geometry(bounds.origin.x, current_mask.bounds, gutter_width);
        let text_mask = ContentMask {
            bounds: text_clip,
            ..Default::default()
        };

        // Prepaint scrolled text first inside the clipped mask so hitboxes behind the sticky
        // gutter never intercept clicks or switch the pointer cursor to I-beam over line numbers.
        window.with_content_mask(Some(text_mask), |window| {
            self.text.prepaint(window, cx);
        });

        // Prepaint the sticky gutter second, shifted back to the viewport's visible left edge.
        window.with_element_offset(point(sticky_shift_x, px(0.)), |window| {
            self.gutter.prepaint(window, cx);
        });

        text_mask
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _state: &mut Self::RequestLayoutState,
        text_mask: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        window.with_content_mask(Some(*text_mask), |window| {
            self.text.paint(window, cx);
        });
        self.gutter.paint(window, cx);
    }
}

impl IntoElement for StickyRowContent {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::size;

    #[test]
    fn compute_sticky_geometry_keeps_zero_shift_when_unscrolled() {
        let viewport = Bounds::new(point(px(24.0), px(80.0)), size(px(800.0), px(400.0)));
        let (shift_x, text_clip) = compute_sticky_geometry(px(24.0), viewport, px(102.0));
        assert_eq!(shift_x, px(0.0));
        assert_eq!(text_clip.origin, point(px(126.0), px(80.0)));
        assert_eq!(text_clip.right(), px(824.0));
        assert_eq!(text_clip.bottom(), px(480.0));
    }

    #[test]
    fn compute_sticky_geometry_counter_shifts_gutter_and_clips_text_when_scrolled() {
        let viewport = Bounds::new(point(px(24.0), px(80.0)), size(px(800.0), px(400.0)));
        // Row scrolled 240px to the left: row_left = 24 - 240 = -216.
        let (shift_x, text_clip) = compute_sticky_geometry(px(-216.0), viewport, px(102.0));
        assert_eq!(shift_x, px(240.0));
        assert_eq!(px(-216.0) + shift_x, viewport.origin.x);
        assert_eq!(text_clip.origin, point(px(126.0), px(80.0)));
        assert_eq!(text_clip.right(), px(824.0));
    }

    #[test]
    fn compute_sticky_geometry_clamps_text_clip_when_viewport_is_narrower_than_gutter() {
        let viewport = Bounds::new(point(px(0.0), px(0.0)), size(px(60.0), px(200.0)));
        let (shift_x, text_clip) = compute_sticky_geometry(px(-40.0), viewport, px(102.0));
        assert_eq!(shift_x, px(40.0));
        assert_eq!(text_clip.origin.x, px(102.0));
        assert_eq!(text_clip.size.width, px(0.0));
    }
}
