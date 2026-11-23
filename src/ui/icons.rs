use gpui::{Hsla, Pixels, Styled, Svg, svg};

/// Consistent 24x24 vector icon catalog (Phosphor-inspired rounded stroke craft).
/// All SVGs render with explicit white geometry so GPUI tints their alpha mask
/// cleanly using `.text_color(...)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Icon {
    NoxMark,
    FolderOpen,
    ClockHistory,
    StarFilled,
    FileText,
    Refresh,
    FollowTail,
    Bookmark,
    BookmarkFilled,
    Hash,
    Copy,
    Close,
    Search,
    Regex,
    CaseSensitive,
    WholeWord,
    InvertMatch,
    ChevronUp,
    ChevronDown,
    ArrowRight,
    Download,
    ListFilter,
    Lines,
    AlertTriangle,
    Sparkles,
    MatchDot,
    Check,
}

impl Icon {
    #[cfg(test)]
    pub const ALL: &'static [Icon] = &[
        Icon::NoxMark,
        Icon::FolderOpen,
        Icon::ClockHistory,
        Icon::StarFilled,
        Icon::FileText,
        Icon::Refresh,
        Icon::FollowTail,
        Icon::Bookmark,
        Icon::BookmarkFilled,
        Icon::Hash,
        Icon::Copy,
        Icon::Close,
        Icon::Search,
        Icon::Regex,
        Icon::CaseSensitive,
        Icon::WholeWord,
        Icon::InvertMatch,
        Icon::ChevronUp,
        Icon::ChevronDown,
        Icon::ArrowRight,
        Icon::Download,
        Icon::ListFilter,
        Icon::Lines,
        Icon::AlertTriangle,
        Icon::Sparkles,
        Icon::MatchDot,
        Icon::Check,
    ];

    pub fn svg_bytes(self) -> &'static [u8] {
        match self {
            Icon::NoxMark => br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none"><path d="M20.354 15.354A9 9 0 0 1 8.646 3.646 9.003 9.003 0 0 0 12 21a9.003 9.003 0 0 0 8.354-5.646Z" fill="#ffffff"/><circle cx="17.5" cy="6.5" r="1.5" fill="#ffffff"/></svg>"##,

            Icon::FolderOpen => br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#ffffff" stroke-width="1.85" stroke-linecap="round" stroke-linejoin="round"><path d="M3 7a2 2 0 0 1 2-2h4.2a2 2 0 0 1 1.6.8l1.2 1.6a2 2 0 0 0 1.6.8H19a2 2 0 0 1 2 2v1"/><path d="M3 10.5V17a2 2 0 0 0 2 2h12.6a2 2 0 0 0 1.94-1.52l1.2-4.8A1.5 1.5 0 0 0 19.28 11H6.1a1.5 1.5 0 0 0-1.45 1.13L3 18.5"/></svg>"##,

            Icon::ClockHistory => br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#ffffff" stroke-width="1.85" stroke-linecap="round" stroke-linejoin="round"><path d="M3 12a9 9 0 1 0 3-6.7L3 8"/><path d="M3 3v5h5"/><path d="M12 7v5l3.5 2"/></svg>"##,

            Icon::StarFilled => br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="#ffffff"><polygon points="12 2.5 14.95 8.48 21.55 9.44 16.77 14.09 17.9 20.66 12 17.56 6.1 20.66 7.23 14.09 2.45 9.44 9.05 8.48 12 2.5"/></svg>"##,

            Icon::FileText => br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#ffffff" stroke-width="1.85" stroke-linecap="round" stroke-linejoin="round"><path d="M14 2.5H6.5A2 2 0 0 0 4.5 4.5v15a2 2 0 0 0 2 2h11a2 2 0 0 0 2-2V8Z"/><polyline points="14 2.5 14 8 19.5 8"/><line x1="8.5" y1="13" x2="15.5" y2="13"/><line x1="8.5" y1="16.5" x2="13.5" y2="16.5"/></svg>"##,

            Icon::Refresh => br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#ffffff" stroke-width="1.85" stroke-linecap="round" stroke-linejoin="round"><path d="M21 12a9 9 0 0 0-15.5-6.3L3 8"/><path d="M3 3v5h5"/><path d="M3 12a9 9 0 0 0 15.5 6.3L21 16"/><path d="M21 21v-5h-5"/></svg>"##,

            Icon::FollowTail => br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#ffffff" stroke-width="1.85" stroke-linecap="round" stroke-linejoin="round"><line x1="12" y1="4" x2="12" y2="15.5"/><polyline points="7.5 11.5 12 16 16.5 11.5"/><line x1="5" y1="20" x2="19" y2="20"/></svg>"##,

            Icon::Bookmark => br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#ffffff" stroke-width="1.85" stroke-linecap="round" stroke-linejoin="round"><path d="M6.5 4.5A1.5 1.5 0 0 1 8 3h8a1.5 1.5 0 0 1 1.5 1.5v16.5L12 17l-5.5 4V4.5Z"/></svg>"##,

            Icon::BookmarkFilled => br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="#ffffff"><path d="M6.5 4.5A1.5 1.5 0 0 1 8 3h8a1.5 1.5 0 0 1 1.5 1.5v16.5L12 17l-5.5 4V4.5Z"/></svg>"##,

            Icon::Hash => br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#ffffff" stroke-width="1.85" stroke-linecap="round" stroke-linejoin="round"><line x1="4.5" y1="9" x2="19.5" y2="9"/><line x1="4.5" y1="15" x2="19.5" y2="15"/><line x1="10" y1="3.5" x2="8" y2="20.5"/><line x1="16" y1="3.5" x2="14" y2="20.5"/></svg>"##,

            Icon::Copy => br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#ffffff" stroke-width="1.85" stroke-linecap="round" stroke-linejoin="round"><rect x="9" y="9" width="11" height="11" rx="2"/><path d="M5.5 15H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h8a2 2 0 0 1 2 2v.5"/></svg>"##,

            Icon::Close => br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#ffffff" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><line x1="6.5" y1="6.5" x2="17.5" y2="17.5"/><line x1="17.5" y1="6.5" x2="6.5" y2="17.5"/></svg>"##,

            Icon::Search => br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#ffffff" stroke-width="1.85" stroke-linecap="round" stroke-linejoin="round"><circle cx="10.75" cy="10.75" r="6.75"/><line x1="15.8" y1="15.8" x2="20.5" y2="20.5"/></svg>"##,

            Icon::Regex => br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#ffffff" stroke-width="1.9" stroke-linecap="round" stroke-linejoin="round"><circle cx="6.25" cy="17.25" r="1.75" fill="#ffffff" stroke="none"/><line x1="15.5" y1="5.5" x2="15.5" y2="14.5"/><line x1="11.6" y1="7.75" x2="19.4" y2="12.25"/><line x1="19.4" y1="7.75" x2="11.6" y2="12.25"/></svg>"##,

            Icon::CaseSensitive => br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#ffffff" stroke-width="1.85" stroke-linecap="round" stroke-linejoin="round"><path d="M3.5 17.5 7.5 6.5l4 11"/><line x1="5" y1="14" x2="10" y2="14"/><circle cx="17.25" cy="14.25" r="3.25"/><path d="M20.5 11v6.5"/></svg>"##,

            Icon::WholeWord => br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#ffffff" stroke-width="1.85" stroke-linecap="round" stroke-linejoin="round"><path d="M4 16v2.5h16V16"/><path d="M5.5 13 8 6.5l2.5 6.5"/><line x1="6.4" y1="10.8" x2="9.6" y2="10.8"/><path d="M13.5 6.5V13"/><path d="M13.5 10.2a2.4 2.4 0 1 1 0 2.8"/></svg>"##,

            Icon::InvertMatch => br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#ffffff" stroke-width="1.85" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="8.5"/><line x1="6.2" y1="17.8" x2="17.8" y2="6.2"/></svg>"##,

            Icon::ChevronUp => br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#ffffff" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><polyline points="6.5 14.5 12 9 17.5 14.5"/></svg>"##,

            Icon::ChevronDown => br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#ffffff" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><polyline points="6.5 9.5 12 15 17.5 9.5"/></svg>"##,

            Icon::ArrowRight => br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#ffffff" stroke-width="1.95" stroke-linecap="round" stroke-linejoin="round"><line x1="5" y1="12" x2="19" y2="12"/><polyline points="13 6 19 12 13 18"/></svg>"##,

            Icon::Download => br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#ffffff" stroke-width="1.85" stroke-linecap="round" stroke-linejoin="round"><path d="M12 4v11"/><polyline points="7.5 10.5 12 15 16.5 10.5"/><path d="M4.5 16.5v2a2 2 0 0 0 2 2h11a2 2 0 0 0 2-2v-2"/></svg>"##,

            Icon::ListFilter => br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#ffffff" stroke-width="1.85" stroke-linecap="round" stroke-linejoin="round"><line x1="4" y1="6.5" x2="20" y2="6.5"/><line x1="7" y1="12" x2="17" y2="12"/><line x1="10" y1="17.5" x2="14" y2="17.5"/></svg>"##,

            Icon::Lines => br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#ffffff" stroke-width="1.85" stroke-linecap="round" stroke-linejoin="round"><line x1="4.5" y1="6.5" x2="19.5" y2="6.5"/><line x1="4.5" y1="12" x2="19.5" y2="12"/><line x1="4.5" y1="17.5" x2="14.5" y2="17.5"/></svg>"##,

            Icon::AlertTriangle => br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#ffffff" stroke-width="1.85" stroke-linecap="round" stroke-linejoin="round"><path d="M10.3 4.1 2.7 17.5a2 2 0 0 0 1.7 3h15.2a2 2 0 0 0 1.7-3L13.7 4.1a2 2 0 0 0-3.4 0Z"/><line x1="12" y1="9.5" x2="12" y2="13.5"/><circle cx="12" cy="17" r="1" fill="#ffffff" stroke="none"/></svg>"##,

            Icon::Sparkles => br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#ffffff" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M12 3l1.9 5.1L19 10l-5.1 1.9L12 17l-1.9-5.1L5 10l5.1-1.9L12 3Z"/><path d="M18.5 16.5l.8 2.2 2.2.8-2.2.8-.8 2.2-.8-2.2-2.2-.8 2.2-.8.8-2.2Z"/></svg>"##,

            Icon::MatchDot => br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="#ffffff"><circle cx="12" cy="12" r="4.5"/></svg>"##,

            Icon::Check => br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#ffffff" stroke-width="2.1" stroke-linecap="round" stroke-linejoin="round"><polyline points="5 12.5 9.8 17.2 19 7.5"/></svg>"##,
        }
    }

    /// Builds an `Svg` element tinted directly with `color`.
    /// GPUI's `Svg::paint` reads `style.text.color` from the `Svg` itself rather than
    /// inheriting parent text color, so `color` is always applied directly here.
    pub fn element(self, size: Pixels, color: Hsla) -> Svg {
        svg()
            .data(self.svg_bytes())
            .size(size)
            .flex_shrink_0()
            .text_color(color)
    }
}
