use super::{
    icons::Icon,
    theme::{ICON_SM, Theme},
};
use gpui::{prelude::*, *};
use std::time::Duration;

pub const TOOLTIP_DELAY: Duration = Duration::from_millis(220);

/// Floating tooltip pairing a concise action label with an optional keyboard shortcut keycap.
pub struct Tooltip {
    label: SharedString,
    shortcut: Option<SharedString>,
    theme: Theme,
}

impl Tooltip {
    pub fn view(
        label: impl Into<SharedString>,
        shortcut: Option<SharedString>,
        theme: Theme,
        cx: &mut App,
    ) -> AnyView {
        let label = label.into();
        cx.new(move |_| Self {
            label,
            shortcut,
            theme,
        })
        .into()
    }
}

impl Render for Tooltip {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let t = &self.theme;
        div()
            .pl_2()
            .pr_2()
            .py_1()
            .rounded_md()
            .bg(t.elevated)
            .border_1()
            .border_color(t.border)
            .shadow_md()
            .font(t.ui_font_spec())
            .text_size(t.small_size)
            .text_color(t.foreground)
            .flex()
            .items_center()
            .gap_2()
            .whitespace_nowrap()
            .child(self.label.clone())
            .when_some(self.shortcut.clone(), |el, key| {
                el.child(
                    div()
                        .px_1p5()
                        .py_0p5()
                        .rounded_sm()
                        .bg(t.background)
                        .border_1()
                        .border_color(t.border)
                        .font_family(t.mono_font.clone())
                        .text_size(t.xs_size)
                        .text_color(t.muted)
                        .child(key),
                )
            })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ButtonTone {
    Ghost,
    Active,
    Primary,
    Subtle,
}

/// Crisp icon-only button with stable bounds, semantic state styling, and rich tooltip.
pub fn icon_button(
    id: impl Into<ElementId>,
    icon: Icon,
    tooltip: impl Into<SharedString>,
    shortcut: Option<SharedString>,
    tone: ButtonTone,
    theme: &Theme,
) -> Stateful<Div> {
    let tooltip: SharedString = tooltip.into();
    let tip_theme = theme.clone();
    let (bg, border, fg, hover_bg, hover_fg) = match tone {
        ButtonTone::Ghost => (
            gpui::transparent_black(),
            gpui::transparent_black(),
            theme.muted,
            theme.hover,
            theme.foreground,
        ),
        ButtonTone::Subtle => (
            theme.elevated,
            theme.border,
            theme.foreground,
            theme.hover,
            theme.foreground,
        ),
        ButtonTone::Active => (
            theme.accent_subtle,
            theme.accent_border,
            theme.accent,
            theme.accent_subtle,
            theme.accent,
        ),
        ButtonTone::Primary => (
            theme.accent,
            theme.accent,
            theme.on_accent,
            theme.accent.opacity(0.88),
            theme.on_accent,
        ),
    };

    div()
        .id(id)
        .size(px(32.0))
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .rounded_md()
        .border_1()
        .border_color(border)
        .bg(bg)
        .text_color(fg)
        .cursor_pointer()
        .hover(move |s| s.bg(hover_bg).text_color(hover_fg))
        .child(icon.element(ICON_SM))
        .tooltip_show_delay(TOOLTIP_DELAY)
        .tooltip(move |_window, cx| {
            Tooltip::view(tooltip.clone(), shortcut.clone(), tip_theme.clone(), cx)
        })
}

/// Compact 26x26 toggle icon button designed to live inline inside the search bar.
pub fn search_toggle(
    id: impl Into<ElementId>,
    icon: Icon,
    tooltip: impl Into<SharedString>,
    enabled: bool,
    theme: &Theme,
) -> Stateful<Div> {
    let tooltip: SharedString = tooltip.into();
    let tip_theme = theme.clone();
    let (bg, border, fg, hover_bg, hover_fg) = if enabled {
        (
            theme.accent_subtle,
            theme.accent_border,
            theme.accent,
            theme.accent_subtle,
            theme.accent,
        )
    } else {
        (
            gpui::transparent_black(),
            gpui::transparent_black(),
            theme.muted,
            theme.hover,
            theme.foreground,
        )
    };

    div()
        .id(id)
        .size(px(26.0))
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .rounded_sm()
        .border_1()
        .border_color(border)
        .bg(bg)
        .text_color(fg)
        .cursor_pointer()
        .hover(move |s| s.bg(hover_bg).text_color(hover_fg))
        .child(icon.element(px(14.0)))
        .tooltip_show_delay(TOOLTIP_DELAY)
        .tooltip(move |_window, cx| Tooltip::view(tooltip.clone(), None, tip_theme.clone(), cx))
}

/// Vertical hairline separator between logical groups of toolbar icons.
pub fn toolbar_divider(theme: &Theme) -> Div {
    div()
        .w(px(1.0))
        .h(px(18.0))
        .mx_1()
        .flex_shrink_0()
        .bg(theme.border)
}

/// Human-friendly byte count (`4.2 KB`, `12.8 MB`, `512 B`).
pub fn format_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.2} GB", b / GB)
    } else if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if b >= KB {
        format!("{:.1} KB", b / KB)
    } else {
        format!("{bytes} B")
    }
}

/// Formats integers with comma grouping (`12,480`) for scannable telemetry.
pub fn format_count(value: usize) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, ch) in digits.chars().rev().enumerate() {
        if index > 0 && index % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out.chars().rev().collect()
}
