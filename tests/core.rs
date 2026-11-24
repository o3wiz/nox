use nox::{
    config::{self, Config, SearchMode},
    logfile::LogFile,
    search,
};
use std::{collections::BTreeSet, fs, sync::atomic::AtomicBool};
use tempfile::tempdir;

fn indexed(bytes: &[u8]) -> (tempfile::TempDir, LogFile) {
    let dir = tempdir().unwrap();
    let path = dir.path().join("test.log");
    fs::write(&path, bytes).unwrap();
    let log = LogFile::open(&path, &AtomicBool::new(false)).unwrap();
    (dir, log)
}
#[test]
fn indexes_empty_crlf_and_unterminated_lines() {
    for (bytes, expected) in [
        (b"".as_slice(), vec![]),
        (b"\n", vec![""]),
        (b"alpha\r\n\r\nomega", vec!["alpha", "", "omega"]),
        (b"a\nb\n", vec!["a", "b"]),
    ] {
        let (_dir, log) = indexed(bytes);
        assert_eq!(log.len(), expected.len());
        for (index, text) in expected.iter().enumerate() {
            assert_eq!(&log.line(index, 100).unwrap(), text);
        }
    }
}
#[test]
fn reads_unicode_invalid_utf8_and_long_lines_without_panicking() {
    let (_dir, log) = indexed("שלום 日本語\n".as_bytes());
    assert_eq!(log.line(0, 100).unwrap(), "שלום 日本語");
    let (_dir, log) = indexed(b"a\xffb\n");
    assert_eq!(log.line(0, 100).unwrap(), "a�b");
    let (_dir, log) = indexed(&vec![b'x'; 600_000]);
    assert_eq!(log.len(), 1);
    assert!(log.line(0, 80).unwrap().contains("truncated"));
}
#[test]
fn indexes_new_file_state_after_append_truncate_and_rotation() {
    let (dir, old) = indexed(b"one\npar");
    let path = dir.path().join("test.log");
    fs::write(&path, b"one\npartial\nthree\n").unwrap();
    let appended = LogFile::open(&path, &AtomicBool::new(false)).unwrap();
    assert_eq!(appended.len(), 3);
    assert_eq!(appended.line(1, 100).unwrap(), "partial");
    fs::rename(&path, dir.path().join("rotated.log")).unwrap();
    fs::write(&path, b"new\n").unwrap();
    let rotated = LogFile::open(&path, &AtomicBool::new(false)).unwrap();
    assert_eq!(rotated.len(), 1);
    assert_eq!(appended.line(2, 100).unwrap(), "three");
    assert_ne!(old.stamp, rotated.stamp);
    fs::write(&path, b"").unwrap();
    assert!(
        LogFile::open(&path, &AtomicBool::new(false))
            .unwrap()
            .is_empty()
    );
}
#[test]
fn cancelled_index_and_search_stop() {
    let (_dir, log) = indexed(b"x\ny\n");
    assert!(LogFile::open(&log.path, &AtomicBool::new(true)).is_err());
    assert!(
        search::search(
            &log,
            "x",
            &Config::load(None).unwrap().search,
            &BTreeSet::new(),
            &AtomicBool::new(true)
        )
        .is_err()
    );
}
#[test]
fn searches_literal_regex_case_words_and_inversion() {
    let (_dir, log) = indexed(b"ERROR one\nerror two\nterror three\na.b\naxb\n");
    let mut options = Config::load(None).unwrap().search;
    let run = |pattern: &str, options: &_| {
        search::search(
            &log,
            pattern,
            options,
            &BTreeSet::new(),
            &AtomicBool::new(false),
        )
        .unwrap()
        .matches
    };
    assert_eq!(run("error", &options), vec![0, 1, 2]);
    options.case_sensitive = true;
    assert_eq!(run("error", &options), vec![1, 2]);
    options.whole_word = true;
    assert_eq!(run("error", &options), vec![1]);
    options.whole_word = false;
    options.mode = SearchMode::Literal;
    assert_eq!(run("a.b", &options), vec![3]);
    options.mode = SearchMode::Regex;
    assert_eq!(run("a.b", &options), vec![3, 4]);
    options.invert = true;
    assert_eq!(run("a.b", &options), vec![0, 1, 2]);
    assert!(search::compile("[", &options).is_err());
}
#[test]
fn results_merge_context_and_bookmarks_without_duplicates_and_report_limits() {
    let (_dir, log) = indexed(b"before\nhit\nhit\nafter\nlast\n");
    let mut options = Config::load(None).unwrap().search;
    options.context_lines = 1;
    let marks = BTreeSet::from([1, 4, 99]);
    let result = search::search(&log, "hit", &options, &marks, &AtomicBool::new(false)).unwrap();
    assert_eq!(result.matches, vec![1, 2]);
    assert_eq!(result.visible, vec![0, 1, 2, 3, 4]);
    assert!(!result.limited);
    options.max_results = 1;
    let result = search::search(
        &log,
        "hit",
        &options,
        &BTreeSet::new(),
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(result.matches, vec![1]);
    assert!(result.limited);
    let empty = search::search(&log, "", &options, &marks, &AtomicBool::new(false)).unwrap();
    assert!(empty.matches.is_empty());
    assert_eq!(empty.visible, vec![1, 4]);
}
#[test]
fn export_preserves_full_lines_and_never_overwrites() {
    let (dir, log) = indexed(b"first\r\nsecond\nlast");
    let out = dir.path().join("out.log");
    log.export(&[0, 2], &out).unwrap();
    assert_eq!(fs::read(&out).unwrap(), b"first\r\nlast\n");
    assert!(log.export(&[1], &out).is_err());
    assert!(log.export(&[1], &log.path).is_err());
}
#[test]
fn overlays_defaults_but_rejects_typos_wrong_types_and_invalid_values() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("window.json");
    fs::write(&path, r#"{"width":1600}"#).unwrap();
    let config = Config::load(Some(dir.path())).unwrap();
    assert_eq!(config.window.width, 1600.);
    assert_eq!(config.window.height, 850.);
    for invalid in [
        r#"{"widht":1600}"#,
        r#"{"width":"1600"}"#,
        r#"{"width":0}"#,
        r#"{"$schema":42}"#,
        "[]",
        "null",
        "{",
    ] {
        fs::write(&path, invalid).unwrap();
        assert!(
            Config::load(Some(dir.path())).is_err(),
            "accepted {invalid}"
        );
    }
}
#[test]
fn validates_highlights_colors_presets_and_duplicate_shortcuts() {
    let mut config = Config::load(None).unwrap();
    config.appearance.accent = "red".into();
    assert!(config.validate().is_err());
    let mut config = Config::load(None).unwrap();
    config.appearance.match_foreground = "red".into();
    assert!(config.validate().is_err());
    let mut config = Config::load(None).unwrap();
    config.appearance.match_background = "#12345".into();
    assert!(config.validate().is_err());
    let mut config = Config::load(None).unwrap();
    config.highlighting.rules[0].pattern = "[".into();
    assert!(config.validate().is_err());
    let mut config = Config::load(None).unwrap();
    config.search.presets[0].pattern = "[".into();
    assert!(config.validate().is_err());
    let mut config = Config::load(None).unwrap();
    config
        .keyboard
        .bindings
        .push(config.keyboard.bindings[0].clone());
    assert!(config.validate().is_err());
}

#[test]
fn presets_parse_highlight_match_default_true_and_reject_non_booleans() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("search.json");
    fs::write(
        &path,
        r#"{
            "presets": [
                { "name": "Highlighted", "pattern": "err", "mode": "regex", "highlight_match": true },
                { "name": "Plain", "pattern": "warn", "mode": "literal", "highlight_match": false },
                { "name": "Defaulted", "pattern": "info", "mode": "literal" }
            ]
        }"#,
    )
    .unwrap();
    let config = Config::load(Some(dir.path())).unwrap();
    assert_eq!(config.search.presets.len(), 3);
    assert!(config.search.presets[0].highlight_match);
    assert!(!config.search.presets[1].highlight_match);
    assert!(config.search.presets[2].highlight_match);

    for invalid in [
        r#"{"presets":[{"name":"Bad","pattern":"err","mode":"regex","highlight_match":"on"}]}"#,
        r#"{"presets":[{"name":"Bad","pattern":"err","mode":"regex","highlight_match":1}]}"#,
        r#"{"presets":[{"name":"Bad","pattern":"err","mode":"regex","highlight_match":null}]}"#,
    ] {
        fs::write(&path, invalid).unwrap();
        assert!(
            Config::load(Some(dir.path())).is_err(),
            "accepted invalid preset highlight_match {invalid}"
        );
    }
}

#[test]
fn appearance_overlays_configurable_match_foreground_and_background_colors() {
    let defaults = Config::load(None).unwrap();
    assert_eq!(defaults.appearance.match_foreground, "#0B1016");
    assert_eq!(defaults.appearance.match_background, "#E0AF4B");

    let dir = tempdir().unwrap();
    let path = dir.path().join("appearance.json");
    fs::write(
        &path,
        r##"{"match_foreground":"#FFFFFF","match_background":"#2563EB"}"##,
    )
    .unwrap();
    let custom = Config::load(Some(dir.path())).unwrap();
    assert_eq!(custom.appearance.match_foreground, "#FFFFFF");
    assert_eq!(custom.appearance.match_background, "#2563EB");
    assert_eq!(custom.appearance.background, defaults.appearance.background);

    // Partial override preserves the unspecified match color default.
    fs::write(&path, r##"{"match_background":"#FF00AA"}"##).unwrap();
    let partial = Config::load(Some(dir.path())).unwrap();
    assert_eq!(
        partial.appearance.match_foreground,
        defaults.appearance.match_foreground
    );
    assert_eq!(partial.appearance.match_background, "#FF00AA");

    for invalid in [
        r##"{"match_foreground":"white"}"##,
        r##"{"match_background":"#FFF"}"##,
        r##"{"match_foreground":123456}"##,
        r##"{"match_background":null}"##,
    ] {
        fs::write(&path, invalid).unwrap();
        assert!(
            Config::load(Some(dir.path())).is_err(),
            "accepted invalid match color {invalid}"
        );
    }
}
#[test]
fn generated_config_round_trips_and_preserves_existing_files() {
    let dir = tempdir().unwrap();
    config::write_defaults(dir.path()).unwrap();
    Config::load(Some(dir.path())).unwrap();
    fs::write(dir.path().join("window.json"), r#"{"width":1800}"#).unwrap();
    assert!(config::write_defaults(dir.path()).is_err());
    assert_eq!(Config::load(Some(dir.path())).unwrap().window.width, 1800.);
    for (name, _) in config::DEFAULTS {
        let schema: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(
                dir.path()
                    .join("schemas")
                    .join(format!("{name}.schema.json")),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(schema["additionalProperties"], false);
        assert!(schema["properties"]["$schema"].is_object());
    }
}

#[test]
fn session_saves_atomically_restores_and_rejects_unknown_versions() {
    use nox::session::{SavedTab, Session};
    let dir = tempdir().unwrap();
    let path = dir.path().join("state/session.json");
    let config = Config::load(None).unwrap();
    let session = Session {
        version: 1,
        active: 0,
        recent: vec!["app.log".into()],
        results_height: Some(340.0),
        tabs: vec![SavedTab {
            path: "app.log".into(),
            query: "ERROR".into(),
            options: config.search.clone(),
            bookmarks: BTreeSet::from([1, 3]),
            selected: 3,
            follow: true,
        }],
    };
    session.save(&path).unwrap();
    session.save(&path).unwrap();
    let restored = Session::load(&path, &config).unwrap();
    assert_eq!(restored.tabs[0].query, "ERROR");
    assert_eq!(restored.tabs[0].bookmarks, BTreeSet::from([1, 3]));
    assert_eq!(restored.results_height, Some(340.0));
    let mut invalid = serde_json::to_value(session).unwrap();
    invalid["version"] = 99.into();
    fs::write(&path, serde_json::to_vec(&invalid).unwrap()).unwrap();
    assert!(Session::load(&path, &config).is_err());
}

#[test]
fn legacy_session_without_highlight_matches_or_results_height_loads_and_rejects_invalid_heights() {
    use nox::session::{SavedTab, Session};
    let dir = tempdir().unwrap();
    let path = dir.path().join("state/legacy-session.json");
    let config = Config::load(None).unwrap();
    let session = Session {
        version: 1,
        active: 0,
        recent: Vec::new(),
        results_height: None,
        tabs: vec![SavedTab {
            path: "app.log".into(),
            query: "ERROR".into(),
            options: config.search.clone(),
            bookmarks: BTreeSet::new(),
            selected: 0,
            follow: false,
        }],
    };
    let mut legacy = serde_json::to_value(&session).unwrap();
    legacy["tabs"][0]["options"]
        .as_object_mut()
        .unwrap()
        .remove("highlight_matches");
    for preset in legacy["tabs"][0]["options"]["presets"]
        .as_array_mut()
        .unwrap()
    {
        preset.as_object_mut().unwrap().remove("highlight_match");
    }
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, serde_json::to_vec_pretty(&legacy).unwrap()).unwrap();

    let restored = Session::load(&path, &config).unwrap();
    assert!(restored.tabs[0].options.highlight_matches);
    assert!(
        restored.tabs[0]
            .options
            .presets
            .iter()
            .all(|preset| preset.highlight_match)
    );
    assert_eq!(restored.results_height, None);

    // Hidden filter pane (`0.0`) is a valid persisted height.
    let mut hidden = legacy.clone();
    hidden["results_height"] = serde_json::json!(0.0);
    fs::write(&path, serde_json::to_vec_pretty(&hidden).unwrap()).unwrap();
    assert_eq!(
        Session::load(&path, &config).unwrap().results_height,
        Some(0.0)
    );

    for bad_height in [-10.0, -0.5, 9999.0] {
        let mut invalid = legacy.clone();
        invalid["results_height"] = serde_json::json!(bad_height);
        fs::write(&path, serde_json::to_vec_pretty(&invalid).unwrap()).unwrap();
        assert!(
            Session::load(&path, &config).is_err(),
            "accepted invalid results_height {bad_height}"
        );
    }
}

#[test]
fn search_highlight_ranges_cover_regex_groups_full_patterns_and_literals() {
    let mut options = Config::load(None).unwrap().search;
    options.mode = SearchMode::Regex;
    let grouped = search::compile(r"job #(\d+).*in (\d+ms)", &options).unwrap();
    let line = "INFO worker Completed job #7842 in 908ms";
    let ranges = search::highlight_ranges(line, &grouped, options.mode);
    assert_eq!(
        ranges.iter().map(|r| &line[r.clone()]).collect::<Vec<_>>(),
        vec!["7842", "908ms"]
    );

    let ungrouped = search::compile(r"ERROR|WARN", &options).unwrap();
    let line2 = "2026 ERROR and WARN";
    let ranges2 = search::highlight_ranges(line2, &ungrouped, options.mode);
    assert_eq!(
        ranges2
            .iter()
            .map(|r| &line2[r.clone()])
            .collect::<Vec<_>>(),
        vec!["ERROR", "WARN"]
    );

    options.mode = SearchMode::Literal;
    let literal = search::compile("(attempt 2/3)", &options).unwrap();
    let line3 = "Retrying job #7842 (attempt 2/3)";
    let ranges3 = search::highlight_ranges(line3, &literal, options.mode);
    assert_eq!(
        ranges3
            .iter()
            .map(|r| &line3[r.clone()])
            .collect::<Vec<_>>(),
        vec!["(attempt 2/3)"]
    );
}

#[test]
fn rotation_to_a_larger_file_is_recognized_as_replacement() {
    let (dir, old) = indexed(b"small\n");
    let path = dir.path().join("test.log");
    fs::rename(&path, dir.path().join("old.log")).unwrap();
    fs::write(&path, b"much larger replacement file\nmore\n").unwrap();
    let new = LogFile::open(&path, &AtomicBool::new(false)).unwrap();
    #[cfg(unix)]
    assert!(old.replaced_by(&new));
    assert_eq!(new.widest_line, 0);
}

#[test]
fn committed_schemas_match_the_rust_types() {
    let directory = tempdir().unwrap();
    config::write_schemas(directory.path()).unwrap();
    for entry in fs::read_dir(directory.path()).unwrap() {
        let entry = entry.unwrap();
        let committed = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("schemas")
            .join(entry.file_name());
        let generated: serde_json::Value =
            serde_json::from_slice(&fs::read(entry.path()).unwrap()).unwrap();
        let checked_in: serde_json::Value =
            serde_json::from_slice(&fs::read(committed).unwrap()).unwrap();
        assert_eq!(
            generated, checked_in,
            "Regenerate schemas after changing configuration types"
        );
    }
}
