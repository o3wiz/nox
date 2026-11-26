//! Fuzzy matching and ranking for saved search pattern names.
use crate::config::Preset;
use std::ops::Range;

const SCORE_MATCH: i32 = 1;
const BONUS_CONSECUTIVE: i32 = 16;
const BONUS_START: i32 = 20;
const BONUS_BOUNDARY: i32 = 15;
const BONUS_CAMEL: i32 = 12;
const BONUS_EXACT_CASE: i32 = 1;
const PENALTY_GAP_START: i32 = -3;
const PENALTY_GAP_EXTEND: i32 = -1;
const MAX_LEADING_PENALTY: i32 = -10;
const NEG_INF: i32 = i32::MIN / 4;
const MAX_DP_CELLS: usize = 100_000;

/// Result of fuzzy-matching a query against a single candidate string.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FuzzyMatch {
    /// Higher score indicates a stronger match.
    pub score: i32,
    /// Merged, sorted UTF-8 byte ranges in `candidate` corresponding to matched characters.
    pub ranges: Vec<Range<usize>>,
}

/// Ranked fuzzy match referencing a preset in `SearchConfig::presets`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PresetMatch {
    /// Original index into `SearchConfig::presets`.
    pub index: usize,
    /// Fuzzy score (higher is better; `0` when query is empty).
    pub score: i32,
    /// Merged, sorted UTF-8 byte ranges in `preset.name` for matched character highlights.
    pub ranges: Vec<Range<usize>>,
}

struct CandChar {
    byte_start: usize,
    byte_end: usize,
    orig: char,
    lower: char,
    boundary_bonus: i32,
}

fn fold_char(ch: char) -> char {
    ch.to_lowercase().next().unwrap_or(ch)
}

fn is_separator(ch: char) -> bool {
    ch.is_whitespace() || matches!(ch, '-' | '_' | '/' | '\\' | '.' | ':' | '(' | '[')
}

fn build_candidate_chars(candidate: &str) -> Vec<CandChar> {
    let mut chars = Vec::with_capacity(candidate.chars().count());
    let mut prev: Option<char> = None;
    for (byte_start, orig) in candidate.char_indices() {
        let boundary_bonus = match prev {
            None => BONUS_START,
            Some(p) if is_separator(p) => BONUS_BOUNDARY,
            Some(p) if p.is_lowercase() && orig.is_uppercase() => BONUS_CAMEL,
            Some(_) => 0,
        };
        chars.push(CandChar {
            byte_start,
            byte_end: byte_start + orig.len_utf8(),
            orig,
            lower: fold_char(orig),
            boundary_bonus,
        });
        prev = Some(orig);
    }
    chars
}

fn merge_ranges(cand: &[CandChar], matched_indices: &[usize]) -> Vec<Range<usize>> {
    let mut ranges: Vec<Range<usize>> = Vec::with_capacity(matched_indices.len());
    for &idx in matched_indices {
        let range = cand[idx].byte_start..cand[idx].byte_end;
        if let Some(last) = ranges.last_mut()
            && last.end == range.start
        {
            last.end = range.end;
        } else {
            ranges.push(range);
        }
    }
    ranges
}

fn greedy_fallback(query: &[(char, char)], cand: &[CandChar]) -> Option<FuzzyMatch> {
    let mut matched = Vec::with_capacity(query.len());
    let mut score = 0i32;
    let mut prev_idx: Option<usize> = None;
    let mut start = 0usize;

    for &(q_orig, q_lower) in query {
        let found = (start..cand.len()).find(|&j| cand[j].lower == q_lower)?;
        let exact = i32::from(cand[found].orig == q_orig) * BONUS_EXACT_CASE;
        match prev_idx {
            None => {
                let leading = (-(found as i32)).max(MAX_LEADING_PENALTY);
                score += SCORE_MATCH + cand[found].boundary_bonus + exact + leading;
            }
            Some(prev) if found == prev + 1 => {
                score += SCORE_MATCH + BONUS_CONSECUTIVE + cand[found].boundary_bonus + exact;
            }
            Some(prev) => {
                let gap = (found - prev - 1) as i32;
                let penalty = PENALTY_GAP_START + PENALTY_GAP_EXTEND * (gap - 1);
                score += SCORE_MATCH + cand[found].boundary_bonus + exact + penalty;
            }
        }
        matched.push(found);
        prev_idx = Some(found);
        start = found + 1;
    }

    Some(FuzzyMatch {
        score,
        ranges: merge_ranges(cand, &matched),
    })
}

/// Fuzzy-matches `query` against `candidate` (case-insensitive, Unicode-safe).
///
/// Whitespace in `query` is ignored so empty/whitespace queries match with score `0`
/// and empty highlight ranges, and multi-word queries match across spaces, punctuation,
/// or CamelCase word boundaries in left-to-right order.
pub fn fuzzy_match(query: &str, candidate: &str) -> Option<FuzzyMatch> {
    let query_chars: Vec<(char, char)> = query
        .chars()
        .filter(|ch| !ch.is_whitespace())
        .map(|ch| (ch, fold_char(ch)))
        .collect();
    if query_chars.is_empty() {
        return Some(FuzzyMatch {
            score: 0,
            ranges: Vec::new(),
        });
    }

    let cand = build_candidate_chars(candidate);
    let m = query_chars.len();
    let n = cand.len();
    if m > n {
        return None;
    }

    // Fast O(n) subsequence check before allocating DP tables.
    let mut qi = 0usize;
    for ch in &cand {
        if ch.lower == query_chars[qi].1 {
            qi += 1;
            if qi == m {
                break;
            }
        }
    }
    if qi < m {
        return None;
    }

    // Guard against pathological inputs with huge strings.
    if m.saturating_mul(n) > MAX_DP_CELLS {
        return greedy_fallback(&query_chars, &cand);
    }

    let mut dp = vec![NEG_INF; m * n];
    let mut prev_col = vec![0usize; m * n];

    // Row 0: match first query character at every matching candidate index.
    let (q0_orig, q0_lower) = query_chars[0];
    for (j, ch) in cand.iter().enumerate() {
        if ch.lower == q0_lower {
            let leading = (-(j as i32)).max(MAX_LEADING_PENALTY);
            let exact = i32::from(ch.orig == q0_orig) * BONUS_EXACT_CASE;
            dp[j] = SCORE_MATCH + ch.boundary_bonus + exact + leading;
        }
    }

    // Remaining rows: optimal subsequence alignment with consecutive & boundary bonuses.
    for (i, &(q_orig, q_lower)) in query_chars.iter().enumerate().skip(1) {
        let prev_row = (i - 1) * n;
        let row = i * n;
        let mut best_gap_score = NEG_INF;
        let mut best_gap_col = 0usize;

        for j in i..n {
            if j >= 2 {
                let extended = if best_gap_score > NEG_INF {
                    best_gap_score + PENALTY_GAP_EXTEND
                } else {
                    NEG_INF
                };
                let started = if dp[prev_row + j - 2] > NEG_INF {
                    dp[prev_row + j - 2] + PENALTY_GAP_START
                } else {
                    NEG_INF
                };
                if started >= extended {
                    best_gap_score = started;
                    best_gap_col = j - 2;
                } else {
                    best_gap_score = extended;
                }
            }

            let ch = &cand[j];
            if ch.lower != q_lower {
                continue;
            }

            let consec_score = if dp[prev_row + j - 1] > NEG_INF {
                dp[prev_row + j - 1] + BONUS_CONSECUTIVE + ch.boundary_bonus
            } else {
                NEG_INF
            };
            let gap_score = if best_gap_score > NEG_INF {
                best_gap_score + ch.boundary_bonus
            } else {
                NEG_INF
            };

            let (best_prev, prev_j) = if consec_score >= gap_score {
                (consec_score, j - 1)
            } else {
                (gap_score, best_gap_col)
            };

            if best_prev > NEG_INF {
                let exact = i32::from(ch.orig == q_orig) * BONUS_EXACT_CASE;
                dp[row + j] = best_prev + SCORE_MATCH + exact;
                prev_col[row + j] = prev_j;
            }
        }
    }

    let last_row = (m - 1) * n;
    let mut best_score = NEG_INF;
    let mut best_end = None;
    for j in (m - 1)..n {
        if dp[last_row + j] > best_score {
            best_score = dp[last_row + j];
            best_end = Some(j);
        }
    }
    let mut cur = best_end?;

    let mut matched_indices = vec![0usize; m];
    for i in (0..m).rev() {
        matched_indices[i] = cur;
        if i > 0 {
            cur = prev_col[i * n + cur];
        }
    }

    Some(FuzzyMatch {
        score: best_score,
        ranges: merge_ranges(&cand, &matched_indices),
    })
}

/// Filters and ranks `presets` by fuzzy-matching `query` strictly against each preset's `name`.
///
/// An empty or whitespace-only query returns all presets in their original configured order.
/// Non-empty queries return only matching presets sorted by best score first, then shorter name,
/// then original configuration order.
pub fn filter_presets(presets: &[Preset], query: &str) -> Vec<PresetMatch> {
    if query.trim().is_empty() {
        return (0..presets.len())
            .map(|index| PresetMatch {
                index,
                score: 0,
                ranges: Vec::new(),
            })
            .collect();
    }

    let mut matches: Vec<(PresetMatch, usize)> = presets
        .iter()
        .enumerate()
        .filter_map(|(index, preset)| {
            fuzzy_match(query, &preset.name).map(|m| {
                (
                    PresetMatch {
                        index,
                        score: m.score,
                        ranges: m.ranges,
                    },
                    preset.name.chars().count(),
                )
            })
        })
        .collect();

    matches.sort_by(|(a, a_len), (b, b_len)| {
        b.score
            .cmp(&a.score)
            .then_with(|| a_len.cmp(b_len))
            .then_with(|| a.index.cmp(&b.index))
    });

    matches.into_iter().map(|(m, _)| m).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SearchMode;

    fn preset(name: &str, pattern: &str) -> Preset {
        Preset {
            name: name.to_owned(),
            pattern: pattern.to_owned(),
            mode: SearchMode::Regex,
            highlight_match: true,
            select_text: None,
        }
    }

    #[test]
    fn empty_and_whitespace_queries_preserve_all_presets_in_order() {
        let presets = vec![
            preset("Errors", "error|fatal"),
            preset("Warnings", "warn"),
            preset("Requests", "GET|POST"),
        ];

        for query in ["", "   ", "\t\n"] {
            let results = filter_presets(&presets, query);
            assert_eq!(
                results.iter().map(|m| m.index).collect::<Vec<_>>(),
                vec![0, 1, 2]
            );
            assert!(results.iter().all(|m| m.ranges.is_empty()));
        }
    }

    #[test]
    fn ranks_prefix_and_shorter_matches_above_word_boundary_and_scattered_matches() {
        let presets = vec![
            preset("Server Retry", "retry"),
            preset("HTTP Errors", "5\\d\\d"),
            preset("Errors Extended", "err"),
            preset("Errors", "error|fatal"),
        ];

        let results = filter_presets(&presets, "err");
        let names: Vec<&str> = results
            .iter()
            .map(|m| presets[m.index].name.as_str())
            .collect();
        assert_eq!(
            names,
            vec!["Errors", "Errors Extended", "HTTP Errors", "Server Retry"]
        );
    }

    #[test]
    fn dp_alignment_prefers_word_boundary_run_over_greedy_earlier_scatter() {
        let matched = fuzzy_match("err", "Server Error").expect("should match");
        let highlighted: Vec<&str> = matched
            .ranges
            .iter()
            .map(|r| &"Server Error"[r.clone()])
            .collect();
        assert_eq!(highlighted, vec!["Err"]);
    }

    #[test]
    fn supports_camel_case_initials_and_multi_word_queries() {
        let presets = vec![
            preset("DB Timeout", "timeout"),
            preset("DB_Timeout", "timeout"),
            preset("DbTimeout", "timeout"),
            preset("Debug Output", "debug"),
        ];

        let results = filter_presets(&presets, "db time");
        let names: Vec<&str> = results
            .iter()
            .map(|m| presets[m.index].name.as_str())
            .collect();
        assert_eq!(names, vec!["DbTimeout", "DB Timeout", "DB_Timeout"]);
        let spans_for = |m: &PresetMatch| -> Vec<&str> {
            let name = &presets[m.index].name;
            m.ranges.iter().map(|r| &name[r.clone()]).collect()
        };
        assert_eq!(spans_for(&results[0]), vec!["DbTime"]);
        assert_eq!(spans_for(&results[1]), vec!["DB", "Time"]);
        assert_eq!(spans_for(&results[2]), vec!["DB", "Time"]);
    }

    #[test]
    fn filters_strictly_by_pattern_name_not_pattern_body() {
        let presets = vec![
            preset("Errors", "error|fatal|panic"),
            preset("Warnings", "warn"),
            preset("Requests", "GET|POST|PUT|DELETE"),
        ];

        assert!(filter_presets(&presets, "panic").is_empty());
        assert!(filter_presets(&presets, "DELETE").is_empty());

        let wrn = filter_presets(&presets, "wrn");
        assert_eq!(wrn.len(), 1);
        assert_eq!(wrn[0].index, 1);
        let spans: Vec<&str> = wrn[0]
            .ranges
            .iter()
            .map(|r| &presets[1].name[r.clone()])
            .collect();
        assert_eq!(spans, vec!["W", "rn"]);
    }

    #[test]
    fn handles_unicode_names_and_valid_utf8_highlight_ranges() {
        let presets = vec![
            preset("Café Errors", "err"),
            preset("日本語ログ検索", "log"),
            preset("שגיאות שרת", "err"),
        ];

        let cafe = filter_presets(&presets, "café");
        assert_eq!(cafe.len(), 1);
        assert_eq!(cafe[0].index, 0);
        assert_eq!(
            cafe[0]
                .ranges
                .iter()
                .map(|r| &presets[0].name[r.clone()])
                .collect::<Vec<_>>(),
            vec!["Café"]
        );

        let jp = filter_presets(&presets, "日検");
        assert_eq!(jp.len(), 1);
        assert_eq!(jp[0].index, 1);
        assert_eq!(
            jp[0]
                .ranges
                .iter()
                .map(|r| &presets[1].name[r.clone()])
                .collect::<Vec<_>>(),
            vec!["日", "検"]
        );

        let heb = filter_presets(&presets, "שרת");
        assert_eq!(heb.len(), 1);
        assert_eq!(heb[0].index, 2);
        assert_eq!(
            heb[0]
                .ranges
                .iter()
                .map(|r| &presets[2].name[r.clone()])
                .collect::<Vec<_>>(),
            vec!["שרת"]
        );
    }

    #[test]
    fn breaks_exact_score_and_length_ties_by_original_config_order() {
        let presets = vec![
            preset("Alpha One", "a"),
            preset("Alpha Two", "b"),
            preset("Alpha Six", "c"),
        ];
        let results = filter_presets(&presets, "alpha");
        assert_eq!(
            results.iter().map(|m| m.index).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
    }
}
