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
pub fn search(
    log: &LogFile,
    pattern: &str,
    options: &SearchConfig,
    bookmarks: &BTreeSet<usize>,
    cancel: &AtomicBool,
) -> Result<SearchResults> {
    if pattern.is_empty() {
        let visible: Vec<_> = bookmarks
            .iter()
            .copied()
            .filter(|&i| i < log.len())
            .collect();
        let widest_row = widest(log, &visible);
        return Ok(SearchResults {
            visible,
            widest_row,
            ..Default::default()
        });
    }
    let regex = compile(pattern, options)?;
    let mut result = SearchResults::default();
    let mut visible = BTreeSet::new();
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
        if !pattern.is_empty() && (regex.is_match(&text) != options.invert) {
            if result.matches.len() == options.max_results {
                result.limited = true;
                break;
            }
            result.matches.push(index);
            visible.extend(
                index.saturating_sub(options.context_lines)
                    ..=(index + options.context_lines).min(log.len().saturating_sub(1)),
            );
        }
        index += 1;
    }
    visible.extend(bookmarks.iter().copied().filter(|&index| index < log.len()));
    result.visible = visible.into_iter().collect();
    result.widest_row = widest(log, &result.visible);
    Ok(result)
}

fn widest(log: &LogFile, rows: &[usize]) -> usize {
    rows.iter()
        .enumerate()
        .max_by_key(|(_, line)| log.line_bytes(**line))
        .map_or(0, |(index, _)| index)
}
