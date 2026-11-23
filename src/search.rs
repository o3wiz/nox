use crate::{
    config::{SearchConfig, SearchMode},
    logfile::LogFile,
};
use anyhow::{Result, ensure};
use regex::{Regex, RegexBuilder};
use std::{
    collections::BTreeSet,
    io::BufRead,
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
