//! Filesystem path resolution and filename completion for the editable file path bar.
use std::{
    fs,
    path::{MAIN_SEPARATOR, Path, PathBuf},
};

/// Maximum number of completion suggestions returned for display.
pub const MAX_COMPLETIONS: usize = 100;

/// Maximum number of directory entries scanned per completion query to keep UI responsive.
const MAX_DIR_SCAN: usize = 10_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathCompletion {
    /// Entry name displayed in the completion menu (directories end with a path separator).
    pub label: String,
    /// Complete text to insert into the path input when accepted.
    pub replacement: String,
    /// Resolved filesystem path for opening/validation.
    pub resolved: PathBuf,
    /// Whether this entry is a directory (following symlinks).
    pub is_dir: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CompletionResult {
    /// Directory label shown in the completion menu header (e.g. `/var/log/` or `~/`).
    pub directory_label: String,
    /// Matched entries, capped at `MAX_COMPLETIONS`.
    pub items: Vec<PathCompletion>,
    /// Total matching entries found before capping (within `MAX_DIR_SCAN`).
    pub total_matches: usize,
    /// Whether scanning hit `MAX_DIR_SCAN`.
    pub scan_truncated: bool,
    /// Longest common prefix text (including `display_prefix`) when it strictly extends
    /// the typed input across prefix matches.
    pub common_prefix: Option<String>,
}

/// Returns the user's home directory from standard environment variables.
pub fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .map(PathBuf::from)
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME").map(PathBuf::from)
    }
}

/// Expands a leading `~` or `~/` using `home`, preserving other paths unchanged.
fn expand_tilde(raw: &str, home: Option<&Path>) -> PathBuf {
    if raw == "~" {
        return home
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from(raw));
    }
    if let Some(rest) = raw
        .strip_prefix("~/")
        .or_else(|| raw.strip_prefix("~\\").filter(|_| cfg!(windows)))
        && let Some(home) = home
    {
        return home.join(rest);
    }
    PathBuf::from(raw)
}

/// Resolves user-entered path text against `cwd` and `home`.
///
/// If the raw input does not exist on disk and is wrapped in matching single or double quotes
/// (common when pasting paths from a terminal), falls back to the unquoted path.
pub fn resolve_input_path(raw: &str, cwd: &Path, home: Option<&Path>) -> Option<PathBuf> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }

    let resolve_candidate = |candidate: &str| -> PathBuf {
        let expanded = expand_tilde(candidate, home);
        if expanded.is_absolute() {
            expanded
        } else {
            cwd.join(expanded)
        }
    };

    // Preserve exact text if it already exists (e.g., filenames with leading/trailing spaces).
    if raw != trimmed {
        let exact = resolve_candidate(raw);
        if exact.exists() {
            return Some(exact);
        }
    }

    let direct = resolve_candidate(trimmed);
    if direct.exists() {
        return Some(direct);
    }

    if trimmed.len() >= 2
        && ((trimmed.starts_with('"') && trimmed.ends_with('"'))
            || (trimmed.starts_with('\'') && trimmed.ends_with('\'')))
    {
        let unquoted = &trimmed[1..trimmed.len() - 1];
        if !unquoted.trim().is_empty() {
            let candidate = resolve_candidate(unquoted);
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }

    Some(direct)
}

/// Ensures a directory path string ends with the platform separator.
pub fn ensure_trailing_separator(input: &str) -> String {
    if input.ends_with(std::path::is_separator) {
        input.to_owned()
    } else {
        format!("{input}{MAIN_SEPARATOR}")
    }
}

/// Computes filesystem completions for `input` using the process's current working directory
/// and user home directory.
pub fn complete_path(input: &str) -> CompletionResult {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let home = home_dir();
    complete_path_with(input, &cwd, home.as_deref())
}

/// Computes filesystem completions for `input` with explicit `cwd` and `home` directories.
pub fn complete_path_with(input: &str, cwd: &Path, home: Option<&Path>) -> CompletionResult {
    // Special case: bare `~` completes to `~/` when home exists.
    if input == "~" {
        if let Some(home) = home
            && home.is_dir()
        {
            let replacement = format!("~{MAIN_SEPARATOR}");
            return CompletionResult {
                directory_label: replacement.clone(),
                items: vec![PathCompletion {
                    label: replacement.clone(),
                    replacement: replacement.clone(),
                    resolved: home.to_path_buf(),
                    is_dir: true,
                }],
                total_matches: 1,
                scan_truncated: false,
                common_prefix: Some(replacement),
            };
        }
        return CompletionResult::default();
    }

    let (display_prefix, search_dir, name_prefix) = split_input(input, cwd, home);
    let Ok(read_dir) = fs::read_dir(&search_dir) else {
        return CompletionResult::default();
    };

    let show_hidden = name_prefix.starts_with('.');
    let name_prefix_lower = name_prefix.to_lowercase();

    let mut prefix_matches: Vec<(String, PathBuf, bool, bool)> = Vec::new();
    let mut substring_matches: Vec<(String, PathBuf, bool)> = Vec::new();
    let mut scan_truncated = false;

    for (scanned, entry) in read_dir.enumerate() {
        if scanned >= MAX_DIR_SCAN {
            scan_truncated = true;
            break;
        }

        let Ok(entry) = entry else {
            continue;
        };
        let os_name = entry.file_name();
        let Some(name) = os_name.to_str() else {
            continue;
        };
        if !show_hidden && name.starts_with('.') {
            continue;
        }

        let exact_prefix = name.starts_with(name_prefix);
        let ci_prefix = exact_prefix || name.to_lowercase().starts_with(&name_prefix_lower);

        if ci_prefix {
            let path = entry.path();
            let is_dir = entry_is_dir(&entry, &path);
            prefix_matches.push((name.to_owned(), path, is_dir, exact_prefix));
        } else if !name_prefix.is_empty() && name.to_lowercase().contains(&name_prefix_lower) {
            let path = entry.path();
            let is_dir = entry_is_dir(&entry, &path);
            substring_matches.push((name.to_owned(), path, is_dir));
        }
    }

    let used_prefix = !prefix_matches.is_empty() || name_prefix.is_empty();
    let mut matched: Vec<(String, PathBuf, bool)> = if used_prefix {
        prefix_matches
            .into_iter()
            .map(|(name, path, is_dir, _)| (name, path, is_dir))
            .collect()
    } else {
        substring_matches
    };

    let total_matches = matched.len();
    if matched.is_empty() {
        return CompletionResult {
            directory_label: display_directory_label(display_prefix, &search_dir),
            items: Vec::new(),
            total_matches: 0,
            scan_truncated,
            common_prefix: None,
        };
    }

    // Compute longest common prefix across all prefix matches before display truncation.
    let common_prefix = if used_prefix && matched.len() > 1 {
        let lcp = longest_common_prefix(matched.iter().map(|(name, _, _)| name.as_str()));
        if lcp.chars().count() > name_prefix.chars().count() {
            Some(format!("{display_prefix}{lcp}"))
        } else {
            None
        }
    } else {
        None
    };

    // Rank exact filename matches first, then directories before files, then alphabetically.
    matched.sort_by(|(a_name, _, a_dir), (b_name, _, b_dir)| {
        let a_exact = !name_prefix.is_empty() && a_name == name_prefix;
        let b_exact = !name_prefix.is_empty() && b_name == name_prefix;
        b_exact
            .cmp(&a_exact)
            .then_with(|| b_dir.cmp(a_dir))
            .then_with(|| a_name.to_lowercase().cmp(&b_name.to_lowercase()))
            .then_with(|| a_name.cmp(b_name))
    });

    let items = matched
        .into_iter()
        .take(MAX_COMPLETIONS)
        .map(|(name, resolved, is_dir)| {
            let label = if is_dir {
                format!("{name}{MAIN_SEPARATOR}")
            } else {
                name
            };
            let replacement = format!("{display_prefix}{label}");
            PathCompletion {
                label,
                replacement,
                resolved,
                is_dir,
            }
        })
        .collect();

    CompletionResult {
        directory_label: display_directory_label(display_prefix, &search_dir),
        items,
        total_matches,
        scan_truncated,
        common_prefix,
    }
}

fn split_input<'a>(input: &'a str, cwd: &Path, home: Option<&Path>) -> (&'a str, PathBuf, &'a str) {
    if let Some(sep_idx) = input.rfind(std::path::is_separator) {
        let sep_end = sep_idx + input[sep_idx..].chars().next().map_or(1, char::len_utf8);
        let display_prefix = &input[..sep_end];
        let name_prefix = &input[sep_end..];
        let expanded = expand_tilde(display_prefix, home);
        let search_dir = if expanded.is_absolute() {
            expanded
        } else {
            cwd.join(expanded)
        };
        (display_prefix, search_dir, name_prefix)
    } else {
        ("", cwd.to_path_buf(), input)
    }
}

fn display_directory_label(display_prefix: &str, search_dir: &Path) -> String {
    if !display_prefix.is_empty() {
        display_prefix.to_owned()
    } else {
        ensure_trailing_separator(&search_dir.display().to_string())
    }
}

fn entry_is_dir(entry: &fs::DirEntry, path: &Path) -> bool {
    match entry.file_type() {
        Ok(ft) if ft.is_symlink() => path.is_dir(),
        Ok(ft) => ft.is_dir(),
        Err(_) => path.is_dir(),
    }
}

fn longest_common_prefix<'a>(mut names: impl Iterator<Item = &'a str>) -> String {
    let Some(first) = names.next() else {
        return String::new();
    };
    let mut prefix: Vec<char> = first.chars().collect();
    for name in names {
        let mut matched_chars = 0usize;
        for (a, b) in prefix.iter().zip(name.chars()) {
            if *a == b {
                matched_chars += 1;
            } else {
                break;
            }
        }
        prefix.truncate(matched_chars);
        if prefix.is_empty() {
            break;
        }
    }
    prefix.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn completes_absolute_directory_and_prefix_with_dirs_first_and_lcp() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir(root.join("archive")).unwrap();
        fs::write(root.join("app-2026-01.log"), "a").unwrap();
        fs::write(root.join("app-2026-02.log"), "b").unwrap();
        fs::write(root.join("worker.log"), "w").unwrap();
        fs::write(root.join(".hidden.log"), "h").unwrap();

        let dir_prefix = ensure_trailing_separator(&root.display().to_string());

        // Listing directory hides dotfiles and places directories before files.
        let all = complete_path_with(&dir_prefix, root, None);
        let labels: Vec<&str> = all.items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(
            labels,
            vec![
                format!("archive{MAIN_SEPARATOR}"),
                "app-2026-01.log".to_owned(),
                "app-2026-02.log".to_owned(),
                "worker.log".to_owned(),
            ]
        );
        assert!(all.items[0].is_dir);
        assert!(!all.items[1].is_dir);
        assert_eq!(all.common_prefix, None);

        // Typing a prefix filters matches and computes longest common prefix.
        let app = complete_path_with(&format!("{dir_prefix}ap"), root, None);
        let labels: Vec<&str> = app.items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels, vec!["app-2026-01.log", "app-2026-02.log"]);
        assert_eq!(
            app.common_prefix.as_deref(),
            Some(format!("{dir_prefix}app-2026-0").as_str())
        );

        // Typing `.` reveals hidden files.
        let hidden = complete_path_with(&format!("{dir_prefix}."), root, None);
        assert_eq!(hidden.items.len(), 1);
        assert_eq!(hidden.items[0].label, ".hidden.log");

        // Substring fallback kicks in only when no prefix matches exist.
        let sub = complete_path_with(&format!("{dir_prefix}2026-02"), root, None);
        assert_eq!(sub.items.len(), 1);
        assert_eq!(sub.items[0].label, "app-2026-02.log");
        assert_eq!(sub.common_prefix, None);
    }

    #[test]
    fn completes_tilde_and_relative_paths_and_resolves_inputs() {
        let home_dir = tempdir().unwrap();
        let cwd_dir = tempdir().unwrap();
        fs::create_dir(home_dir.path().join("logs")).unwrap();
        fs::write(home_dir.path().join("logs/server.log"), "ok").unwrap();
        fs::write(cwd_dir.path().join("local.log"), "ok").unwrap();
        fs::write(cwd_dir.path().join("with space.log"), "ok").unwrap();

        // Bare `~` completes to `~/`.
        let tilde = complete_path_with("~", cwd_dir.path(), Some(home_dir.path()));
        assert_eq!(tilde.items.len(), 1);
        assert!(tilde.items[0].is_dir);
        assert_eq!(tilde.items[0].replacement, format!("~{MAIN_SEPARATOR}"));

        // `~/logs/s` preserves `~/logs/` prefix in replacement while resolving under home.
        let nested = complete_path_with("~/logs/s", cwd_dir.path(), Some(home_dir.path()));
        assert_eq!(nested.items.len(), 1);
        assert_eq!(nested.items[0].replacement, "~/logs/server.log");
        assert_eq!(
            nested.items[0].resolved,
            home_dir.path().join("logs/server.log")
        );

        // Relative completion preserves relative display prefix.
        let rel = complete_path_with("loc", cwd_dir.path(), Some(home_dir.path()));
        assert_eq!(rel.items.len(), 1);
        assert_eq!(rel.items[0].replacement, "local.log");
        assert_eq!(rel.items[0].resolved, cwd_dir.path().join("local.log"));

        // Input resolution handles tilde, relative, and quoted paths.
        assert_eq!(
            resolve_input_path("~/logs/server.log", cwd_dir.path(), Some(home_dir.path())),
            Some(home_dir.path().join("logs/server.log"))
        );
        assert_eq!(
            resolve_input_path("local.log", cwd_dir.path(), Some(home_dir.path())),
            Some(cwd_dir.path().join("local.log"))
        );
        assert_eq!(
            resolve_input_path("\"with space.log\"", cwd_dir.path(), Some(home_dir.path())),
            Some(cwd_dir.path().join("with space.log"))
        );
        assert_eq!(
            resolve_input_path("   ", cwd_dir.path(), Some(home_dir.path())),
            None
        );
    }

    #[test]
    fn longest_common_prefix_handles_unicode_safely() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("ログ-α.log"), "1").unwrap();
        fs::write(root.join("ログ-β.log"), "2").unwrap();
        let prefix = ensure_trailing_separator(&root.display().to_string());
        let res = complete_path_with(&format!("{prefix}ロ"), root, None);
        assert_eq!(res.items.len(), 2);
        assert_eq!(
            res.common_prefix.as_deref(),
            Some(format!("{prefix}ログ-").as_str())
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_directories_are_recognized_as_directories() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("real_dir");
        fs::create_dir(&target).unwrap();
        let link = dir.path().join("link_dir");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let prefix = ensure_trailing_separator(&dir.path().display().to_string());
        let res = complete_path_with(&format!("{prefix}link"), dir.path(), None);
        assert_eq!(res.items.len(), 1);
        assert!(res.items[0].is_dir);
        assert_eq!(res.items[0].label, format!("link_dir{MAIN_SEPARATOR}"));
    }
}
