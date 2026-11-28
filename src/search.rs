use crate::{
    config::{Preset, PresetToggle, SearchConfig, SearchMode},
    logfile::LogFile,
    selection::LineSelection,
};
use anyhow::{Result, ensure};
use regex::{Regex, RegexBuilder};
use std::{
    borrow::Cow,
    collections::BTreeSet,
    io::BufRead,
    ops::Range,
    sync::atomic::{AtomicBool, Ordering},
};

fn ends_with_unescaped_pipe(pattern: &str) -> bool {
    let Some(prefix) = pattern.strip_suffix('|') else {
        return false;
    };
    let trailing_backslashes = prefix.bytes().rev().take_while(|&b| b == b'\\').count();
    trailing_backslashes.is_multiple_of(2)
}

/// Appends `addition` to `existing` as a regex `|` alternative, avoiding duplicate `||` when
/// `existing` already ends with an unescaped `|`. Returns the combined pattern and the UTF-8 byte
/// offset where `addition` starts inside it.
pub fn append_regex_alternative_with_offset(existing: &str, addition: &str) -> (String, usize) {
    if existing.is_empty() {
        (addition.to_owned(), 0)
    } else if ends_with_unescaped_pipe(existing) {
        (format!("{existing}{addition}"), existing.len())
    } else {
        (format!("{existing}|{addition}"), existing.len() + 1)
    }
}

/// Appends `addition` to `existing` as a regex `|` alternative.
pub fn append_regex_alternative(existing: &str, addition: &str) -> String {
    append_regex_alternative_with_offset(existing, addition).0
}

/// Result of composing a [`Preset`] onto an existing `SearchMode::Regex` search pattern.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppendPresetResult {
    pub pattern: String,
    pub select_range: Option<Range<usize>>,
    pub case_sensitive: bool,
    pub whole_word: bool,
    pub highlight_matches: bool,
}

/// Composes `preset` onto `existing` for an active `SearchMode::Regex` search while preserving
/// exact matching semantics across `mode`, `match_case`, `whole_word`, and `select_text`.
pub fn append_preset_to_regex(
    existing: &str,
    active: &SearchConfig,
    preset: &Preset,
) -> AppendPresetResult {
    let (raw_addition, raw_select_range) = preset.regex_pattern_and_select_range();
    if existing.is_empty() {
        let mut case_sensitive = active.case_sensitive;
        let mut whole_word = active.whole_word;
        preset.match_case.apply_to(&mut case_sensitive);
        preset.whole_word.apply_to(&mut whole_word);
        return AppendPresetResult {
            pattern: raw_addition,
            select_range: raw_select_range,
            case_sensitive,
            whole_word,
            highlight_matches: preset.highlight_match,
        };
    }

    let case_sensitive = active.case_sensitive;
    let (base_existing, whole_word) = if active.whole_word && preset.whole_word == PresetToggle::Off
    {
        let core = if ends_with_unescaped_pipe(existing) && existing.len() > 1 {
            &existing[..existing.len() - 1]
        } else {
            existing
        };
        (Cow::Owned(format!(r"\b(?:{core})\b")), false)
    } else {
        (Cow::Borrowed(existing), active.whole_word)
    };

    let case_override = preset
        .match_case
        .forced_value()
        .filter(|&wanted| wanted != case_sensitive);
    let wrap_word = preset.whole_word == PresetToggle::On && !whole_word;

    let (prefix, suffix) = match (case_override, wrap_word) {
        (None, false) => ("", ""),
        (Some(true), false) => ("(?-i:", ")"),
        (Some(false), false) => ("(?i:", ")"),
        (None, true) => (r"\b(?:", r")\b"),
        (Some(true), true) => (r"\b(?-i:", r")\b"),
        (Some(false), true) => (r"\b(?i:", r")\b"),
    };

    let wrapped_addition = if prefix.is_empty() {
        raw_addition
    } else {
        format!("{prefix}{raw_addition}{suffix}")
    };
    let wrapped_select_range =
        raw_select_range.map(|range| (range.start + prefix.len())..(range.end + prefix.len()));

    let (combined, offset) =
        append_regex_alternative_with_offset(&base_existing, &wrapped_addition);
    let select_range =
        wrapped_select_range.map(|range| (range.start + offset)..(range.end + offset));

    AppendPresetResult {
        pattern: combined,
        select_range,
        case_sensitive,
        whole_word,
        highlight_matches: active.highlight_matches,
    }
}

pub fn compile(pattern: &str, options: &SearchConfig) -> Result<Regex> {
    let pattern = if options.mode == SearchMode::Literal {
        regex::escape(pattern)
    } else {
        pattern.to_owned()
    };
    let pattern = if options.whole_word {
        format!(r"\b(?:{pattern})\b")
    } else {
        pattern
    };
    Ok(RegexBuilder::new(&pattern)
        .case_insensitive(!options.case_sensitive)
        .build()?)
}
#[derive(Debug, Default)]
pub struct SearchResults {
    pub matches: Vec<usize>,
    pub visible: Vec<usize>,
    pub limited: bool,
    pub widest_row: usize,
}
pub fn visible_rows(
    log: &LogFile,
    matches: &[usize],
    context_lines: usize,
    bookmarks: &BTreeSet<usize>,
) -> (Vec<usize>, usize) {
    let len = log.len();
    if len == 0 {
        return (Vec::new(), 0);
    }
    let last = len - 1;
    let visible = if matches.windows(2).all(|w| w[0] <= w[1]) {
        let mut from_matches = Vec::with_capacity(matches.len());
        let mut next_start = 0;
        for &index in matches {
            if index >= len {
                break;
            }
            let start = index.saturating_sub(context_lines).max(next_start);
            let end = index.saturating_add(context_lines).min(last);
            if start <= end {
                from_matches.extend(start..=end);
                next_start = end + 1;
            }
        }
        let mut merged = Vec::with_capacity(from_matches.len() + bookmarks.len());
        let mut m_iter = from_matches.into_iter().peekable();
        let mut b_iter = bookmarks
            .iter()
            .copied()
            .take_while(|&b| b < len)
            .peekable();
        loop {
            match (m_iter.peek().copied(), b_iter.peek().copied()) {
                (Some(m), Some(b)) if m < b => {
                    merged.push(m);
                    m_iter.next();
                }
                (Some(m), Some(b)) if b < m => {
                    merged.push(b);
                    b_iter.next();
                }
                (Some(m), Some(_)) => {
                    merged.push(m);
                    m_iter.next();
                    b_iter.next();
                }
                (Some(_), None) => {
                    merged.extend(m_iter);
                    break;
                }
                (None, Some(_)) => {
                    merged.extend(b_iter);
                    break;
                }
                (None, None) => break,
            }
        }
        merged
    } else {
        let mut set = BTreeSet::new();
        for &index in matches {
            if index < len {
                set.extend(
                    index.saturating_sub(context_lines)
                        ..=index.saturating_add(context_lines).min(last),
                );
            }
        }
        set.extend(bookmarks.iter().copied().filter(|&index| index < len));
        set.into_iter().collect()
    };
    let widest_row = widest(log, &visible);
    (visible, widest_row)
}

pub fn search(
    log: &LogFile,
    pattern: &str,
    options: &SearchConfig,
    bookmarks: &BTreeSet<usize>,
    cancel: &AtomicBool,
) -> Result<SearchResults> {
    if pattern.is_empty() {
        let (visible, widest_row) = visible_rows(log, &[], options.context_lines, bookmarks);
        return Ok(SearchResults {
            visible,
            widest_row,
            ..Default::default()
        });
    }
    let regex = compile(pattern, options)?;
    let mut result = SearchResults::default();
    let mut reader = log.lines();
    let mut bytes = Vec::new();
    let mut index = 0;
    while index < log.len() {
        ensure!(!cancel.load(Ordering::Relaxed), "Cancelled");
        bytes.clear();
        if reader.read_until(b'\n', &mut bytes)? == 0 {
            break;
        }
        while matches!(bytes.last(), Some(b'\n' | b'\r')) {
            bytes.pop();
        }
        let text = String::from_utf8_lossy(&bytes);
        if regex.is_match(&text) != options.invert {
            if result.matches.len() == options.max_results {
                result.limited = true;
                break;
            }
            result.matches.push(index);
        }
        index += 1;
    }
    let (visible, widest_row) =
        visible_rows(log, &result.matches, options.context_lines, bookmarks);
    result.visible = visible;
    result.widest_row = widest_row;
    Ok(result)
}

fn widest(log: &LogFile, rows: &[usize]) -> usize {
    rows.iter()
        .enumerate()
        .max_by_key(|(_, line)| log.line_bytes(**line))
        .map_or(0, |(index, _)| index)
}

/// Returns sorted, non-overlapping, non-empty UTF-8 byte ranges in `text` to highlight
/// for the compiled `regex` and `mode`.
///
/// - In `SearchMode::Regex`:
///   - When the pattern defines capturing groups (`regex.captures_len() > 1`), highlights all
///     non-empty matched capturing groups (`1..regex.captures_len()`).
///   - When the pattern has no capturing groups (`regex.captures_len() <= 1`), highlights the
///     full match (group `0`).
/// - In `SearchMode::Literal`:
///   - Highlights every non-empty literal match.
///
/// Overlapping or adjacent ranges are merged so the output satisfies `StyledText::with_highlights`.
pub fn highlight_ranges(text: &str, regex: &Regex, mode: SearchMode) -> Vec<Range<usize>> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut ranges = Vec::new();
    if mode == SearchMode::Regex && regex.captures_len() > 1 {
        for captures in regex.captures_iter(text) {
            for group in 1..regex.captures_len() {
                if let Some(matched) = captures.get(group)
                    && matched.start() < matched.end()
                {
                    ranges.push(matched.start()..matched.end());
                }
            }
        }
    } else {
        for matched in regex.find_iter(text) {
            if matched.start() < matched.end() {
                ranges.push(matched.start()..matched.end());
            }
        }
    }
    normalize_ranges(ranges)
}

fn normalize_ranges(mut ranges: Vec<Range<usize>>) -> Vec<Range<usize>> {
    if ranges.len() <= 1 {
        return ranges;
    }
    ranges.sort_unstable_by_key(|range| (range.start, range.end));
    let mut merged: Vec<Range<usize>> = Vec::with_capacity(ranges.len());
    for range in ranges {
        if let Some(last) = merged.last_mut()
            && range.start <= last.end
        {
            last.end = last.end.max(range.end);
        } else {
            merged.push(range);
        }
    }
    merged
}

/// Expands `\t` characters in `text` to `tab_width` spaces while remapping sorted,
/// non-overlapping highlight byte `ranges` into the expanded string's byte offsets.
pub fn expand_tabs_with_highlights(
    text: &str,
    ranges: &[Range<usize>],
    tab_width: usize,
) -> (String, Vec<Range<usize>>) {
    if !text.contains('\t') {
        return (text.to_owned(), ranges.to_vec());
    }
    let tab_width = tab_width.max(1);
    let spaces = " ".repeat(tab_width);
    if ranges.is_empty() {
        return (text.replace('\t', &spaces), Vec::new());
    }
    let tab_count = text.bytes().filter(|&b| b == b'\t').count();
    let mut expanded = String::with_capacity(text.len() + tab_count * (tab_width - 1));
    let mut mapped = Vec::with_capacity(ranges.len());
    let mut range_idx = 0;
    let mut open_start: Option<usize> = None;
    let mut extra = 0usize;
    let mut chunk_start = 0usize;

    for (byte_idx, &byte) in text.as_bytes().iter().enumerate() {
        while range_idx < ranges.len()
            && (ranges[range_idx].start >= ranges[range_idx].end
                || ranges[range_idx].end > text.len())
        {
            range_idx += 1;
        }
        if range_idx < ranges.len() && open_start.is_none() && byte_idx == ranges[range_idx].start {
            open_start = Some(byte_idx + extra);
        }
        if byte == b'\t' {
            if chunk_start < byte_idx {
                expanded.push_str(&text[chunk_start..byte_idx]);
            }
            expanded.push_str(&spaces);
            extra += tab_width - 1;
            chunk_start = byte_idx + 1;
        }
        if let Some(start) = open_start
            && byte_idx + 1 == ranges[range_idx].end
        {
            mapped.push(start..(byte_idx + 1 + extra));
            open_start = None;
            range_idx += 1;
        }
    }
    if chunk_start < text.len() {
        expanded.push_str(&text[chunk_start..]);
    }
    (expanded, mapped)
}

/// Outcome of collecting match results for clipboard copying.
#[derive(Debug, PartialEq, Eq)]
pub enum MatchCopyResult {
    /// Formatted clipboard text and number of matched lines included.
    Copied { text: String, count: usize },
    /// No matched lines or no non-empty highlightable match text to copy.
    Empty,
    /// Formatted output would exceed the configured byte limit.
    LimitExceeded,
}

/// Returns sorted, deduplicated source line indices that are both selected by `selection`
/// and present in sorted `matches`.
///
/// - When `visible` is `None`, `selection` stores source line indices directly (`0..log_len`,
///   for the main log pane).
/// - When `visible` is `Some(rows)`, `selection` stores pane-local row indices into the
///   sorted `rows` slice (for the filter pane).
pub fn selected_match_lines(
    matches: &[usize],
    selection: &LineSelection,
    visible: Option<&[usize]>,
    log_len: usize,
) -> Vec<usize> {
    if matches.is_empty() || selection.is_empty() || log_len == 0 {
        return Vec::new();
    }
    let last_line = log_len - 1;
    let mut out = Vec::new();
    match visible {
        None => {
            for &(start, end) in selection.ranges() {
                if start > last_line {
                    break;
                }
                let end = end.min(last_line);
                let lo = matches.partition_point(|&line| line < start);
                let hi = matches.partition_point(|&line| line <= end);
                out.extend_from_slice(&matches[lo..hi]);
            }
        }
        Some(visible_rows) => {
            if visible_rows.is_empty() {
                return out;
            }
            let last_row = visible_rows.len() - 1;
            for &(start_row, end_row) in selection.ranges() {
                if start_row > last_row {
                    break;
                }
                let end_row = end_row.min(last_row);
                let slice = &visible_rows[start_row..=end_row];
                let first_line = slice[0];
                if first_line > last_line {
                    break;
                }
                let capped_last = slice[slice.len() - 1].min(last_line);
                let lo = matches.partition_point(|&line| line < first_line);
                let hi = matches.partition_point(|&line| line <= capped_last);
                let mut rest = slice;
                for &line in &matches[lo..hi] {
                    if rest.first() == Some(&line) {
                        out.push(line);
                        rest = &rest[1..];
                    } else if let Ok(idx) = rest.binary_search(&line) {
                        out.push(line);
                        rest = &rest[idx + 1..];
                    }
                }
            }
        }
    }
    out
}

/// Collects text to copy for the given matched source lines in `matches`.
///
/// - In `SearchMode::Regex` (when `options.invert` is false and `regex` is `Some`),
///   extracts only the spans that match highlighting would highlight
///   ([`highlight_ranges`]: capturing groups when defined, otherwise full matches),
///   regardless of `options.highlight_matches`. Multiple spans on the same line are
///   separated by a single space (`' '`), and lines are separated by `'\n'`. Matched
///   lines with no non-empty highlight ranges are skipped.
/// - In `SearchMode::Literal` (or when `options.invert` is true), copies the full
///   matched lines separated by `'\n'`.
pub fn extract_match_results(
    log: &LogFile,
    matches: &[usize],
    regex: Option<&Regex>,
    options: &SearchConfig,
    max_bytes: u64,
) -> Result<MatchCopyResult> {
    if matches.is_empty() {
        return Ok(MatchCopyResult::Empty);
    }

    let mut copied = String::new();
    let mut copied_count = 0usize;

    if options.mode == SearchMode::Regex && !options.invert {
        let Some(regex) = regex else {
            return Ok(MatchCopyResult::Empty);
        };
        for &line in matches {
            let text = log.line(line, usize::MAX)?;
            let ranges = highlight_ranges(&text, regex, SearchMode::Regex);
            if ranges.is_empty() {
                continue;
            }
            let line_len = ranges
                .iter()
                .map(|r| r.len() as u64)
                .fold(0u64, u64::saturating_add)
                .saturating_add((ranges.len() - 1) as u64);
            let sep = u64::from(copied_count > 0);
            if (copied.len() as u64)
                .saturating_add(sep)
                .saturating_add(line_len)
                > max_bytes
            {
                return Ok(MatchCopyResult::LimitExceeded);
            }
            if copied_count > 0 {
                copied.push('\n');
            }
            for (idx, range) in ranges.into_iter().enumerate() {
                if idx > 0 {
                    copied.push(' ');
                }
                copied.push_str(&text[range]);
            }
            copied_count += 1;
        }
    } else {
        for &line in matches {
            let sep = u64::from(copied_count > 0);
            // Fast pre-check before reading an oversized single line from disk.
            // Raw disk bytes may include up to 2 line-ending bytes (`\r\n`).
            let min_line_bytes = log.line_bytes(line).saturating_sub(2);
            if (copied.len() as u64)
                .saturating_add(sep)
                .saturating_add(min_line_bytes)
                > max_bytes
            {
                return Ok(MatchCopyResult::LimitExceeded);
            }
            let text = log.line(line, usize::MAX)?;
            if (copied.len() as u64)
                .saturating_add(sep)
                .saturating_add(text.len() as u64)
                > max_bytes
            {
                return Ok(MatchCopyResult::LimitExceeded);
            }
            if copied_count > 0 {
                copied.push('\n');
            }
            copied.push_str(&text);
            copied_count += 1;
        }
    }

    if copied_count == 0 {
        Ok(MatchCopyResult::Empty)
    } else {
        Ok(MatchCopyResult::Copied {
            text: copied,
            count: copied_count,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn ranges_for(text: &str, pattern: &str, mode: SearchMode) -> Vec<Range<usize>> {
        let mut opts = Config::load(None).unwrap().search;
        opts.mode = mode;
        let regex = compile(pattern, &opts).unwrap();
        highlight_ranges(text, &regex, mode)
    }

    #[test]
    fn highlights_literal_matches_respecting_case_and_whole_word() {
        let mut opts = Config::load(None).unwrap().search;
        opts.mode = SearchMode::Literal;
        let re = compile("(a.b)", &opts).unwrap();
        assert_eq!(
            highlight_ranges("x (a.b) (axb) (A.B)", &re, opts.mode),
            vec![2..7, 14..19]
        );

        opts.case_sensitive = true;
        let re = compile("(a.b)", &opts).unwrap();
        assert_eq!(
            highlight_ranges("x (a.b) (A.B)", &re, opts.mode),
            vec![2..7]
        );

        opts.case_sensitive = false;
        opts.whole_word = true;
        let re = compile("err", &opts).unwrap();
        assert_eq!(
            highlight_ranges("error err ERR_1 err", &re, opts.mode),
            vec![6..9, 16..19]
        );
    }

    #[test]
    fn highlights_full_match_in_regex_mode_when_no_capturing_groups_exist() {
        assert_eq!(
            ranges_for("INFO ok ERROR fail WARN", "ERROR|WARN", SearchMode::Regex),
            vec![8..13, 19..23]
        );
        // Non-capturing groups do not count as capturing groups.
        assert_eq!(
            ranges_for("foo bar baz", "(?:foo|baz)", SearchMode::Regex),
            vec![0..3, 8..11]
        );
        // Whole-word wrapping uses a non-capturing group, preserving full-match behavior.
        let mut opts = Config::load(None).unwrap().search;
        opts.mode = SearchMode::Regex;
        opts.whole_word = true;
        let re = compile("err|warn", &opts).unwrap();
        assert_eq!(
            highlight_ranges("error err warn warning", &re, opts.mode),
            vec![6..9, 10..14]
        );
    }

    #[test]
    fn highlights_only_capturing_groups_in_regex_mode() {
        // Single group
        assert_eq!(
            ranges_for(
                "req latency=124ms ok",
                r"latency=(\d+ms)",
                SearchMode::Regex
            ),
            vec![12..17]
        );
        // Multiple groups in one match
        assert_eq!(
            ranges_for(
                "user=alice path=/api status=503",
                r"user=(\w+).*status=(\d{3})",
                SearchMode::Regex,
            ),
            vec![5..10, 28..31]
        );
        // Named groups
        assert_eq!(
            ranges_for(
                "host=api:8080",
                r"(?P<host>[a-z]+):(?P<port>\d+)",
                SearchMode::Regex,
            ),
            vec![5..8, 9..13]
        );
        // Multiple matches on one line
        assert_eq!(
            ranges_for("p50=12ms p99=124ms", r"(\d+)ms", SearchMode::Regex),
            vec![4..6, 13..16]
        );
    }

    #[test]
    fn normalizes_nested_overlapping_optional_and_zero_length_matches() {
        // Nested & overlapping groups merge cleanly without overlapping ranges.
        assert_eq!(
            ranges_for("user=alice", r"(user=(\w+))", SearchMode::Regex),
            vec![0..10]
        );
        // Adjacent matches collapse into one run.
        assert_eq!(ranges_for("abcd", ".", SearchMode::Regex), vec![0..4]);
        // Optional alternation groups highlight only participating groups.
        assert_eq!(
            ranges_for("ERROR and WARN", r"(ERROR)|(WARN)", SearchMode::Regex),
            vec![0..5, 10..14]
        );
        // Pattern with capturing group where optional group did not participate yields empty.
        assert_eq!(
            ranges_for("foo baz", r"foo(bar)?", SearchMode::Regex),
            Vec::<Range<usize>>::new()
        );
        // Zero-length matches and empty capture groups are skipped cleanly.
        assert_eq!(
            ranges_for("abc", r"^|$", SearchMode::Regex),
            Vec::<Range<usize>>::new()
        );
        assert_eq!(
            ranges_for("abaa", r"a*", SearchMode::Regex),
            vec![0..1, 2..4]
        );
        assert_eq!(
            ranges_for("foobar", r"foo()bar", SearchMode::Regex),
            Vec::<Range<usize>>::new()
        );
        // Empty text returns empty.
        assert_eq!(
            ranges_for("", "foo", SearchMode::Regex),
            Vec::<Range<usize>>::new()
        );
    }

    #[test]
    fn highlights_unicode_and_maps_tab_expansion_offsets() {
        let text = "שלום\tERROR\t世界";
        let ranges = ranges_for(text, r"(ERROR)|(世界)", SearchMode::Regex);
        let (expanded, mapped) = expand_tabs_with_highlights(text, &ranges, 4);
        assert_eq!(expanded, "שלום    ERROR    世界");
        assert_eq!(
            mapped
                .iter()
                .map(|r| &expanded[r.clone()])
                .collect::<Vec<_>>(),
            vec!["ERROR", "世界"]
        );

        // Leading tab and match spanning a tab itself.
        let tab_text = "\ta\tb";
        let tab_ranges = ranges_for(tab_text, r"\ta\t", SearchMode::Regex);
        let (expanded, mapped) = expand_tabs_with_highlights(tab_text, &tab_ranges, 4);
        assert_eq!(expanded, "    a    b");
        assert_eq!(mapped, vec![0..9]);
        assert_eq!(&expanded[mapped[0].clone()], "    a    ");

        // Tab width 1 preserves offsets.
        let single_tab = ranges_for("a\tb", "b", SearchMode::Literal);
        let (expanded, mapped) = expand_tabs_with_highlights("a\tb", &single_tab, 1);
        assert_eq!(expanded, "a b");
        assert_eq!(mapped, vec![2..3]);
    }

    #[test]
    fn extract_match_results_handles_regex_groups_full_matches_literals_invert_and_limits() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("matches.log");
        std::fs::write(
            &path,
            concat!(
                "INFO boot\n",
                "WARN user=alice\tstatus=200 code=ok\n",
                "DEBUG middle\n",
                "ERROR user=bob\tstatus=503 status=504\n",
                "INFO שלום world\n",
            ),
        )
        .unwrap();
        let log = LogFile::open(&path, &AtomicBool::new(false)).unwrap();
        let cancel = AtomicBool::new(false);
        let bookmarks = BTreeSet::from([0]);

        // Regex mode with capturing groups: multiple groups on one line are space-separated,
        // even when `highlight_matches` is disabled. Context and bookmark rows are excluded.
        let mut opts = Config::load(None).unwrap().search;
        opts.mode = SearchMode::Regex;
        opts.highlight_matches = false;
        opts.context_lines = 1;
        let pattern = r"user=(\w+).*?status=(\d{3})";
        let res = search(&log, pattern, &opts, &bookmarks, &cancel).unwrap();
        assert_eq!(res.matches, vec![1, 3]);
        assert_eq!(res.visible, vec![0, 1, 2, 3, 4]);
        let re = compile(pattern, &opts).unwrap();
        assert_eq!(
            extract_match_results(&log, &res.matches, Some(&re), &opts, 64 * 1024).unwrap(),
            MatchCopyResult::Copied {
                text: "alice 200\nbob 503".to_owned(),
                count: 2,
            }
        );

        // Regex mode without capturing groups copies full matches, space-separated per line.
        let ungrouped = r"status=\d{3}";
        let res = search(&log, ungrouped, &opts, &bookmarks, &cancel).unwrap();
        let re = compile(ungrouped, &opts).unwrap();
        assert_eq!(
            extract_match_results(&log, &res.matches, Some(&re), &opts, 64 * 1024).unwrap(),
            MatchCopyResult::Copied {
                text: "status=200\nstatus=503 status=504".to_owned(),
                count: 2,
            }
        );

        // Regex mode skips matched lines whose optional capture group did not participate.
        let optional = r"INFO (שלום)?";
        let res = search(&log, optional, &opts, &bookmarks, &cancel).unwrap();
        assert_eq!(res.matches, vec![0, 4]);
        let re = compile(optional, &opts).unwrap();
        assert_eq!(
            extract_match_results(&log, &res.matches, Some(&re), &opts, 64 * 1024).unwrap(),
            MatchCopyResult::Copied {
                text: "שלום".to_owned(),
                count: 1,
            }
        );

        // Regex mode where no matched line has any non-empty highlightable text returns Empty.
        let zero_len = r"^";
        let res = search(&log, zero_len, &opts, &bookmarks, &cancel).unwrap();
        let re = compile(zero_len, &opts).unwrap();
        assert_eq!(
            extract_match_results(&log, &res.matches, Some(&re), &opts, 64 * 1024).unwrap(),
            MatchCopyResult::Empty
        );

        // Literal mode copies full matched lines verbatim (including raw tabs), ignoring bookmarks/context.
        opts.mode = SearchMode::Literal;
        opts.highlight_matches = true;
        let res = search(&log, "status=", &opts, &bookmarks, &cancel).unwrap();
        assert_eq!(res.matches, vec![1, 3]);
        assert_eq!(
            extract_match_results(&log, &res.matches, None, &opts, 64 * 1024).unwrap(),
            MatchCopyResult::Copied {
                text: "WARN user=alice\tstatus=200 code=ok\nERROR user=bob\tstatus=503 status=504"
                    .to_owned(),
                count: 2,
            }
        );

        // Inverted regex mode copies full matched (non-excluded) lines.
        opts.mode = SearchMode::Regex;
        opts.invert = true;
        let res = search(&log, r"status=\d{3}", &opts, &bookmarks, &cancel).unwrap();
        assert_eq!(res.matches, vec![0, 2, 4]);
        assert_eq!(
            extract_match_results(&log, &res.matches, None, &opts, 64 * 1024).unwrap(),
            MatchCopyResult::Copied {
                text: "INFO boot\nDEBUG middle\nINFO שלום world".to_owned(),
                count: 3,
            }
        );

        // Byte limit enforcement for both literal and regex modes, while allowing small
        // regex captures extracted from longer source lines.
        opts.invert = false;
        opts.mode = SearchMode::Literal;
        assert_eq!(
            extract_match_results(&log, &[1, 3], None, &opts, 20).unwrap(),
            MatchCopyResult::LimitExceeded
        );
        opts.mode = SearchMode::Regex;
        let re = compile(r"user=(\w+)", &opts).unwrap();
        assert_eq!(
            extract_match_results(&log, &[1, 3], Some(&re), &opts, 9).unwrap(),
            MatchCopyResult::Copied {
                text: "alice\nbob".to_owned(),
                count: 2,
            }
        );
        assert_eq!(
            extract_match_results(&log, &[1, 3], Some(&re), &opts, 8).unwrap(),
            MatchCopyResult::LimitExceeded
        );
        assert_eq!(
            extract_match_results(&log, &[], Some(&re), &opts, 64).unwrap(),
            MatchCopyResult::Empty
        );
    }

    #[test]
    fn selected_match_lines_intersects_main_and_filter_selections() {
        let matches = vec![1, 3, 6, 8];
        let visible = vec![0, 1, 2, 3, 4, 6, 8, 9];

        // Empty selection, empty matches, or empty log yields nothing.
        assert!(selected_match_lines(&matches, &LineSelection::empty(), None, 10).is_empty());
        assert!(selected_match_lines(&[], &LineSelection::single(1), None, 10).is_empty());
        assert!(selected_match_lines(&matches, &LineSelection::single(1), None, 0).is_empty());

        // Main pane: single matching vs non-matching line.
        assert_eq!(
            selected_match_lines(&matches, &LineSelection::single(1), None, 10),
            vec![1]
        );
        assert!(selected_match_lines(&matches, &LineSelection::single(2), None, 10).is_empty());

        // Main pane: multi-range non-contiguous selection and select-all.
        let mut main_sel = LineSelection::single(0);
        main_sel.extend_to(3); // lines 0..=3 -> matches 1, 3
        main_sel.toggle(8); // + line 8 -> match 8
        main_sel.toggle(9); // + line 9 -> non-match
        assert_eq!(
            selected_match_lines(&matches, &main_sel, None, 10),
            vec![1, 3, 8]
        );

        let mut all_main = LineSelection::empty();
        all_main.select_all(10_000_000);
        assert_eq!(
            selected_match_lines(&matches, &all_main, None, 10),
            vec![1, 3, 6, 8]
        );

        // Main pane clamps to log_len.
        assert_eq!(
            selected_match_lines(&matches, &all_main, None, 5),
            vec![1, 3]
        );

        // Filter pane: single context/bookmark row vs matching row.
        assert!(
            selected_match_lines(&matches, &LineSelection::single(0), Some(&visible), 10)
                .is_empty()
        );
        assert_eq!(
            selected_match_lines(&matches, &LineSelection::single(1), Some(&visible), 10),
            vec![1]
        );

        // Filter pane: range spanning context, matches, and bookmarks, plus non-contiguous toggle.
        let mut res_sel = LineSelection::single(0);
        res_sel.extend_to(4); // visible[0..=4] = [0, 1, 2, 3, 4] -> matches 1, 3
        res_sel.toggle(6); // visible[6] = 8 -> match 8
        res_sel.toggle(7); // visible[7] = 9 -> bookmark only
        assert_eq!(
            selected_match_lines(&matches, &res_sel, Some(&visible), 10),
            vec![1, 3, 8]
        );

        // Filter pane: select-all when visible equals matches and when context is present.
        let mut all_res = LineSelection::empty();
        all_res.select_all(visible.len());
        assert_eq!(
            selected_match_lines(&matches, &all_res, Some(&visible), 10),
            vec![1, 3, 6, 8]
        );
        let mut all_exact = LineSelection::empty();
        all_exact.select_all(matches.len());
        assert_eq!(
            selected_match_lines(&matches, &all_exact, Some(&matches), 10),
            vec![1, 3, 6, 8]
        );

        // Filter pane: out-of-range selection rows and empty visible are handled safely.
        assert!(
            selected_match_lines(&matches, &LineSelection::single(99), Some(&visible), 10)
                .is_empty()
        );
        assert!(
            selected_match_lines(&matches, &LineSelection::single(0), Some(&[]), 10).is_empty()
        );
    }

    #[test]
    fn append_preset_to_regex_preserves_exact_semantics_across_all_combinations() {
        let base_opts = Config::load(None).unwrap().search;
        let corpus = [
            "err",
            "ERR",
            "error",
            "ERROR",
            "warn",
            "WARN",
            "warning",
            "Foo",
            "foo",
            "FOO",
            "myFoo",
            "Foobar",
            "Bar",
            "bar",
            "BAR",
            "foobar",
            "foo.bar",
            "FOO.BAR",
            "fooXbar",
            "x foo.bar y",
            "afoo.bar",
            "foo.barz",
            "err|warn",
            "ERR|WARN",
            "x err|warn y",
            "a|",
            "A|",
            "xa|y",
            r"a\",
            r"A\",
            r"xa\y",
            "code=ERR",
            "code=err",
            "mycode=ERR",
            "code=ERR1",
            "code=404",
            "code=404x",
            "unrelated line 123",
        ];

        // `(input_in_search_box, semantic_existing_regex_when_non_empty)`
        let existing_cases: &[(&str, Option<&str>)] = &[
            ("", None),
            ("err", Some("err")),
            ("err|warn", Some("err|warn")),
            ("err|", Some("err")),
            (r"a\|", Some(r"a\|")),
            (r"a\\|", Some(r"a\\")),
        ];

        // `(mode, pattern, expected_capture_groups)`
        let preset_patterns: &[(SearchMode, &str, usize)] = &[
            (SearchMode::Literal, "Foo", 0),
            (SearchMode::Literal, "foo.bar", 0),
            (SearchMode::Literal, "err|warn", 0),
            (SearchMode::Regex, "Foo|Bar", 0),
            (SearchMode::Regex, r"code=(\d+|ERR)", 1),
        ];

        let toggles = [PresetToggle::Leave, PresetToggle::Off, PresetToggle::On];

        for &(existing, semantic_existing) in existing_cases {
            for active_case in [false, true] {
                for active_word in [false, true] {
                    for &(preset_mode, preset_pattern, preset_captures) in preset_patterns {
                        for match_case in toggles {
                            for whole_word in toggles {
                                for highlight_match in [false, true] {
                                    let mut active = base_opts.clone();
                                    active.mode = SearchMode::Regex;
                                    active.case_sensitive = active_case;
                                    active.whole_word = active_word;
                                    active.highlight_matches = !highlight_match;

                                    let preset = Preset {
                                        name: "Combo".into(),
                                        pattern: preset_pattern.into(),
                                        mode: preset_mode,
                                        highlight_match,
                                        match_case,
                                        whole_word,
                                        select_text: None,
                                    };

                                    let result = append_preset_to_regex(existing, &active, &preset);

                                    // Highlight adoption on empty, preservation on non-empty.
                                    if existing.is_empty() {
                                        assert_eq!(result.highlight_matches, highlight_match);
                                    } else {
                                        assert_eq!(
                                            result.highlight_matches,
                                            active.highlight_matches
                                        );
                                    }

                                    // Compile combined search with resulting tab options.
                                    let mut combined_opts = active.clone();
                                    combined_opts.case_sensitive = result.case_sensitive;
                                    combined_opts.whole_word = result.whole_word;
                                    combined_opts.highlight_matches = result.highlight_matches;
                                    let combined_re =
                                        compile(&result.pattern, &combined_opts).unwrap_or_else(
                                            |err| {
                                                panic!(
                                                    "failed to compile {:?} (existing={existing:?}, active_case={active_case}, active_word={active_word}, mode={preset_mode:?}, preset={preset_pattern:?}, match_case={match_case:?}, whole_word={whole_word:?}): {err}",
                                                    result.pattern
                                                )
                                            },
                                        );

                                    // Wrappers must never introduce spurious capturing groups.
                                    assert_eq!(
                                        combined_re.captures_len(),
                                        1 + preset_captures,
                                        "unexpected capture group count for {:?}",
                                        result.pattern
                                    );

                                    // Semantic oracle: existing branch + preset branch with its effective flags.
                                    let existing_re =
                                        semantic_existing.map(|pat| compile(pat, &active).unwrap());
                                    let mut preset_opts = active.clone();
                                    preset_opts.mode = preset_mode;
                                    match_case.apply_to(&mut preset_opts.case_sensitive);
                                    whole_word.apply_to(&mut preset_opts.whole_word);
                                    let preset_re = compile(preset_pattern, &preset_opts).unwrap();

                                    for line in corpus {
                                        let expected = existing_re
                                            .as_ref()
                                            .is_some_and(|re| re.is_match(line))
                                            || preset_re.is_match(line);
                                        assert_eq!(
                                            combined_re.is_match(line),
                                            expected,
                                            "mismatch on line {line:?} for combined={:?} (res_case={}, res_word={}) from existing={existing:?} (case={active_case}, word={active_word}) + preset=({preset_mode:?}, {preset_pattern:?}, match_case={match_case:?}, whole_word={whole_word:?})",
                                            result.pattern,
                                            result.case_sensitive,
                                            result.whole_word
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        // Degenerate single `|` with active whole_word=true and preset whole_word=Off lifts safely.
        let mut active = base_opts;
        active.mode = SearchMode::Regex;
        active.whole_word = true;
        let preset = Preset {
            name: "Warn".into(),
            pattern: "warn".into(),
            mode: SearchMode::Literal,
            highlight_match: true,
            match_case: PresetToggle::On,
            whole_word: PresetToggle::Off,
            select_text: None,
        };
        let result = append_preset_to_regex("|", &active, &preset);
        assert_eq!(result.pattern, r"\b(?:|)\b|(?-i:warn)");
        assert!(!result.whole_word);
        let mut opts = active;
        opts.whole_word = result.whole_word;
        assert!(compile(&result.pattern, &opts).is_ok());
    }

    #[test]
    fn append_preset_to_regex_handles_chained_appends_and_select_text_offsets() {
        let base_opts = Config::load(None).unwrap().search;

        // Multi-step chain starting from case-sensitive + whole-word active search:
        // 1. Lift whole-word + add case-insensitive non-whole-word literal preset
        // 2. Append case-sensitive whole-word regex preset (inherits active case_sensitive=true)
        // 3. Append case-insensitive whole-word literal preset with metacharacters
        let mut opts = base_opts.clone();
        opts.mode = SearchMode::Regex;
        opts.case_sensitive = true;
        opts.whole_word = true;

        let p1 = Preset {
            name: "Timeout".into(),
            pattern: "timeout".into(),
            mode: SearchMode::Literal,
            highlight_match: true,
            match_case: PresetToggle::Off,
            whole_word: PresetToggle::Off,
            select_text: None,
        };
        let p2 = Preset {
            name: "Verbs".into(),
            pattern: "GET|POST".into(),
            mode: SearchMode::Regex,
            highlight_match: true,
            match_case: PresetToggle::Leave,
            whole_word: PresetToggle::On,
            select_text: None,
        };
        let p3 = Preset {
            name: "Status".into(),
            pattern: "err=5xx".into(),
            mode: SearchMode::Literal,
            highlight_match: true,
            match_case: PresetToggle::Off,
            whole_word: PresetToggle::On,
            select_text: None,
        };

        let s1 = append_preset_to_regex("ERR", &opts, &p1);
        assert_eq!(s1.pattern, r"\b(?:ERR)\b|(?i:timeout)");
        assert!(s1.case_sensitive);
        assert!(!s1.whole_word);
        opts.case_sensitive = s1.case_sensitive;
        opts.whole_word = s1.whole_word;

        let s2 = append_preset_to_regex(&s1.pattern, &opts, &p2);
        assert_eq!(s2.pattern, r"\b(?:ERR)\b|(?i:timeout)|\b(?:GET|POST)\b");
        assert!(s2.case_sensitive);
        assert!(!s2.whole_word);

        let s3 = append_preset_to_regex(&s2.pattern, &opts, &p3);
        assert_eq!(
            s3.pattern,
            r"\b(?:ERR)\b|(?i:timeout)|\b(?:GET|POST)\b|\b(?i:err=5xx)\b"
        );
        let re = compile(&s3.pattern, &opts).unwrap();
        assert!(re.is_match("ERR failed"));
        assert!(!re.is_match("err failed"));
        assert!(!re.is_match("ERROR failed"));
        assert!(re.is_match("ReadTimeoutException"));
        assert!(re.is_match("GET /api"));
        assert!(!re.is_match("get /api"));
        assert!(!re.is_match("TARGET /api"));
        assert!(re.is_match("x ERR=5XX y"));
        assert!(!re.is_match("x myERR=5XX y"));

        // Verify `select_text` range slicing across every wrapper combination + whole-word lift + Unicode.
        let literal_sel = Preset {
            name: "UnicodeHost".into(),
            pattern: "[שלום] host=<HOST.NAME>".into(),
            mode: SearchMode::Literal,
            highlight_match: false,
            match_case: PresetToggle::On,
            whole_word: PresetToggle::On,
            select_text: Some("<HOST.NAME>".into()),
        };
        let regex_sel = Preset {
            name: "RegexPort".into(),
            pattern: "port=PORT_NUM".into(),
            mode: SearchMode::Regex,
            highlight_match: false,
            match_case: PresetToggle::Off,
            whole_word: PresetToggle::Off,
            select_text: Some("PORT_NUM".into()),
        };

        for existing in ["", "INFO", "INFO|"] {
            for active_case in [false, true] {
                for active_word in [false, true] {
                    let mut active = base_opts.clone();
                    active.mode = SearchMode::Regex;
                    active.case_sensitive = active_case;
                    active.whole_word = active_word;

                    for (preset, expected_sel) in
                        [(&literal_sel, r"<HOST\.NAME>"), (&regex_sel, "PORT_NUM")]
                    {
                        let res = append_preset_to_regex(existing, &active, preset);
                        let range = res.select_range.expect("expected mapped select range");
                        assert_eq!(
                            &res.pattern[range], expected_sel,
                            "wrong select slice for pattern {:?} (existing={existing:?}, case={active_case}, word={active_word})",
                            res.pattern
                        );
                    }
                }
            }
        }
    }
}
