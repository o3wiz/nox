//! Interval-backed multi-line selection and character-accurate text selection for log panes.

use std::ops::{Range, RangeInclusive};

/// Tracks the cursor, anchor, and selected row indices for a single list pane.
///
/// Indices are pane-local row indices (`0..log.len()` for the main pane and
/// `0..results.visible.len()` for the filter pane). Selected rows are stored as
/// sorted, disjoint, non-adjacent inclusive intervals `(start, end)` so `Select All`
/// and large Shift-ranges stay O(1) in time and memory.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LineSelection {
    /// Active (lead) row index in this pane.
    pub cursor: usize,
    /// Anchor row index for Shift-click, Shift-arrow, and drag range extensions.
    pub anchor: usize,
    /// Sorted, disjoint, non-adjacent inclusive ranges `(start, end)` with `start <= end`.
    ranges: Vec<(usize, usize)>,
    /// Selection ranges preserved before the current Shift-range extension so shrinking
    /// the active Shift-range does not discard earlier Cmd/Ctrl-toggled selections.
    base_ranges: Vec<(usize, usize)>,
}

impl LineSelection {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn single(row: usize) -> Self {
        Self {
            cursor: row,
            anchor: row,
            ranges: vec![(row, row)],
            base_ranges: Vec::new(),
        }
    }

    pub fn single_clamped(row: usize, len: usize) -> Self {
        if len == 0 {
            Self::empty()
        } else {
            Self::single(row.min(len - 1))
        }
    }

    /// Replaces the entire selection with a single row.
    pub fn select_single(&mut self, row: usize) {
        self.cursor = row;
        self.anchor = row;
        self.ranges.clear();
        self.ranges.push((row, row));
        self.base_ranges.clear();
    }

    /// Extends (or shrinks) the active range between `anchor` and `row`, preserving `base_ranges`.
    pub fn extend_to(&mut self, row: usize) {
        self.cursor = row;
        let active = (self.anchor.min(row), self.anchor.max(row));
        self.ranges = union_with_range(&self.base_ranges, active);
    }

    /// Toggles `row` in or out of the selection (Cmd/Ctrl+click) and moves `cursor`/`anchor` to it.
    pub fn toggle(&mut self, row: usize) {
        if self.contains(row) {
            self.ranges = remove_row(&self.ranges, row);
            self.base_ranges = self.ranges.clone();
        } else {
            self.base_ranges = self.ranges.clone();
            self.ranges = union_with_range(&self.base_ranges, (row, row));
        }
        self.cursor = row;
        self.anchor = row;
    }

    /// Selects every row in `0..len` without moving the cursor past the last row.
    pub fn select_all(&mut self, len: usize) {
        if len == 0 {
            *self = Self::empty();
            return;
        }
        self.anchor = 0;
        self.cursor = self.cursor.min(len - 1);
        self.ranges.clear();
        self.ranges.push((0, len - 1));
        self.base_ranges.clear();
    }

    /// Collapses a multi-selection down to the current cursor row.
    pub fn collapse_to_cursor(&mut self, len: usize) {
        if len == 0 {
            *self = Self::empty();
        } else {
            let row = self.cursor.min(len - 1);
            self.select_single(row);
        }
    }

    /// Returns `true` when more than one row is currently selected.
    pub fn has_multi_selection(&self) -> bool {
        self.ranges.len() > 1 || self.ranges.first().is_some_and(|&(start, end)| end > start)
    }

    /// Swaps the active cursor and anchor ends of a multi-line selection.
    ///
    /// Returns `true` when the cursor or anchor changed.
    pub fn swap_anchor(&mut self) -> bool {
        if !self.has_multi_selection() {
            return false;
        }
        // If `select_all` left a single full range whose endpoints do not match `(anchor, cursor)`,
        // snap to the range's actual endpoints so subsequent extensions adjust the whole range.
        if self.base_ranges.is_empty() && self.ranges.len() == 1 {
            let (start, end) = self.ranges[0];
            if start < end
                && (self.anchor.min(self.cursor), self.anchor.max(self.cursor)) != (start, end)
            {
                if self.cursor == start {
                    self.anchor = start;
                    self.cursor = end;
                } else {
                    self.anchor = end;
                    self.cursor = start;
                }
                return true;
            }
        }
        if self.anchor != self.cursor {
            std::mem::swap(&mut self.cursor, &mut self.anchor);
            return true;
        }
        // When `anchor == cursor` (e.g. after Cmd/Ctrl-clicking an adjacent row that merged into a
        // contiguous range), promote that containing range into the active `(anchor, cursor)` span.
        if let Some((start, end)) = self
            .ranges
            .iter()
            .copied()
            .find(|&(start, end)| start <= self.cursor && self.cursor <= end && start < end)
        {
            self.base_ranges = self
                .ranges
                .iter()
                .copied()
                .filter(|&range| range != (start, end))
                .collect();
            if self.cursor == start {
                self.anchor = start;
                self.cursor = end;
            } else {
                self.anchor = end;
                self.cursor = start;
            }
            return true;
        }
        false
    }

    pub fn contains(&self, row: usize) -> bool {
        self.ranges
            .binary_search_by(|&(start, end)| {
                if row < start {
                    std::cmp::Ordering::Greater
                } else if row > end {
                    std::cmp::Ordering::Less
                } else {
                    std::cmp::Ordering::Equal
                }
            })
            .is_ok()
    }

    pub fn count(&self) -> usize {
        self.ranges
            .iter()
            .map(|&(start, end)| end - start + 1)
            .sum()
    }

    pub fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }

    pub fn ranges(&self) -> &[(usize, usize)] {
        &self.ranges
    }

    pub fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        self.ranges.iter().flat_map(|&(start, end)| start..=end)
    }

    /// Clamps cursor, anchor, and all ranges to `0..len`.
    pub fn clamp(&mut self, len: usize) {
        if len == 0 {
            *self = Self::empty();
            return;
        }
        let max = len - 1;
        self.cursor = self.cursor.min(max);
        self.anchor = self.anchor.min(max);
        clamp_ranges(&mut self.ranges, max);
        clamp_ranges(&mut self.base_ranges, max);
    }

    /// Remaps row indices from `old_lines` into `new_lines` (both sorted ascending) so a
    /// filter-pane selection survives bookmark toggles and live appends under the same query.
    pub fn remap_sorted(&mut self, old_lines: &[usize], new_lines: &[usize]) {
        if new_lines.is_empty() {
            *self = Self::empty();
            return;
        }
        if old_lines == new_lines {
            return;
        }
        if self.is_empty() {
            let mapped_cursor = old_lines
                .get(self.cursor)
                .and_then(|line| new_lines.binary_search(line).ok())
                .unwrap_or_else(|| self.cursor.min(new_lines.len() - 1));
            self.cursor = mapped_cursor;
            self.anchor = mapped_cursor;
            return;
        }

        // Preserve Select-All in O(1) without iterating millions of entries.
        if !old_lines.is_empty() && self.count() == old_lines.len() {
            let mapped_cursor = old_lines
                .get(self.cursor)
                .and_then(|line| new_lines.binary_search(line).ok())
                .unwrap_or_else(|| self.cursor.min(new_lines.len() - 1));
            self.select_all(new_lines.len());
            self.cursor = mapped_cursor;
            return;
        }

        const MAX_REMAP_ROWS: usize = 100_000;
        if self.count() > MAX_REMAP_ROWS {
            self.clamp(new_lines.len());
            return;
        }

        let mut new_ranges = Vec::new();
        for row in self.iter() {
            if let Some(source_line) = old_lines.get(row)
                && let Ok(new_row) = new_lines.binary_search(source_line)
            {
                push_sorted_row(&mut new_ranges, new_row);
            }
        }

        if new_ranges.is_empty() {
            *self = Self::single(0);
            return;
        }

        let mapped_cursor = old_lines
            .get(self.cursor)
            .and_then(|line| new_lines.binary_search(line).ok())
            .unwrap_or(new_ranges[0].0);
        let mapped_anchor = old_lines
            .get(self.anchor)
            .and_then(|line| new_lines.binary_search(line).ok())
            .unwrap_or(mapped_cursor);

        self.cursor = mapped_cursor;
        self.anchor = mapped_anchor;
        self.ranges = new_ranges;
        self.base_ranges.clear();
    }
}

fn push_sorted_row(ranges: &mut Vec<(usize, usize)>, row: usize) {
    if let Some(last) = ranges.last_mut()
        && row <= last.1.saturating_add(1)
    {
        last.1 = last.1.max(row);
        return;
    }
    ranges.push((row, row));
}

fn union_with_range(existing: &[(usize, usize)], new: (usize, usize)) -> Vec<(usize, usize)> {
    let mut out = Vec::with_capacity(existing.len() + 1);
    let mut merged = new;
    let mut inserted = false;
    for &(start, end) in existing {
        if end.saturating_add(1) < merged.0 {
            out.push((start, end));
        } else if merged.1.saturating_add(1) < start {
            if !inserted {
                out.push(merged);
                inserted = true;
            }
            out.push((start, end));
        } else {
            merged.0 = merged.0.min(start);
            merged.1 = merged.1.max(end);
        }
    }
    if !inserted {
        out.push(merged);
    }
    out
}

fn remove_row(existing: &[(usize, usize)], row: usize) -> Vec<(usize, usize)> {
    let mut out = Vec::with_capacity(existing.len() + 1);
    for &(start, end) in existing {
        if row < start || row > end {
            out.push((start, end));
        } else {
            if start < row {
                out.push((start, row - 1));
            }
            if row < end {
                out.push((row + 1, end));
            }
        }
    }
    out
}

fn clamp_ranges(ranges: &mut Vec<(usize, usize)>, max: usize) {
    ranges.retain_mut(|(start, end)| {
        if *start > max {
            false
        } else {
            *end = (*end).min(max);
            true
        }
    });
}

/// A UTF-8 byte position within a displayed pane row.
///
/// `row` is a pane-local row index (`0..log.len()` for the main pane,
/// `0..results.visible.len()` for the filter pane), and `offset` is a UTF-8 byte
/// offset into that row's displayed text.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct TextPosition {
    pub row: usize,
    pub offset: usize,
}

impl TextPosition {
    pub const fn new(row: usize, offset: usize) -> Self {
        Self { row, offset }
    }
}

/// Character-level text selection spanning one or more displayed rows in a single pane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TextSelection {
    pub anchor: TextPosition,
    pub head: TextPosition,
}

impl TextSelection {
    pub const fn new(anchor: TextPosition, head: TextPosition) -> Self {
        Self { anchor, head }
    }

    pub fn is_empty(&self) -> bool {
        self.anchor == self.head
    }

    /// Returns `(start, end)` ordered so `start <= end`.
    pub fn ordered(&self) -> (TextPosition, TextPosition) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }

    pub fn row_range(&self) -> RangeInclusive<usize> {
        let (start, end) = self.ordered();
        start.row..=end.row
    }

    /// Returns the selected UTF-8 byte range within `row`'s `text`, or `None` when `row`
    /// has no highlighted characters. All returned bounds are valid UTF-8 character boundaries.
    pub fn byte_range_for_row(&self, row: usize, text: &str) -> Option<Range<usize>> {
        if self.is_empty() {
            return None;
        }
        let (start, end) = self.ordered();
        if row < start.row || row > end.row {
            return None;
        }
        let len = text.len();
        let (raw_start, raw_end) = if start.row == end.row {
            (start.offset.min(len), end.offset.min(len))
        } else if row == start.row {
            (start.offset.min(len), len)
        } else if row == end.row {
            (0, end.offset.min(len))
        } else {
            (0, len)
        };
        let s = clamp_char_boundary(text, raw_start);
        let e = clamp_char_boundary(text, raw_end);
        (s < e).then_some(s..e)
    }

    /// Clamps row indices to `0..len`. Returns `false` if the selection falls out of range.
    pub fn clamp_rows(&mut self, len: usize) -> bool {
        if len == 0 || self.is_empty() {
            return false;
        }
        let (start, end) = self.ordered();
        start.row < len && end.row < len
    }

    /// Remaps row indices from `old_lines` into `new_lines` (both sorted ascending) so a
    /// filter-pane text selection survives bookmark toggles and live appends when all spanned
    /// rows remain contiguous. Returns `false` if the selection should be cleared.
    pub fn remap_sorted(&mut self, old_lines: &[usize], new_lines: &[usize]) -> bool {
        if new_lines.is_empty() || self.is_empty() {
            return false;
        }
        if old_lines == new_lines {
            return self.clamp_rows(new_lines.len());
        }
        let (start, end) = self.ordered();
        let Some(&start_line) = old_lines.get(start.row) else {
            return false;
        };
        let Some(&end_line) = old_lines.get(end.row) else {
            return false;
        };
        let Ok(new_start_row) = new_lines.binary_search(&start_line) else {
            return false;
        };
        let Ok(new_end_row) = new_lines.binary_search(&end_line) else {
            return false;
        };
        if new_end_row.saturating_sub(new_start_row) != end.row.saturating_sub(start.row) {
            return false;
        }
        for offset in 0..=(end.row - start.row) {
            if old_lines.get(start.row + offset) != new_lines.get(new_start_row + offset) {
                return false;
            }
        }
        if self.anchor <= self.head {
            self.anchor.row = new_start_row;
            self.head.row = new_end_row;
        } else {
            self.anchor.row = new_end_row;
            self.head.row = new_start_row;
        }
        true
    }
}

/// Clamps `offset` to `0..=text.len()` on a valid UTF-8 character boundary (rounding down).
pub fn clamp_char_boundary(text: &str, offset: usize) -> usize {
    let mut idx = offset.min(text.len());
    while idx > 0 && !text.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CharClass {
    Word,
    Whitespace,
    Punctuation,
}

fn char_class(ch: char) -> CharClass {
    if ch.is_alphanumeric() || ch == '_' {
        CharClass::Word
    } else if ch.is_whitespace() {
        CharClass::Whitespace
    } else {
        CharClass::Punctuation
    }
}

/// Returns the byte range of the word (or contiguous whitespace/punctuation run) containing
/// `byte_offset` in `text`. Used for double-click word selection in log rows.
pub fn word_range_at(text: &str, byte_offset: usize) -> Range<usize> {
    if text.is_empty() {
        return 0..0;
    }
    let idx = clamp_char_boundary(text, byte_offset);
    let target_start = if idx < text.len() {
        idx
    } else {
        text.char_indices().next_back().map_or(0, |(i, _)| i)
    };
    let Some(target_char) = text[target_start..].chars().next() else {
        return 0..0;
    };
    let class = char_class(target_char);

    let mut start = target_start;
    for (i, ch) in text[..target_start].char_indices().rev() {
        if char_class(ch) == class {
            start = i;
        } else {
            break;
        }
    }

    let mut end = target_start + target_char.len_utf8();
    for (rel_i, ch) in text[end..].char_indices() {
        if char_class(ch) == class {
            end = target_start + target_char.len_utf8() + rel_i + ch.len_utf8();
        } else {
            break;
        }
    }

    start..end
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shift_extend_grows_and_shrinks_around_anchor() {
        let mut sel = LineSelection::single(5);
        sel.extend_to(8);
        assert_eq!(sel.ranges(), &[(5, 8)]);
        assert_eq!(sel.cursor, 8);
        assert_eq!(sel.anchor, 5);
        assert_eq!(sel.count(), 4);

        sel.extend_to(6);
        assert_eq!(sel.ranges(), &[(5, 6)]);
        assert_eq!(sel.cursor, 6);

        sel.extend_to(3);
        assert_eq!(sel.ranges(), &[(3, 5)]);
        assert_eq!(sel.cursor, 3);
        assert_eq!(sel.anchor, 5);
    }

    #[test]
    fn toggle_adds_splits_merges_and_preserves_base_ranges_for_shift() {
        let mut sel = LineSelection::single(2);
        sel.extend_to(4);
        assert_eq!(sel.ranges(), &[(2, 4)]);

        // Toggle middle item off splits range.
        sel.toggle(3);
        assert_eq!(sel.ranges(), &[(2, 2), (4, 4)]);
        assert!(!sel.contains(3));
        assert!(sel.contains(2));
        assert!(sel.contains(4));

        // Toggle back on merges adjacent ranges.
        sel.toggle(3);
        assert_eq!(sel.ranges(), &[(2, 4)]);

        // Cmd+click distant row then Shift-extend preserves earlier range and shrinks cleanly.
        sel.toggle(8);
        assert_eq!(sel.ranges(), &[(2, 4), (8, 8)]);
        sel.extend_to(10);
        assert_eq!(sel.ranges(), &[(2, 4), (8, 10)]);
        assert_eq!(sel.iter().collect::<Vec<_>>(), vec![2, 3, 4, 8, 9, 10]);

        sel.extend_to(9);
        assert_eq!(sel.ranges(), &[(2, 4), (8, 9)]);
    }

    #[test]
    fn select_all_and_collapse_scale_to_large_counts() {
        let mut sel = LineSelection::single(42);
        sel.select_all(5_000_000);
        assert_eq!(sel.count(), 5_000_000);
        assert_eq!(sel.ranges(), &[(0, 4_999_999)]);
        assert_eq!(sel.cursor, 42);
        assert!(sel.contains(0));
        assert!(sel.contains(4_999_999));
        assert!(!sel.contains(5_000_000));

        sel.collapse_to_cursor(5_000_000);
        assert_eq!(sel.ranges(), &[(42, 42)]);
        assert_eq!(sel.count(), 1);
    }

    #[test]
    fn clamp_handles_truncation_and_empty() {
        let mut sel = LineSelection::single(2);
        sel.extend_to(6);
        sel.toggle(9);
        sel.clamp(5);
        assert_eq!(sel.ranges(), &[(2, 4)]);
        assert_eq!(sel.cursor, 4);
        assert_eq!(sel.anchor, 4);

        sel.clamp(0);
        assert!(sel.is_empty());
        assert_eq!(sel.count(), 0);
    }

    #[test]
    fn remap_sorted_preserves_selected_source_lines() {
        let old = vec![10, 20, 30, 40];
        let new = vec![5, 10, 20, 25, 40];
        let mut sel = LineSelection::single(1); // line 20
        sel.toggle(3); // line 40
        sel.remap_sorted(&old, &new);
        assert_eq!(sel.iter().collect::<Vec<_>>(), vec![2, 4]); // lines 20 and 40
        assert_eq!(sel.cursor, 4);
    }

    #[test]
    fn swap_anchor_toggles_between_top_and_bottom_and_supports_subsequent_shrink_and_extend() {
        let mut sel = LineSelection::single(3);
        assert!(!sel.has_multi_selection());
        assert!(!sel.swap_anchor());

        // Select lines 4..=13 (0-based 3..=12), cursor at bottom (12), anchor at top (3).
        sel.extend_to(12);
        assert!(sel.has_multi_selection());
        assert_eq!(sel.ranges(), &[(3, 12)]);
        assert_eq!((sel.anchor, sel.cursor), (3, 12));

        // Swap moves anchor to bottom (12) and cursor to top (3) without changing selected rows.
        assert!(sel.swap_anchor());
        assert_eq!(sel.ranges(), &[(3, 12)]);
        assert_eq!((sel.anchor, sel.cursor), (12, 3));

        // Extending one line down shrinks from the top to 4..=12 (9 selected, cursor at 4).
        sel.extend_to(4);
        assert_eq!(sel.ranges(), &[(4, 12)]);
        assert_eq!(sel.count(), 9);
        assert_eq!((sel.anchor, sel.cursor), (12, 4));

        // Swapping again moves cursor back to bottom (12) and anchor to top (4).
        assert!(sel.swap_anchor());
        assert_eq!(sel.ranges(), &[(4, 12)]);
        assert_eq!((sel.anchor, sel.cursor), (4, 12));

        // Extending downward grows the bottom end.
        sel.extend_to(14);
        assert_eq!(sel.ranges(), &[(4, 14)]);
        assert_eq!((sel.anchor, sel.cursor), (4, 14));
    }

    #[test]
    fn swap_anchor_handles_select_all_base_ranges_and_merged_toggles() {
        // Select All from a middle row snaps cleanly to the full range's endpoints.
        let mut sel = LineSelection::single(5);
        sel.select_all(18);
        assert!(sel.swap_anchor());
        assert_eq!((sel.anchor, sel.cursor), (17, 0));
        assert_eq!(sel.ranges(), &[(0, 17)]);
        sel.extend_to(1);
        assert_eq!(sel.ranges(), &[(1, 17)]);
        assert!(sel.swap_anchor());
        assert_eq!((sel.anchor, sel.cursor), (1, 17));

        // Select All when cursor starts at row 0 jumps cursor to the bottom row first.
        let mut sel = LineSelection::single(0);
        sel.select_all(10);
        assert!(sel.swap_anchor());
        assert_eq!((sel.anchor, sel.cursor), (0, 9));
        assert!(sel.swap_anchor());
        assert_eq!((sel.anchor, sel.cursor), (9, 0));

        // Disjoint base_ranges are preserved when swapping the active Shift-range.
        let mut sel = LineSelection::single(1);
        sel.extend_to(3);
        sel.toggle(7);
        sel.extend_to(10);
        assert!(sel.swap_anchor());
        assert_eq!((sel.anchor, sel.cursor), (10, 7));
        sel.extend_to(8);
        assert_eq!(sel.ranges(), &[(1, 3), (8, 10)]);

        // Adjacent Cmd/Ctrl-toggled rows merged into a contiguous range promote cleanly.
        let mut sel = LineSelection::single(2);
        sel.toggle(3);
        sel.toggle(4);
        assert_eq!(sel.ranges(), &[(2, 4)]);
        assert_eq!((sel.anchor, sel.cursor), (4, 4));
        assert!(sel.swap_anchor());
        assert_eq!((sel.anchor, sel.cursor), (4, 2));
        sel.extend_to(3);
        assert_eq!(sel.ranges(), &[(3, 4)]);

        // Empty selection is a no-op.
        let mut empty = LineSelection::empty();
        assert!(!empty.has_multi_selection());
        assert!(!empty.swap_anchor());
    }

    #[test]
    fn text_selection_computes_single_and_multi_row_ranges_with_unicode_safety() {
        let single = TextSelection::new(TextPosition::new(2, 11), TextPosition::new(2, 5));
        assert!(!single.is_empty());
        assert_eq!(single.row_range(), 2..=2);
        assert_eq!(single.byte_range_for_row(1, "hello world"), None);
        assert_eq!(
            single.byte_range_for_row(2, "INFO ERROR timeout"),
            Some(5..11)
        );
        assert_eq!(single.byte_range_for_row(3, "hello world"), None);

        // Multi-row reversed selection normalizes cleanly.
        let multi = TextSelection::new(TextPosition::new(4, 3), TextPosition::new(2, 5));
        assert_eq!(multi.row_range(), 2..=4);
        assert_eq!(multi.byte_range_for_row(2, "0123456789"), Some(5..10));
        assert_eq!(multi.byte_range_for_row(3, "middle"), Some(0..6));
        assert_eq!(multi.byte_range_for_row(3, ""), None);
        assert_eq!(multi.byte_range_for_row(4, "abcdef"), Some(0..3));

        // Out-of-bounds or mid-codepoint offsets clamp safely to UTF-8 boundaries.
        let unicode = "שלום世界";
        let sel = TextSelection::new(TextPosition::new(0, 1), TextPosition::new(0, 99));
        let range = sel.byte_range_for_row(0, unicode).unwrap();
        assert_eq!(&unicode[range], unicode);

        // Empty selection yields no range.
        let empty = TextSelection::new(TextPosition::new(1, 4), TextPosition::new(1, 4));
        assert!(empty.is_empty());
        assert_eq!(empty.byte_range_for_row(1, "abcdef"), None);
    }

    #[test]
    fn text_selection_remap_sorted_tracks_contiguous_rows_and_clears_broken_spans() {
        let old = vec![10, 20, 30, 40];
        let new = vec![5, 10, 20, 30, 40];
        let mut sel = TextSelection::new(TextPosition::new(1, 2), TextPosition::new(2, 6));
        assert!(sel.remap_sorted(&old, &new));
        assert_eq!(sel.anchor, TextPosition::new(2, 2));
        assert_eq!(sel.head, TextPosition::new(3, 6));

        // Inserting a row in the middle of a multi-row span invalidates the selection.
        let split = vec![5, 10, 20, 25, 30, 40];
        assert!(!sel.remap_sorted(&new, &split));

        // Single-row selection survives insertions elsewhere.
        let mut single = TextSelection::new(TextPosition::new(2, 1), TextPosition::new(2, 4));
        assert!(single.remap_sorted(&new, &split));
        assert_eq!(single.anchor, TextPosition::new(2, 1));
        assert_eq!(single.head, TextPosition::new(2, 4));

        // Removing the selected row clears the selection.
        assert!(!single.remap_sorted(&split, &[5, 10, 30]));
    }

    #[test]
    fn word_range_at_selects_identifiers_punctuation_whitespace_and_unicode() {
        let line = "INFO user_id=alice_42 [ERROR] שלום";
        assert_eq!(&line[word_range_at(line, 0)], "INFO");
        assert_eq!(&line[word_range_at(line, 6)], "user_id");
        assert_eq!(&line[word_range_at(line, 12)], "=");
        assert_eq!(&line[word_range_at(line, 15)], "alice_42");
        assert_eq!(&line[word_range_at(line, 23)], "ERROR");
        assert_eq!(&line[word_range_at(line, line.len())], "שלום");
        assert_eq!(word_range_at("", 0), 0..0);
    }
}
