use crate::{
    config::{SearchConfig, SearchMode},
    logfile::LogFile,
};
use anyhow::{Result, ensure};
use regex::{Regex, RegexBuilder};
use std::{
    collections::BTreeSet,
    io::BufRead,
    ops::Range,
    sync::atomic::{AtomicBool, Ordering},
};

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
}
