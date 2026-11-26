use gpui::{
    App, Bounds, CursorStyle, DispatchPhase, Display, Element, ElementId, GlobalElementId,
    HighlightStyle, Hitbox, HitboxBehavior, Hsla, InspectorElementId, IntoElement, LayoutId,
    Modifiers, MouseButton, MouseDownEvent, PaintQuad, Pixels, ShapedTextLayout, SharedString,
    Style, StyledText, Window, point,
};
use nox::selection::clamp_char_boundary;
use std::{cell::RefCell, ops::Range, rc::Rc, sync::Arc};

/// Geometry and shaped text for a single visible row captured during prepaint.
#[derive(Clone)]
pub struct VisibleRowLayout {
    pub row: usize,
    pub text: SharedString,
    pub selectable_len: usize,
    pub bounds: Bounds<Pixels>,
    pub clip_left: Pixels,
    pub row_top: Pixels,
    pub row_bottom: Pixels,
    pub line_height: Pixels,
    pub shaped: Option<Arc<ShapedTextLayout>>,
}

impl VisibleRowLayout {
    /// Maps a window X coordinate to the closest UTF-8 character boundary within selectable text.
    pub fn closest_byte_offset(&self, x: Pixels) -> usize {
        let x = x.max(self.clip_left);
        if self.selectable_len == 0 || x <= self.bounds.left() {
            return 0;
        }
        if x >= self.bounds.right() {
            return self.selectable_len;
        }
        let Some(shaped) = &self.shaped else {
            return 0;
        };
        let local = point(x - self.bounds.origin.x, self.line_height * 0.5);
        let raw = shaped
            .closest_byte_index_for_pixel_point(local, self.line_height)
            .unwrap_or_else(|idx| idx);
        clamp_char_boundary(&self.text, raw.min(self.selectable_len))
    }

    /// Maps a window X coordinate to the UTF-8 byte index of the character under the pointer.
    pub fn hit_byte_offset(&self, x: Pixels) -> usize {
        let x = x.max(self.clip_left);
        if self.selectable_len == 0 || x <= self.bounds.left() {
            return 0;
        }
        if x >= self.bounds.right() {
            return self.selectable_len;
        }
        let Some(shaped) = &self.shaped else {
            return 0;
        };
        let local = point(x - self.bounds.origin.x, self.line_height * 0.5);
        let raw = shaped
            .byte_index_for_pixel_point(local, self.line_height)
            .unwrap_or_else(|idx| idx);
        clamp_char_boundary(&self.text, raw.min(self.selectable_len))
    }
}

#[derive(Default)]
struct RowLayoutCacheInner {
    frame: u64,
    rows: Vec<VisibleRowLayout>,
}

/// Per-pane cache of prepainted visible row layouts for accurate drag hit-testing.
#[derive(Clone, Default)]
pub struct RowLayoutCache {
    inner: Rc<RefCell<RowLayoutCacheInner>>,
}

impl RowLayoutCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn begin_frame(&self, frame: u64) {
        let mut inner = self.inner.borrow_mut();
        if inner.frame != frame {
            inner.frame = frame;
            inner.rows.clear();
        }
    }

    pub fn clear(&self) {
        self.inner.borrow_mut().rows.clear();
    }

    pub fn record(&self, frame: u64, entry: VisibleRowLayout) {
        let mut inner = self.inner.borrow_mut();
        if inner.frame != frame {
            inner.frame = frame;
            inner.rows.clear();
        }
        if let Some(existing) = inner.rows.iter_mut().find(|r| r.row == entry.row) {
            *existing = entry;
        } else {
            inner.rows.push(entry);
        }
    }

    /// Returns the visible row containing `y`, or the nearest visible row vertically when
    /// dragging above/below the pane bounds.
    pub fn row_at_y(&self, y: Pixels) -> Option<VisibleRowLayout> {
        let inner = self.inner.borrow();
        if inner.rows.is_empty() {
            return None;
        }
        if let Some(exact) = inner
            .rows
            .iter()
            .find(|entry| y >= entry.row_top && y <= entry.row_bottom)
        {
            return Some(exact.clone());
        }
        inner
            .rows
            .iter()
            .min_by(|a, b| {
                let dist_a = if y < a.row_top {
                    a.row_top - y
                } else {
                    y - a.row_bottom
                };
                let dist_b = if y < b.row_top {
                    b.row_top - y
                } else {
                    y - b.row_bottom
                };
                dist_a
                    .partial_cmp(&dist_b)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .cloned()
    }
}

/// Details delivered when the user presses the left mouse button on a row's text.
pub struct TextPressEvent {
    pub row: usize,
    pub closest_offset: usize,
    pub hit_offset: usize,
    pub text: SharedString,
    pub selectable_len: usize,
    pub modifiers: Modifiers,
    pub click_count: usize,
}

type TextMouseDownHandler = Box<dyn Fn(TextPressEvent, &mut Window, &mut App) -> bool>;

/// Text element for a log/filter row that paints a configurable translucent selection box
/// between match highlights and crisp foreground glyphs, records layout geometry for multi-row
/// drag selection, and reports mouse presses on the text.
pub struct SelectableRowText {
    styled: StyledText,
    raw_text: SharedString,
    selectable_len: usize,
    row: usize,
    row_height: Pixels,
    frame: u64,
    cache: RowLayoutCache,
    selection_range: Option<Range<usize>>,
    selection_color: Hsla,
    on_mouse_down: Option<TextMouseDownHandler>,
}

impl SelectableRowText {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        text: impl Into<SharedString>,
        selectable_len: usize,
        highlights: Vec<(Range<usize>, HighlightStyle)>,
        row: usize,
        row_height: Pixels,
        frame: u64,
        cache: RowLayoutCache,
        selection_range: Option<Range<usize>>,
        selection_color: Hsla,
        on_mouse_down: impl Fn(TextPressEvent, &mut Window, &mut App) -> bool + 'static,
    ) -> Self {
        let raw_text: SharedString = text.into();
        let selectable_len = clamp_char_boundary(&raw_text, selectable_len);
        let styled = if highlights.is_empty() {
            StyledText::new(raw_text.clone())
        } else {
            StyledText::new(raw_text.clone()).with_highlights(highlights)
        };
        Self {
            styled,
            raw_text,
            selectable_len,
            row,
            row_height,
            frame,
            cache,
            selection_range,
            selection_color,
            on_mouse_down: Some(Box::new(on_mouse_down)),
        }
    }
}

impl Element for SelectableRowText {
    type RequestLayoutState = ();
    type PrepaintState = (Hitbox, VisibleRowLayout);

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let (text_layout_id, state) = self.styled.request_layout(id, inspector_id, window, cx);
        let style = Style {
            display: Display::Flex,
            ..Default::default()
        };
        let layout_id = window.request_layout(style, [text_layout_id], cx);
        (layout_id, state)
    }

    fn prepaint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        state: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        self.styled
            .prepaint(id, inspector_id, bounds, state, window, cx);
        let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
        let clip_left = window.content_mask().bounds.left();
        let layout = self.styled.layout();
        let text_bounds = layout.bounds();
        let line_height = layout.line_height();
        let shaped = layout.line_layout_for_index(0);
        let half_row = self.row_height * 0.5;
        let center_y = text_bounds.center().y;
        let visible = VisibleRowLayout {
            row: self.row,
            text: self.raw_text.clone(),
            selectable_len: self.selectable_len,
            bounds: text_bounds,
            clip_left,
            row_top: center_y - half_row,
            row_bottom: center_y + half_row,
            line_height,
            shaped,
        };
        self.cache.record(self.frame, visible.clone());
        (hitbox, visible)
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _state: &mut Self::RequestLayoutState,
        (hitbox, visible): &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        if visible.selectable_len > 0 {
            window.set_cursor_style(CursorStyle::IBeam, hitbox);
        }

        let text_layout = self.styled.layout().clone();
        let _ = text_layout.paint_background(window, cx);

        if let Some(range) = &self.selection_range
            && !range.is_empty()
            && let Some(shaped) = &visible.shaped
        {
            for rect in shaped.selection_bounds(range.clone(), visible.line_height) {
                let quad_bounds = Bounds::new(visible.bounds.origin + rect.origin, rect.size);
                window.paint_quad(PaintQuad::new(quad_bounds).background(self.selection_color));
            }
        }

        let _ = text_layout.paint_foreground(window, cx);

        if let Some(on_mouse_down) = self.on_mouse_down.take()
            && visible.selectable_len > 0
        {
            let hitbox = hitbox.clone();
            let visible = visible.clone();
            window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
                if phase == DispatchPhase::Bubble
                    && event.button == MouseButton::Left
                    && hitbox.is_hovered(window)
                {
                    let closest_offset = visible.closest_byte_offset(event.position.x);
                    let hit_offset = visible.hit_byte_offset(event.position.x);
                    let press = TextPressEvent {
                        row: visible.row,
                        closest_offset,
                        hit_offset,
                        text: visible.text.clone(),
                        selectable_len: visible.selectable_len,
                        modifiers: event.modifiers,
                        click_count: event.click_count,
                    };
                    if on_mouse_down(press, window, cx) {
                        cx.stop_propagation();
                    }
                }
            });
        }
    }
}

impl IntoElement for SelectableRowText {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}
