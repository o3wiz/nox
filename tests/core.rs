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
    config.window.filter_resize_sensitivity = 0.05;
    assert!(config.validate().is_err());
    let mut config = Config::load(None).unwrap();
    config.window.filter_resize_sensitivity = 10.5;
    assert!(config.validate().is_err());
    let mut config = Config::load(None).unwrap();
    config.window.filter_resize_sensitivity = f32::NAN;
    assert!(config.validate().is_err());
    let mut config = Config::load(None).unwrap();
    config.window.filter_resize_sensitivity = f32::INFINITY;
    assert!(config.validate().is_err());
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
    config.appearance.text_selection_color = "blue".into();
    assert!(config.validate().is_err());
    let mut config = Config::load(None).unwrap();
    config.appearance.text_selection_opacity = -0.05;
    assert!(config.validate().is_err());
    let mut config = Config::load(None).unwrap();
    config.appearance.text_selection_opacity = 1.2;
    assert!(config.validate().is_err());
    let mut config = Config::load(None).unwrap();
    config.appearance.text_selection_opacity = f32::NAN;
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
fn presets_parse_select_text_and_report_validation_warnings() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("search.json");
    fs::write(
        &path,
        r#"{
            "presets": [
                { "name": "Immediate", "pattern": "err", "mode": "literal" },
                { "name": "NullSelect", "pattern": "warn", "mode": "literal", "select_text": null },
                { "name": "Port", "pattern": "GET .* port PORT_NUMBER", "mode": "regex", "select_text": "PORT_NUMBER" },
                { "name": "Unicode", "pattern": "user=שלום_ID!", "mode": "literal", "select_text": "שלום_ID" },
                { "name": "Repeated", "pattern": "HOST -> HOST", "mode": "literal", "select_text": "HOST" },
                { "name": "Missing", "pattern": "GET .* port 8080", "mode": "regex", "select_text": "PORT_NUMBER" },
                { "name": "Empty", "pattern": "GET .*", "mode": "regex", "select_text": "" }
            ]
        }"#,
    )
    .unwrap();

    // Mismatched or empty `select_text` produces warnings, not fatal load/validation errors.
    let config = Config::load(Some(dir.path())).unwrap();
    config.validate().unwrap();
    assert_eq!(config.search.presets.len(), 7);
    assert_eq!(config.search.presets[0].select_text, None);
    assert_eq!(config.search.presets[0].select_text_range(), None);
    assert_eq!(config.search.presets[0].select_text_warning(), None);

    assert_eq!(config.search.presets[1].select_text, None);
    assert_eq!(config.search.presets[1].select_text_range(), None);
    assert_eq!(config.search.presets[1].select_text_warning(), None);

    assert_eq!(
        config.search.presets[2].select_text.as_deref(),
        Some("PORT_NUMBER")
    );
    let port_range = config.search.presets[2].select_text_range().unwrap();
    assert_eq!(&config.search.presets[2].pattern[port_range], "PORT_NUMBER");
    assert_eq!(config.search.presets[2].select_text_warning(), None);

    let unicode_range = config.search.presets[3].select_text_range().unwrap();
    assert_eq!(&config.search.presets[3].pattern[unicode_range], "שלום_ID");
    assert_eq!(config.search.presets[3].select_text_warning(), None);

    assert_eq!(config.search.presets[4].select_text_range(), Some(0..4));
    assert_eq!(config.search.presets[4].select_text_warning(), None);

    assert_eq!(config.search.presets[5].select_text_range(), None);
    let missing_warning = config.search.presets[5]
        .select_text_warning()
        .expect("missing select_text should warn");
    assert!(missing_warning.contains("Missing"));
    assert!(missing_warning.contains("PORT_NUMBER"));

    assert_eq!(config.search.presets[6].select_text_range(), None);
    let empty_warning = config.search.presets[6]
        .select_text_warning()
        .expect("empty select_text should warn");
    assert!(empty_warning.contains("Empty"));
    assert!(empty_warning.contains("empty"));

    let warnings = config.warnings();
    assert_eq!(warnings.len(), 2);
    assert_eq!(warnings[0], missing_warning);
    assert_eq!(warnings[1], empty_warning);

    // Session serialization omits `select_text` when `None` and preserves it when `Some`.
    let serialized = serde_json::to_value(&config.search.presets).unwrap();
    assert!(serialized[0].get("select_text").is_none());
    assert_eq!(serialized[2]["select_text"], "PORT_NUMBER");

    // Non-string values for `select_text` are rejected.
    for invalid in [
        r#"{"presets":[{"name":"Bad","pattern":"err","mode":"regex","select_text":123}]}"#,
        r#"{"presets":[{"name":"Bad","pattern":"err","mode":"regex","select_text":true}]}"#,
        r#"{"presets":[{"name":"Bad","pattern":"err","mode":"regex","select_text":[]}]}"#,
        r#"{"presets":[{"name":"Bad","pattern":"err","mode":"regex","select_text":{}}]}"#,
    ] {
        fs::write(&path, invalid).unwrap();
        assert!(
            Config::load(Some(dir.path())).is_err(),
            "accepted invalid preset select_text {invalid}"
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
fn appearance_overlays_configurable_text_selection_color_and_opacity() {
    let defaults = Config::load(None).unwrap();
    assert_eq!(defaults.appearance.text_selection_color, "#38BDF8");
    assert!((defaults.appearance.text_selection_opacity - 0.35).abs() < 1e-6);

    let dir = tempdir().unwrap();
    let path = dir.path().join("appearance.json");
    fs::write(
        &path,
        r##"{"text_selection_color":"#FF5500","text_selection_opacity":0.65}"##,
    )
    .unwrap();
    let custom = Config::load(Some(dir.path())).unwrap();
    assert_eq!(custom.appearance.text_selection_color, "#FF5500");
    assert!((custom.appearance.text_selection_opacity - 0.65).abs() < 1e-6);
    assert_eq!(custom.appearance.selection, defaults.appearance.selection);

    // Partial overrides and boundary opacity values (0.0 and 1.0) work cleanly.
    fs::write(&path, r##"{"text_selection_opacity":0.0}"##).unwrap();
    let transparent = Config::load(Some(dir.path())).unwrap();
    assert_eq!(
        transparent.appearance.text_selection_color,
        defaults.appearance.text_selection_color
    );
    assert_eq!(transparent.appearance.text_selection_opacity, 0.0);

    fs::write(&path, r##"{"text_selection_opacity":1.0}"##).unwrap();
    let opaque = Config::load(Some(dir.path())).unwrap();
    assert_eq!(opaque.appearance.text_selection_opacity, 1.0);

    for invalid in [
        r##"{"text_selection_color":"blue"}"##,
        r##"{"text_selection_color":"#FFF"}"##,
        r##"{"text_selection_color":123}"##,
        r##"{"text_selection_opacity":-0.1}"##,
        r##"{"text_selection_opacity":1.05}"##,
        r##"{"text_selection_opacity":"0.5"}"##,
        r##"{"text_selection_opacity":null}"##,
    ] {
        fs::write(&path, invalid).unwrap();
        assert!(
            Config::load(Some(dir.path())).is_err(),
            "accepted invalid text selection appearance {invalid}"
        );
    }
}

#[test]
fn window_overlays_and_validates_filter_resize_sensitivity() {
    let defaults = Config::load(None).unwrap();
    assert_eq!(defaults.window.filter_resize_sensitivity, 1.0);

    let dir = tempdir().unwrap();
    let path = dir.path().join("window.json");
    fs::write(&path, r#"{"filter_resize_sensitivity": 2.5}"#).unwrap();
    let custom = Config::load(Some(dir.path())).unwrap();
    assert!((custom.window.filter_resize_sensitivity - 2.5).abs() < 1e-6);
    assert_eq!(custom.window.results_height, defaults.window.results_height);

    // Boundary values 0.1 and 10.0 are accepted.
    for valid in [0.1f32, 10.0] {
        fs::write(&path, format!(r#"{{"filter_resize_sensitivity":{valid}}}"#)).unwrap();
        let loaded = Config::load(Some(dir.path())).unwrap();
        assert!((loaded.window.filter_resize_sensitivity - valid).abs() < 1e-6);
    }

    for invalid in [
        r#"{"filter_resize_sensitivity":0}"#,
        r#"{"filter_resize_sensitivity":0.09}"#,
        r#"{"filter_resize_sensitivity":-1.0}"#,
        r#"{"filter_resize_sensitivity":10.1}"#,
        r#"{"filter_resize_sensitivity":"2.0"}"#,
        r#"{"filter_resize_sensitivity":null}"#,
    ] {
        fs::write(&path, invalid).unwrap();
        assert!(
            Config::load(Some(dir.path())).is_err(),
            "accepted invalid filter_resize_sensitivity {invalid}"
        );
    }
}

#[test]
fn viewer_overlays_and_validates_cursor_surrounding_lines() {
    let defaults = Config::load(None).unwrap();
    assert_eq!(defaults.viewer.cursor_surrounding_lines, 0);

    let dir = tempdir().unwrap();
    let path = dir.path().join("viewer.json");
    fs::write(&path, r#"{"cursor_surrounding_lines": 5}"#).unwrap();
    let custom = Config::load(Some(dir.path())).unwrap();
    assert_eq!(custom.viewer.cursor_surrounding_lines, 5);
    assert_eq!(custom.viewer.line_height, defaults.viewer.line_height);

    // Boundary values 0 and 1000 are accepted.
    for valid in [0usize, 1000] {
        fs::write(&path, format!(r#"{{"cursor_surrounding_lines":{valid}}}"#)).unwrap();
        assert_eq!(
            Config::load(Some(dir.path()))
                .unwrap()
                .viewer
                .cursor_surrounding_lines,
            valid
        );
    }

    for invalid in [
        r#"{"cursor_surrounding_lines":-1}"#,
        r#"{"cursor_surrounding_lines":1001}"#,
        r#"{"cursor_surrounding_lines":2.5}"#,
        r#"{"cursor_surrounding_lines":"5"}"#,
        r#"{"cursor_surrounding_lines":null}"#,
    ] {
        fs::write(&path, invalid).unwrap();
        assert!(
            Config::load(Some(dir.path())).is_err(),
            "accepted invalid cursor_surrounding_lines {invalid}"
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
    assert!(dir.path().join("schemas/common.schema.json").is_file());
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
        let rejects_unknown = schema["additionalProperties"] == false
            || (schema.get("$ref").is_some() && schema["unevaluatedProperties"] == false);
        assert!(
            rejects_unknown,
            "{name}.schema.json must reject unknown properties"
        );
        assert!(schema["properties"]["$schema"].is_object());
    }

    // Existing common schema file alone also prevents overwriting.
    let existing_common = tempdir().unwrap();
    fs::create_dir_all(existing_common.path().join("schemas")).unwrap();
    fs::write(
        existing_common.path().join("schemas/common.schema.json"),
        "{}",
    )
    .unwrap();
    assert!(config::write_defaults(existing_common.path()).is_err());
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
        zoom_percent: Some(130),
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
    assert_eq!(restored.zoom_percent, Some(130));
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
        zoom_percent: None,
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
    assert_eq!(restored.zoom_percent, None);

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

    for bad_zoom in [0, 40, 49, 301, 500] {
        let mut invalid = legacy.clone();
        invalid["zoom_percent"] = serde_json::json!(bad_zoom);
        fs::write(&path, serde_json::to_vec_pretty(&invalid).unwrap()).unwrap();
        assert!(
            Session::load(&path, &config).is_err(),
            "accepted invalid zoom_percent {bad_zoom}"
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
    assert_ne!(old.stamp, new.stamp);
    #[cfg(unix)]
    assert!(old.replaced_by(&new));
    assert_eq!(new.widest_line, 0);
}

#[test]
fn committed_schemas_match_the_rust_types() {
    let directory = tempdir().unwrap();
    config::write_schemas(directory.path()).unwrap();
    let committed_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("schemas");

    let mut generated_files = BTreeSet::new();
    for entry in fs::read_dir(directory.path()).unwrap() {
        let entry = entry.unwrap();
        let file_name = entry.file_name();
        generated_files.insert(file_name.clone());
        let committed = committed_dir.join(&file_name);
        let generated: serde_json::Value =
            serde_json::from_slice(&fs::read(entry.path()).unwrap()).unwrap();
        let checked_in: serde_json::Value =
            serde_json::from_slice(&fs::read(committed).unwrap()).unwrap();
        assert_eq!(
            generated, checked_in,
            "Regenerate schemas after changing configuration types"
        );
    }

    let committed_files: BTreeSet<_> = fs::read_dir(&committed_dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(
        committed_files, generated_files,
        "Committed schemas directory has stale or missing schema files"
    );
}

#[test]
fn shared_schema_definitions_are_deduplicated_and_all_refs_resolve() {
    use serde_json::Value;
    use std::collections::BTreeMap;

    fn collect_refs(value: &Value, refs: &mut Vec<String>) {
        match value {
            Value::Object(map) => {
                if let Some(reference) = map.get("$ref").and_then(Value::as_str) {
                    refs.push(reference.to_string());
                }
                for child in map.values() {
                    collect_refs(child, refs);
                }
            }
            Value::Array(items) => {
                for item in items {
                    collect_refs(item, refs);
                }
            }
            _ => {}
        }
    }

    let directory = tempdir().unwrap();
    config::write_schemas(directory.path()).unwrap();

    let mut schemas: BTreeMap<String, Value> = BTreeMap::new();
    for entry in fs::read_dir(directory.path()).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().to_string_lossy().into_owned();
        let value: Value = serde_json::from_slice(&fs::read(entry.path()).unwrap()).unwrap();
        schemas.insert(name, value);
    }

    let common = schemas
        .get("common.schema.json")
        .expect("common.schema.json must be generated");
    let common_defs = common["$defs"]
        .as_object()
        .expect("common.schema.json must define $defs");
    for expected in ["HexColor", "Preset", "SearchConfig", "SearchMode"] {
        assert!(
            common_defs.contains_key(expected),
            "common.schema.json missing shared definition {expected}"
        );
    }

    // `search.schema.json` and `session.schema.json` must share `SearchConfig`, `Preset`, and
    // `SearchMode` via `common.schema.json` without duplicating them locally.
    let search = &schemas["search.schema.json"];
    assert_eq!(search["$ref"], "common.schema.json#/$defs/SearchConfig");
    assert_eq!(search["unevaluatedProperties"], false);
    assert!(search.get("$defs").is_none());

    let session = &schemas["session.schema.json"];
    let session_defs = session["$defs"].as_object().unwrap();
    assert!(!session_defs.contains_key("Preset"));
    assert!(!session_defs.contains_key("SearchMode"));
    assert_eq!(
        session_defs["SearchConfig"]["$ref"],
        "common.schema.json#/$defs/SearchConfig"
    );
    assert_eq!(session_defs["SearchConfig"]["unevaluatedProperties"], false);
    assert!(session_defs["SearchConfig"]["required"].is_array());

    // `appearance.schema.json` and `highlighting.schema.json` share `HexColor` via common schema,
    // while `FontSize` stays local to `appearance.schema.json`.
    let appearance = &schemas["appearance.schema.json"];
    assert_eq!(
        appearance["properties"]["background"]["$ref"],
        "common.schema.json#/$defs/HexColor"
    );
    assert_eq!(
        appearance["properties"]["ui_font_size"]["$ref"],
        "#/$defs/FontSize"
    );
    assert_eq!(
        appearance["properties"]["font_size"]["$ref"],
        "#/$defs/FontSize"
    );
    assert!(
        !appearance["$defs"]
            .as_object()
            .unwrap()
            .contains_key("HexColor")
    );

    let highlighting = &schemas["highlighting.schema.json"];
    assert_eq!(
        highlighting["$defs"]["HighlightRule"]["properties"]["color"]["$ref"],
        "common.schema.json#/$defs/HexColor"
    );
    assert!(
        !highlighting["$defs"]
            .as_object()
            .unwrap()
            .contains_key("HexColor")
    );

    // No `$defs` body may be duplicated across any two schema files.
    let mut seen_defs: BTreeMap<String, (String, Value)> = BTreeMap::new();
    for (file_name, schema) in &schemas {
        if let Some(defs) = schema.get("$defs").and_then(Value::as_object) {
            for (def_name, def_val) in defs {
                if let Some((prev_file, prev_val)) = seen_defs.get(def_name) {
                    assert_ne!(
                        prev_val, def_val,
                        "Duplicate $defs/{def_name} in {prev_file} and {file_name}"
                    );
                } else {
                    seen_defs.insert(def_name.clone(), (file_name.clone(), def_val.clone()));
                }
            }
        }
    }

    // Every `$ref` across every schema file must resolve to an existing `$defs` target.
    for (file_name, schema) in &schemas {
        let mut refs = Vec::new();
        collect_refs(schema, &mut refs);
        for reference in refs {
            if let Some(local_name) = reference.strip_prefix("#/$defs/") {
                assert!(
                    schema["$defs"]
                        .as_object()
                        .is_some_and(|defs| defs.contains_key(local_name)),
                    "Unresolved local $ref {reference:?} in {file_name}"
                );
            } else if let Some((target_file, local_name)) = reference.split_once("#/$defs/") {
                let target_schema = schemas.get(target_file).unwrap_or_else(|| {
                    panic!("Unresolved external schema file {target_file:?} in {file_name}")
                });
                assert!(
                    target_schema["$defs"]
                        .as_object()
                        .is_some_and(|defs| defs.contains_key(local_name)),
                    "Unresolved external $ref {reference:?} in {file_name}"
                );
            } else {
                panic!("Unexpected $ref format {reference:?} in {file_name}");
            }
        }
    }
}

#[test]
fn migrate_config_adds_missing_files_fields_preset_defaults_bindings_and_updates_schemas() {
    let dir = tempdir().unwrap();

    // Partial window.json without `$schema` and with a deprecated field.
    fs::write(
        dir.path().join("window.json"),
        r#"{
  "width": 1600,
  "legacy_opacity": 0.85
}
"#,
    )
    .unwrap();

    // Partial appearance.json missing newer color/selection fields and keeping a deprecated field.
    fs::write(
        dir.path().join("appearance.json"),
        r##"{
  "$schema": "schemas/appearance.schema.json",
  "accent": "#FF00FF",
  "old_theme": "solarized"
}
"##,
    )
    .unwrap();

    // Partial viewer.json missing newer fields.
    fs::write(
        dir.path().join("viewer.json"),
        r#"{
  "$schema": "schemas/viewer.schema.json",
  "tab_width": 8
}
"#,
    )
    .unwrap();

    // Partial search.json with custom presets: one missing `highlight_match` + deprecated key,
    // one explicitly disabling `highlight_match`.
    fs::write(
        dir.path().join("search.json"),
        r#"{
  "$schema": "schemas/search.schema.json",
  "mode": "literal",
  "presets": [
    {
      "name": "Custom",
      "pattern": "timeout",
      "mode": "literal",
      "deprecated_priority": 3
    },
    {
      "name": "NoHighlight",
      "pattern": "debug",
      "mode": "regex",
      "highlight_match": false
    }
  ]
}
"#,
    )
    .unwrap();

    // Partial keyboard.json with:
    // - customized key for `open` (default `secondary-o` must NOT be re-added),
    // - conflicting key `secondary-=` bound to `next_tab` (default `zoom_in` must NOT collide),
    // - a deprecated binding with an extra field (must be preserved verbatim),
    // - missing `zoom_out`, `zoom_reset`, and all `FilePath` bindings (including both
    //   `shift-tab` and `up` for `previous_line`).
    fs::write(
        dir.path().join("keyboard.json"),
        r#"{
  "$schema": "schemas/keyboard.schema.json",
  "bindings": [
    {
      "keys": "ctrl-alt-o",
      "context": "Workspace",
      "action": "open"
    },
    {
      "keys": "secondary-=",
      "context": "Workspace",
      "action": "next_tab"
    },
    {
      "keys": "ctrl-shift-z",
      "context": "Log",
      "action": "legacy_action",
      "deprecated_note": "keep"
    }
  ]
}
"#,
    )
    .unwrap();

    // Omit `highlighting.json` and `files.json` so migration creates them.
    // Seed an outdated schema and an unrelated file that must stay untouched.
    fs::create_dir_all(dir.path().join("schemas")).unwrap();
    fs::write(
        dir.path().join("schemas/window.schema.json"),
        r#"{"outdated":true}"#,
    )
    .unwrap();
    fs::write(dir.path().join("notes.txt"), "keep me\n").unwrap();

    let report = config::migrate_config(dir.path()).unwrap();
    assert_eq!(
        report.created_files,
        vec!["highlighting.json".to_string(), "files.json".to_string()]
    );
    assert_eq!(
        report.updated_files,
        vec![
            "window.json".to_string(),
            "appearance.json".to_string(),
            "viewer.json".to_string(),
            "search.json".to_string(),
            "keyboard.json".to_string(),
        ]
    );
    assert!(report.added_fields > 0);
    assert!(report.added_bindings > 0);
    assert_eq!(report.schemas_updated, config::DEFAULTS.len() + 2);

    // Check window.json: `$schema` inserted first, existing + deprecated fields kept, missing added.
    let window: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(dir.path().join("window.json")).unwrap()).unwrap();
    let window_obj = window.as_object().unwrap();
    assert_eq!(
        window_obj.keys().next().map(String::as_str),
        Some("$schema")
    );
    assert_eq!(window["$schema"], "schemas/window.schema.json");
    assert_eq!(window["width"], 1600);
    assert_eq!(window["legacy_opacity"], 0.85);
    assert_eq!(window["height"], 850);
    assert_eq!(window["maximized"], false);
    assert_eq!(window["results_height"], 250);
    assert_eq!(window["filter_resize_sensitivity"], 1.0);
    assert_eq!(window["show_status_bar"], true);

    // Check appearance.json: custom + deprecated preserved, newer fields added.
    let appearance: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(dir.path().join("appearance.json")).unwrap())
            .unwrap();
    assert_eq!(appearance["accent"], "#FF00FF");
    assert_eq!(appearance["old_theme"], "solarized");
    assert_eq!(appearance["text_selection_color"], "#38BDF8");
    assert_eq!(appearance["text_selection_opacity"], 0.35);
    assert_eq!(appearance["match_foreground"], "#0B1016");
    assert_eq!(appearance["match_background"], "#E0AF4B");

    // Check viewer.json.
    let viewer: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(dir.path().join("viewer.json")).unwrap()).unwrap();
    assert_eq!(viewer["tab_width"], 8);
    assert_eq!(viewer["cursor_surrounding_lines"], 0);
    assert_eq!(viewer["overview_width"], 16);

    // Check search.json: user presets preserved, missing preset field defaulted, deprecated kept.
    let search: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(dir.path().join("search.json")).unwrap()).unwrap();
    assert_eq!(search["mode"], "literal");
    assert_eq!(search["highlight_matches"], true);
    let presets = search["presets"].as_array().unwrap();
    assert_eq!(presets.len(), 2);
    assert_eq!(presets[0]["name"], "Custom");
    assert_eq!(presets[0]["highlight_match"], true);
    assert_eq!(presets[0]["deprecated_priority"], 3);
    assert!(presets[0].get("select_text").is_none());
    assert_eq!(presets[1]["name"], "NoHighlight");
    assert_eq!(presets[1]["highlight_match"], false);
    assert!(presets[1].get("select_text").is_none());

    // Check keyboard.json: custom & deprecated preserved, conflicts skipped, missing added.
    let keyboard: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(dir.path().join("keyboard.json")).unwrap())
            .unwrap();
    let bindings = keyboard["bindings"].as_array().unwrap();
    assert!(bindings.iter().any(|b| {
        b["keys"] == "ctrl-alt-o" && b["context"] == "Workspace" && b["action"] == "open"
    }));
    assert!(!bindings.iter().any(|b| {
        b["keys"] == "secondary-o" && b["context"] == "Workspace" && b["action"] == "open"
    }));
    assert!(bindings.iter().any(|b| {
        b["keys"] == "secondary-=" && b["context"] == "Workspace" && b["action"] == "next_tab"
    }));
    assert!(!bindings.iter().any(|b| {
        b["keys"] == "secondary-=" && b["context"] == "Workspace" && b["action"] == "zoom_in"
    }));
    assert!(bindings.iter().any(|b| {
        b["keys"] == "ctrl-shift-z"
            && b["context"] == "Log"
            && b["action"] == "legacy_action"
            && b["deprecated_note"] == "keep"
    }));
    assert!(bindings.iter().any(|b| {
        b["keys"] == "secondary--" && b["context"] == "Workspace" && b["action"] == "zoom_out"
    }));
    assert!(bindings.iter().any(|b| {
        b["keys"] == "shift-tab" && b["context"] == "FilePath" && b["action"] == "previous_line"
    }));
    assert!(bindings.iter().any(|b| {
        b["keys"] == "up" && b["context"] == "FilePath" && b["action"] == "previous_line"
    }));
    assert!(bindings.iter().any(|b| {
        b["keys"] == "c" && b["context"] == "Log" && b["action"] == "copy_match_results"
    }));

    // Created missing files should point to local `schemas/`.
    let files_cfg: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(dir.path().join("files.json")).unwrap()).unwrap();
    assert_eq!(files_cfg["$schema"], "schemas/files.schema.json");
    let highlighting: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(dir.path().join("highlighting.json")).unwrap())
            .unwrap();
    assert_eq!(highlighting["$schema"], "schemas/highlighting.schema.json");

    // Schemas should all be refreshed and match fresh schema generation.
    let fresh_schemas = tempdir().unwrap();
    config::write_schemas(fresh_schemas.path()).unwrap();
    for entry in fs::read_dir(fresh_schemas.path()).unwrap() {
        let entry = entry.unwrap();
        let expected: serde_json::Value =
            serde_json::from_slice(&fs::read(entry.path()).unwrap()).unwrap();
        let migrated: serde_json::Value = serde_json::from_slice(
            &fs::read(dir.path().join("schemas").join(entry.file_name())).unwrap(),
        )
        .unwrap();
        assert_eq!(migrated, expected);
    }

    // Unrelated user files are preserved.
    assert_eq!(
        fs::read_to_string(dir.path().join("notes.txt")).unwrap(),
        "keep me\n"
    );

    // Re-running migration is idempotent (no further file or field changes).
    let second = config::migrate_config(dir.path()).unwrap();
    assert!(!second.has_config_changes());
    assert!(second.created_files.is_empty());
    assert!(second.updated_files.is_empty());
    assert_eq!(second.added_fields, 0);
    assert_eq!(second.added_bindings, 0);
    assert_eq!(second.schemas_updated, config::DEFAULTS.len() + 2);
}

#[test]
fn migrate_config_preserves_deprecated_fields_while_load_remains_strict() {
    let dir = tempdir().unwrap();
    config::write_defaults(dir.path()).unwrap();

    // Remove one current field and add a deprecated field.
    let window_path = dir.path().join("window.json");
    let mut window: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&window_path).unwrap()).unwrap();
    window.as_object_mut().unwrap().remove("show_status_bar");
    window
        .as_object_mut()
        .unwrap()
        .insert("deprecated_flag".into(), serde_json::json!(true));
    fs::write(&window_path, serde_json::to_string_pretty(&window).unwrap()).unwrap();

    let report = config::migrate_config(dir.path()).unwrap();
    assert_eq!(report.updated_files, vec!["window.json".to_string()]);
    assert_eq!(report.added_fields, 1);

    let migrated: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&window_path).unwrap()).unwrap();
    assert_eq!(migrated["show_status_bar"], true);
    assert_eq!(migrated["deprecated_flag"], true);

    // Runtime loading keeps strict `deny_unknown_fields` validation.
    assert!(Config::load(Some(dir.path())).is_err());

    // Once the user removes the deprecated key, the migrated configuration loads cleanly.
    let mut cleaned = migrated;
    cleaned.as_object_mut().unwrap().remove("deprecated_flag");
    fs::write(
        &window_path,
        serde_json::to_string_pretty(&cleaned).unwrap(),
    )
    .unwrap();
    let loaded = Config::load(Some(dir.path())).unwrap();
    assert!(loaded.window.show_status_bar);
}

#[test]
fn migrate_config_rejects_missing_dir_and_corrupt_json_without_partial_writes() {
    let dir = tempdir().unwrap();

    // Nonexistent directory fails.
    assert!(config::migrate_config(&dir.path().join("missing")).is_err());

    // Regular file instead of directory fails.
    let file_path = dir.path().join("not-a-dir");
    fs::write(&file_path, "{}").unwrap();
    assert!(config::migrate_config(&file_path).is_err());

    // Corrupt JSON in one file prevents modifying any earlier files or creating missing ones.
    let broken_dir = dir.path().join("broken");
    fs::create_dir_all(&broken_dir).unwrap();
    let original_window = r#"{"width":1500}"#;
    fs::write(broken_dir.join("window.json"), original_window).unwrap();
    fs::write(broken_dir.join("viewer.json"), "{ invalid").unwrap();
    assert!(config::migrate_config(&broken_dir).is_err());
    assert_eq!(
        fs::read_to_string(broken_dir.join("window.json")).unwrap(),
        original_window
    );
    assert!(!broken_dir.join("files.json").exists());
    assert!(!broken_dir.join("schemas").exists());

    // Non-object root JSON also fails without partial writes.
    fs::write(broken_dir.join("viewer.json"), "[]").unwrap();
    assert!(config::migrate_config(&broken_dir).is_err());
    assert_eq!(
        fs::read_to_string(broken_dir.join("window.json")).unwrap(),
        original_window
    );

    // A regular file occupying `schemas` also fails cleanly before touching config files.
    fs::remove_file(broken_dir.join("viewer.json")).unwrap();
    fs::write(broken_dir.join("schemas"), "not a directory").unwrap();
    assert!(config::migrate_config(&broken_dir).is_err());
    assert_eq!(
        fs::read_to_string(broken_dir.join("window.json")).unwrap(),
        original_window
    );
}

#[test]
fn text_editing_line_selection_and_deletion_config_defaults_and_migration() {
    use nox::config::{Command, KeyContext};

    let defaults = Config::load(None).unwrap();
    let has_binding = |cfg: &Config, keys: &str, context: KeyContext, action: Command| {
        cfg.keyboard
            .bindings
            .iter()
            .any(|b| b.keys == keys && b.context == context && b.action == action)
    };

    // Existing move bindings remain intact.
    assert!(has_binding(
        &defaults,
        "cmd-left",
        KeyContext::TextInput,
        Command::MoveToLineStart
    ));
    assert!(has_binding(
        &defaults,
        "cmd-right",
        KeyContext::TextInput,
        Command::MoveToLineEnd
    ));
    assert!(has_binding(
        &defaults,
        "home",
        KeyContext::TextInput,
        Command::MoveToLineStart
    ));
    assert!(has_binding(
        &defaults,
        "end",
        KeyContext::TextInput,
        Command::MoveToLineEnd
    ));

    // New default select-to-line bindings exist in TextInput (and not shift-home/shift-end).
    assert!(has_binding(
        &defaults,
        "cmd-shift-left",
        KeyContext::TextInput,
        Command::SelectToLineStart
    ));
    assert!(has_binding(
        &defaults,
        "shift-up",
        KeyContext::TextInput,
        Command::SelectToLineStart
    ));
    assert!(has_binding(
        &defaults,
        "cmd-shift-right",
        KeyContext::TextInput,
        Command::SelectToLineEnd
    ));
    assert!(has_binding(
        &defaults,
        "shift-down",
        KeyContext::TextInput,
        Command::SelectToLineEnd
    ));
    assert!(!defaults.keyboard.bindings.iter().any(|b| {
        b.context == KeyContext::TextInput && matches!(b.keys.as_str(), "shift-home" | "shift-end")
    }));

    // Delete-to-line-start defaults to cmd-backspace in TextInput; delete-to-line-end has no default.
    assert!(has_binding(
        &defaults,
        "cmd-backspace",
        KeyContext::TextInput,
        Command::DeleteToLineStart
    ));
    assert!(
        !defaults
            .keyboard
            .bindings
            .iter()
            .any(|b| b.action == Command::DeleteToLineEnd)
    );

    // Custom keyboard.json can override delete_to_line_start and configure delete_to_line_end and custom select bindings.
    let custom_dir = tempdir().unwrap();
    fs::write(
        custom_dir.path().join("keyboard.json"),
        r#"{
  "bindings": [
    { "keys": "ctrl-u", "context": "TextInput", "action": "delete_to_line_start" },
    { "keys": "alt-k", "context": "TextInput", "action": "delete_to_line_end" },
    { "keys": "shift-home", "context": "TextInput", "action": "select_to_line_start" },
    { "keys": "shift-end", "context": "TextInput", "action": "select_to_line_end" }
  ]
}"#,
    )
    .unwrap();
    let custom = Config::load(Some(custom_dir.path())).unwrap();
    assert!(has_binding(
        &custom,
        "ctrl-u",
        KeyContext::TextInput,
        Command::DeleteToLineStart
    ));
    assert!(!has_binding(
        &custom,
        "cmd-backspace",
        KeyContext::TextInput,
        Command::DeleteToLineStart
    ));
    assert!(has_binding(
        &custom,
        "alt-k",
        KeyContext::TextInput,
        Command::DeleteToLineEnd
    ));

    // Migration adds missing select_to_line_* and delete_to_line_start defaults without adding delete_to_line_end.
    let migrate_dir = tempdir().unwrap();
    fs::write(
        migrate_dir.path().join("keyboard.json"),
        r#"{
  "$schema": "schemas/keyboard.schema.json",
  "bindings": [
    { "keys": "cmd-left", "context": "TextInput", "action": "move_to_line_start" },
    { "keys": "cmd-right", "context": "TextInput", "action": "move_to_line_end" }
  ]
}"#,
    )
    .unwrap();
    let report = config::migrate_config(migrate_dir.path()).unwrap();
    assert!(report.added_bindings > 0);
    let migrated = Config::load(Some(migrate_dir.path())).unwrap();
    assert!(has_binding(
        &migrated,
        "cmd-shift-left",
        KeyContext::TextInput,
        Command::SelectToLineStart
    ));
    assert!(has_binding(
        &migrated,
        "shift-up",
        KeyContext::TextInput,
        Command::SelectToLineStart
    ));
    assert!(has_binding(
        &migrated,
        "cmd-shift-right",
        KeyContext::TextInput,
        Command::SelectToLineEnd
    ));
    assert!(has_binding(
        &migrated,
        "shift-down",
        KeyContext::TextInput,
        Command::SelectToLineEnd
    ));
    assert!(has_binding(
        &migrated,
        "cmd-backspace",
        KeyContext::TextInput,
        Command::DeleteToLineStart
    ));
    assert!(
        !migrated
            .keyboard
            .bindings
            .iter()
            .any(|b| b.action == Command::DeleteToLineEnd)
    );

    // Migration preserves an existing custom delete_to_line_start binding instead of adding cmd-backspace.
    let custom_migrate_dir = tempdir().unwrap();
    fs::write(
        custom_migrate_dir.path().join("keyboard.json"),
        r#"{
  "$schema": "schemas/keyboard.schema.json",
  "bindings": [
    { "keys": "ctrl-u", "context": "TextInput", "action": "delete_to_line_start" }
  ]
}"#,
    )
    .unwrap();
    config::migrate_config(custom_migrate_dir.path()).unwrap();
    let custom_migrated = Config::load(Some(custom_migrate_dir.path())).unwrap();
    assert!(has_binding(
        &custom_migrated,
        "ctrl-u",
        KeyContext::TextInput,
        Command::DeleteToLineStart
    ));
    assert!(!has_binding(
        &custom_migrated,
        "cmd-backspace",
        KeyContext::TextInput,
        Command::DeleteToLineStart
    ));
}

#[test]
fn first_and_last_line_shortcuts_defaults_and_migration() {
    use nox::config::{Command, KeyContext};

    let defaults = Config::load(None).unwrap();
    let has_binding = |cfg: &Config, keys: &str, context: KeyContext, action: Command| {
        cfg.keyboard
            .bindings
            .iter()
            .any(|b| b.keys == keys && b.context == context && b.action == action)
    };

    // Log supports both home/end and secondary-up/down for first/last line and selection extension.
    for (keys, action) in [
        ("home", Command::FirstLine),
        ("secondary-up", Command::FirstLine),
        ("end", Command::LastLine),
        ("secondary-down", Command::LastLine),
        ("shift-home", Command::SelectFirstLine),
        ("secondary-shift-up", Command::SelectFirstLine),
        ("shift-end", Command::SelectLastLine),
        ("secondary-shift-down", Command::SelectLastLine),
    ] {
        assert!(
            has_binding(&defaults, keys, KeyContext::Log, action),
            "missing Log default binding {keys} -> {action:?}"
        );
    }

    // Presets and FilePath menus both support secondary-up/down for first/last line.
    for context in [KeyContext::Presets, KeyContext::FilePath] {
        assert!(has_binding(
            &defaults,
            "secondary-up",
            context,
            Command::FirstLine
        ));
        assert!(has_binding(
            &defaults,
            "secondary-down",
            context,
            Command::LastLine
        ));
    }

    // Migrating an older keyboard.json with default home/end Log bindings and basic FilePath
    // bindings adds the newly introduced secondary-up/down and secondary-shift-up/down defaults.
    let upgrade_dir = tempdir().unwrap();
    fs::write(
        upgrade_dir.path().join("keyboard.json"),
        r#"{
  "$schema": "schemas/keyboard.schema.json",
  "bindings": [
    { "keys": "home", "context": "Log", "action": "first_line" },
    { "keys": "end", "context": "Log", "action": "last_line" },
    { "keys": "shift-home", "context": "Log", "action": "select_first_line" },
    { "keys": "shift-end", "context": "Log", "action": "select_last_line" },
    { "keys": "up", "context": "FilePath", "action": "previous_line" },
    { "keys": "down", "context": "FilePath", "action": "next_line" }
  ]
}"#,
    )
    .unwrap();
    let report = config::migrate_config(upgrade_dir.path()).unwrap();
    assert!(report.added_bindings > 0);
    let upgraded = Config::load(Some(upgrade_dir.path())).unwrap();
    for (keys, context, action) in [
        ("home", KeyContext::Log, Command::FirstLine),
        ("secondary-up", KeyContext::Log, Command::FirstLine),
        ("end", KeyContext::Log, Command::LastLine),
        ("secondary-down", KeyContext::Log, Command::LastLine),
        ("shift-home", KeyContext::Log, Command::SelectFirstLine),
        (
            "secondary-shift-up",
            KeyContext::Log,
            Command::SelectFirstLine,
        ),
        ("shift-end", KeyContext::Log, Command::SelectLastLine),
        (
            "secondary-shift-down",
            KeyContext::Log,
            Command::SelectLastLine,
        ),
        ("secondary-up", KeyContext::Presets, Command::FirstLine),
        ("secondary-down", KeyContext::Presets, Command::LastLine),
        ("secondary-up", KeyContext::FilePath, Command::FirstLine),
        ("secondary-down", KeyContext::FilePath, Command::LastLine),
    ] {
        assert!(
            has_binding(&upgraded, keys, context, action),
            "migrated config missing {keys} in {context:?} -> {action:?}"
        );
    }
    let second = config::migrate_config(upgrade_dir.path()).unwrap();
    assert_eq!(second.added_bindings, 0);

    // Custom rebindings and conflicting key slots remain respected during migration.
    let custom_dir = tempdir().unwrap();
    fs::write(
        custom_dir.path().join("keyboard.json"),
        r#"{
  "$schema": "schemas/keyboard.schema.json",
  "bindings": [
    { "keys": "g", "context": "Log", "action": "first_line" },
    { "keys": "secondary-down", "context": "FilePath", "action": "next_line" }
  ]
}"#,
    )
    .unwrap();
    config::migrate_config(custom_dir.path()).unwrap();
    let custom_migrated = Config::load(Some(custom_dir.path())).unwrap();
    assert!(has_binding(
        &custom_migrated,
        "g",
        KeyContext::Log,
        Command::FirstLine
    ));
    assert!(!has_binding(
        &custom_migrated,
        "secondary-up",
        KeyContext::Log,
        Command::FirstLine
    ));
    assert!(has_binding(
        &custom_migrated,
        "secondary-down",
        KeyContext::FilePath,
        Command::NextLine
    ));
    assert!(!has_binding(
        &custom_migrated,
        "secondary-down",
        KeyContext::FilePath,
        Command::LastLine
    ));
    assert!(has_binding(
        &custom_migrated,
        "secondary-up",
        KeyContext::FilePath,
        Command::FirstLine
    ));
}

#[test]
fn reset_horizontal_scroll_default_binding_and_migration() {
    use nox::config::{Command, KeyContext};

    let defaults = Config::load(None).unwrap();
    let has_binding = |cfg: &Config, keys: &str, context: KeyContext, action: Command| {
        cfg.keyboard
            .bindings
            .iter()
            .any(|b| b.keys == keys && b.context == context && b.action == action)
    };

    assert!(has_binding(
        &defaults,
        "secondary-left",
        KeyContext::Log,
        Command::ResetHorizontalScroll
    ));

    // Migrating an older keyboard.json with left/right scroll bindings adds secondary-left.
    let upgrade_dir = tempdir().unwrap();
    fs::write(
        upgrade_dir.path().join("keyboard.json"),
        r#"{
  "$schema": "schemas/keyboard.schema.json",
  "bindings": [
    { "keys": "left", "context": "Log", "action": "scroll_left" },
    { "keys": "right", "context": "Log", "action": "scroll_right" }
  ]
}"#,
    )
    .unwrap();
    let report = config::migrate_config(upgrade_dir.path()).unwrap();
    assert!(report.added_bindings > 0);
    let upgraded = Config::load(Some(upgrade_dir.path())).unwrap();
    assert!(has_binding(
        &upgraded,
        "secondary-left",
        KeyContext::Log,
        Command::ResetHorizontalScroll
    ));

    // Custom rebindings for reset_horizontal_scroll are preserved without adding secondary-left.
    let custom_dir = tempdir().unwrap();
    fs::write(
        custom_dir.path().join("keyboard.json"),
        r#"{
  "$schema": "schemas/keyboard.schema.json",
  "bindings": [
    { "keys": "0", "context": "Log", "action": "reset_horizontal_scroll" }
  ]
}"#,
    )
    .unwrap();
    config::migrate_config(custom_dir.path()).unwrap();
    let custom_migrated = Config::load(Some(custom_dir.path())).unwrap();
    assert!(has_binding(
        &custom_migrated,
        "0",
        KeyContext::Log,
        Command::ResetHorizontalScroll
    ));
    assert!(!has_binding(
        &custom_migrated,
        "secondary-left",
        KeyContext::Log,
        Command::ResetHorizontalScroll
    ));
}

#[test]
fn restore_closed_tab_default_binding_and_migration() {
    use nox::config::{Command, KeyContext};

    let defaults = Config::load(None).unwrap();
    let has_binding = |cfg: &Config, keys: &str, context: KeyContext, action: Command| {
        cfg.keyboard
            .bindings
            .iter()
            .any(|b| b.keys == keys && b.context == context && b.action == action)
    };

    assert!(has_binding(
        &defaults,
        "secondary-shift-t",
        KeyContext::Workspace,
        Command::RestoreClosedTab
    ));

    // Migrating an older keyboard.json with close_tab adds secondary-shift-t -> restore_closed_tab.
    let upgrade_dir = tempdir().unwrap();
    fs::write(
        upgrade_dir.path().join("keyboard.json"),
        r#"{
  "$schema": "schemas/keyboard.schema.json",
  "bindings": [
    { "keys": "secondary-w", "context": "Workspace", "action": "close_tab" }
  ]
}"#,
    )
    .unwrap();
    let report = config::migrate_config(upgrade_dir.path()).unwrap();
    assert!(report.added_bindings > 0);
    let upgraded = Config::load(Some(upgrade_dir.path())).unwrap();
    assert!(has_binding(
        &upgraded,
        "secondary-shift-t",
        KeyContext::Workspace,
        Command::RestoreClosedTab
    ));

    // Custom rebindings for restore_closed_tab are preserved without adding secondary-shift-t.
    let custom_dir = tempdir().unwrap();
    fs::write(
        custom_dir.path().join("keyboard.json"),
        r#"{
  "$schema": "schemas/keyboard.schema.json",
  "bindings": [
    { "keys": "secondary-alt-t", "context": "Workspace", "action": "restore_closed_tab" }
  ]
}"#,
    )
    .unwrap();
    config::migrate_config(custom_dir.path()).unwrap();
    let custom_migrated = Config::load(Some(custom_dir.path())).unwrap();
    assert!(has_binding(
        &custom_migrated,
        "secondary-alt-t",
        KeyContext::Workspace,
        Command::RestoreClosedTab
    ));
    assert!(!has_binding(
        &custom_migrated,
        "secondary-shift-t",
        KeyContext::Workspace,
        Command::RestoreClosedTab
    ));
}

#[test]
fn preset_regex_pattern_and_select_range_escapes_literal_presets_and_maps_selection() {
    use nox::config::{Preset, SearchMode};

    // Regex preset is returned verbatim with its raw select_text range.
    let regex_preset = Preset {
        name: "Port".into(),
        pattern: r"GET .* port=(PORT_NUM)\b".into(),
        mode: SearchMode::Regex,
        highlight_match: true,
        select_text: Some("PORT_NUM".into()),
    };
    let (pattern, range) = regex_preset.regex_pattern_and_select_range();
    assert_eq!(pattern, regex_preset.pattern);
    let range = range.expect("regex select range");
    assert_eq!(&pattern[range], "PORT_NUM");

    // Literal preset without select_text escapes all regex metacharacters.
    let literal_plain = Preset {
        name: "Literal".into(),
        pattern: "[ERR] req(1.2)+$a|b".into(),
        mode: SearchMode::Literal,
        highlight_match: true,
        select_text: None,
    };
    let (pattern, range) = literal_plain.regex_pattern_and_select_range();
    assert_eq!(pattern, regex::escape("[ERR] req(1.2)+$a|b"));
    assert_eq!(range, None);
    assert!(
        regex::Regex::new(&pattern)
            .unwrap()
            .is_match("[ERR] req(1.2)+$a|b")
    );

    // Literal preset with metacharacters before AND inside select_text maps offsets accurately.
    let literal_selectable = Preset {
        name: "LiteralHost".into(),
        pattern: "req[0] host=<HOST.NAME>+".into(),
        mode: SearchMode::Literal,
        highlight_match: false,
        select_text: Some("<HOST.NAME>".into()),
    };
    let (pattern, range) = literal_selectable.regex_pattern_and_select_range();
    assert_eq!(pattern, regex::escape("req[0] host=<HOST.NAME>+"));
    let range = range.expect("mapped literal select range");
    assert_eq!(&pattern[range], r"<HOST\.NAME>");

    // Unicode literal preset preserves UTF-8 boundaries when escaping.
    let literal_unicode = Preset {
        name: "Unicode".into(),
        pattern: "[שלום](ID).log".into(),
        mode: SearchMode::Literal,
        highlight_match: true,
        select_text: Some("ID".into()),
    };
    let (pattern, range) = literal_unicode.regex_pattern_and_select_range();
    assert_eq!(pattern, regex::escape("[שלום](ID).log"));
    let range = range.expect("unicode literal select range");
    assert_eq!(&pattern[range], "ID");
}

#[test]
fn append_preset_default_binding_and_migration() {
    use nox::config::{Command, KeyContext};

    let defaults = Config::load(None).unwrap();
    let has_binding = |cfg: &Config, keys: &str, context: KeyContext, action: Command| {
        cfg.keyboard
            .bindings
            .iter()
            .any(|b| b.keys == keys && b.context == context && b.action == action)
    };

    assert!(has_binding(
        &defaults,
        "secondary-enter",
        KeyContext::PresetsRegexMode,
        Command::AppendPreset
    ));

    // Migrating an older keyboard.json with Presets bindings adds secondary-enter in Presets && RegexMode.
    let upgrade_dir = tempdir().unwrap();
    fs::write(
        upgrade_dir.path().join("keyboard.json"),
        r#"{
  "$schema": "schemas/keyboard.schema.json",
  "bindings": [
    { "keys": "enter", "context": "Presets", "action": "apply_preset" }
  ]
}"#,
    )
    .unwrap();
    let report = config::migrate_config(upgrade_dir.path()).unwrap();
    assert!(report.added_bindings > 0);
    let upgraded = Config::load(Some(upgrade_dir.path())).unwrap();
    assert!(has_binding(
        &upgraded,
        "secondary-enter",
        KeyContext::PresetsRegexMode,
        Command::AppendPreset
    ));

    // Custom rebindings for append_preset are preserved without adding secondary-enter.
    let custom_dir = tempdir().unwrap();
    fs::write(
        custom_dir.path().join("keyboard.json"),
        r#"{
  "$schema": "schemas/keyboard.schema.json",
  "bindings": [
    { "keys": "alt-enter", "context": "Presets && RegexMode", "action": "append_preset" }
  ]
}"#,
    )
    .unwrap();
    config::migrate_config(custom_dir.path()).unwrap();
    let custom_migrated = Config::load(Some(custom_dir.path())).unwrap();
    assert!(has_binding(
        &custom_migrated,
        "alt-enter",
        KeyContext::PresetsRegexMode,
        Command::AppendPreset
    ));
    assert!(!has_binding(
        &custom_migrated,
        "secondary-enter",
        KeyContext::PresetsRegexMode,
        Command::AppendPreset
    ));
}
