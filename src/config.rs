//! Typed configuration and JSON schemas share a single source of truth.
use anyhow::{Context, Result, bail, ensure};
use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::{
    collections::HashSet,
    fs,
    io::Write,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WindowConfig {
    pub title: String,
    #[schemars(range(min = 640, max = 7680))]
    pub width: f32,
    #[schemars(range(min = 480, max = 4320))]
    pub height: f32,
    pub maximized: bool,
    #[schemars(range(min = 100, max = 1200))]
    pub results_height: f32,
    pub show_status_bar: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AppearanceConfig {
    pub ui_font_family: String,
    #[schemars(range(min = 8, max = 32))]
    pub ui_font_size: f32,
    pub font_family: String,
    #[schemars(range(min = 8, max = 32))]
    pub font_size: f32,
    #[schemars(regex(pattern = r"^#[0-9A-Fa-f]{6}$"))]
    pub background: String,
    #[schemars(regex(pattern = r"^#[0-9A-Fa-f]{6}$"))]
    pub panel: String,
    #[schemars(regex(pattern = r"^#[0-9A-Fa-f]{6}$"))]
    pub foreground: String,
    #[schemars(regex(pattern = r"^#[0-9A-Fa-f]{6}$"))]
    pub muted: String,
    #[schemars(regex(pattern = r"^#[0-9A-Fa-f]{6}$"))]
    pub accent: String,
    #[schemars(regex(pattern = r"^#[0-9A-Fa-f]{6}$"))]
    pub selection: String,
    #[schemars(regex(pattern = r"^#[0-9A-Fa-f]{6}$"))]
    pub text_selection_color: String,
    #[schemars(range(min = 0.0, max = 1.0))]
    pub text_selection_opacity: f32,
    #[schemars(regex(pattern = r"^#[0-9A-Fa-f]{6}$"))]
    pub border: String,
    #[schemars(regex(pattern = r"^#[0-9A-Fa-f]{6}$"))]
    pub match_foreground: String,
    #[schemars(regex(pattern = r"^#[0-9A-Fa-f]{6}$"))]
    pub match_background: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ViewerConfig {
    pub line_numbers: bool,
    pub show_overview: bool,
    #[schemars(range(min = 8, max = 64))]
    pub overview_width: f32,
    #[schemars(range(min = 16, max = 64))]
    pub line_height: f32,
    #[schemars(range(min = 1, max = 16))]
    pub tab_width: usize,
    #[schemars(range(min = 256, max = 1000000))]
    pub max_display_chars: usize,
    pub follow: bool,
    pub include_bookmarks: bool,
    #[schemars(range(min = 100, max = 60000))]
    pub poll_interval_ms: u64,
    #[serde(default)]
    #[schemars(range(min = 0, max = 1000))]
    pub cursor_surrounding_lines: usize,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum SearchMode {
    Literal,
    Regex,
}
fn default_highlight_matches() -> bool {
    true
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Preset {
    pub name: String,
    pub pattern: String,
    pub mode: SearchMode,
    #[serde(default = "default_highlight_matches")]
    pub highlight_match: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SearchConfig {
    pub mode: SearchMode,
    pub case_sensitive: bool,
    pub whole_word: bool,
    pub invert: bool,
    #[serde(default = "default_highlight_matches")]
    pub highlight_matches: bool,
    #[schemars(range(min = 0, max = 100))]
    pub context_lines: usize,
    #[schemars(range(min = 1, max = 10000000))]
    pub max_results: usize,
    pub presets: Vec<Preset>,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HighlightRule {
    pub name: String,
    pub pattern: String,
    pub color: String,
    pub case_sensitive: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HighlightConfig {
    pub enabled: bool,
    pub rules: Vec<HighlightRule>,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FilesConfig {
    pub startup: Vec<String>,
    pub favorites: Vec<String>,
    pub restore_session: bool,
    pub save_session: bool,
    pub state_file: Option<String>,
    #[schemars(range(min = 0, max = 1000))]
    pub recent_limit: usize,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Command {
    Open,
    CloseTab,
    NextTab,
    PreviousTab,
    Search,
    ToggleRegex,
    ToggleCaseSensitive,
    ToggleWholeWord,
    ToggleInvert,
    ToggleHighlightMatches,
    FocusSearch,
    FocusFilePath,
    FocusLog,
    ReplaceLogPath,
    OpenPathInNewTab,
    CompleteFilePath,
    ToggleFollow,
    ToggleBookmark,
    NextMatch,
    PreviousMatch,
    CopyLine,
    CopySelectedText,
    CopyMatchResults,
    SearchSelectedText,
    AppendSelectedTextToSearch,
    Reload,
    GoToLine,
    ExportResults,
    Quit,
    NextLine,
    PreviousLine,
    FirstLine,
    LastLine,
    PageUp,
    PageDown,
    ScrollLeft,
    ScrollRight,
    NextBookmark,
    PreviousBookmark,
    ShowFiles,
    SwitchPane,
    SelectNextLine,
    SelectPreviousLine,
    SelectFirstLine,
    SelectLastLine,
    SelectPageUp,
    SelectPageDown,
    SelectAll,
    ClearSelection,
    ClearTextSelection,
    SwapSelectionAnchor,
    TogglePresets,
    ApplyPreset,
    IncreaseFilterHeight,
    DecreaseFilterHeight,
    ZoomIn,
    ZoomOut,
    ZoomReset,
    MoveToLineStart,
    MoveToLineEnd,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq, Hash)]
pub enum KeyContext {
    Workspace,
    Log,
    FilterLog,
    Selection,
    TextSelected,
    #[serde(rename = "TextSelected && RegexMode")]
    TextSelectedRegexMode,
    RegexMode,
    TextInput,
    Search,
    FilePath,
    GoTo,
    Presets,
}
impl KeyContext {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Workspace => "Workspace",
            Self::Log => "Log",
            Self::FilterLog => "FilterLog",
            Self::Selection => "Selection",
            Self::TextSelected => "TextSelected",
            Self::TextSelectedRegexMode => "TextSelected && RegexMode",
            Self::RegexMode => "RegexMode",
            Self::TextInput => "TextInput",
            Self::Search => "Search",
            Self::FilePath => "FilePath",
            Self::GoTo => "GoTo",
            Self::Presets => "Presets",
        }
    }

    pub fn predicate(self) -> &'static str {
        match self {
            Self::TextSelectedRegexMode => "RegexMode > TextSelected",
            Self::TextInput => "EditableText",
            other => other.as_str(),
        }
    }

    pub fn specificity(self) -> u8 {
        match self {
            Self::Workspace => 0,
            Self::RegexMode => 1,
            Self::Log | Self::TextInput => 0,
            Self::FilterLog => 1,
            Self::Search | Self::FilePath | Self::GoTo | Self::Presets => 1,
            Self::Selection | Self::TextSelected => 1,
            Self::TextSelectedRegexMode => 2,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Binding {
    pub keys: String,
    pub context: KeyContext,
    pub action: Command,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct KeyboardConfig {
    pub bindings: Vec<Binding>,
}
#[derive(Clone, Debug)]
pub struct Config {
    pub window: WindowConfig,
    pub appearance: AppearanceConfig,
    pub viewer: ViewerConfig,
    pub search: SearchConfig,
    pub highlighting: HighlightConfig,
    pub files: FilesConfig,
    pub keyboard: KeyboardConfig,
}

pub const DEFAULTS: &[(&str, &str)] = &[
    ("window", include_str!("../config/window.json")),
    ("appearance", include_str!("../config/appearance.json")),
    ("viewer", include_str!("../config/viewer.json")),
    ("search", include_str!("../config/search.json")),
    ("highlighting", include_str!("../config/highlighting.json")),
    ("files", include_str!("../config/files.json")),
    ("keyboard", include_str!("../config/keyboard.json")),
];
fn merge(base: &mut Value, patch: Value) {
    if let (Some(base), Some(patch)) = (base.as_object_mut(), patch.as_object()) {
        for (key, value) in patch {
            merge(base.entry(key).or_insert(Value::Null), value.clone());
        }
    } else {
        *base = patch;
    }
}
fn section<T: DeserializeOwned>(name: &str, directory: Option<&Path>) -> Result<T> {
    let mut value: Value =
        serde_json::from_str(DEFAULTS.iter().find(|(n, _)| *n == name).unwrap().1)?;
    value.as_object_mut().unwrap().remove("$schema");
    if let Some(directory) = directory {
        let path = directory.join(format!("{name}.json"));
        if path.exists() {
            let mut patch: Value = serde_json::from_str(&fs::read_to_string(&path)?)
                .with_context(|| format!("Invalid JSON: {}", path.display()))?;
            ensure!(
                patch.is_object(),
                "{} must contain an object",
                path.display()
            );
            if let Some(schema) = patch.get("$schema") {
                ensure!(schema.is_string(), "{name}.json: $schema must be a string");
            }
            patch.as_object_mut().unwrap().remove("$schema");
            merge(&mut value, patch);
        }
    }
    serde_json::from_value(value).with_context(|| format!("Invalid {name}.json"))
}
pub fn color(value: &str) -> Result<u32> {
    ensure!(
        value.len() == 7 && value.starts_with('#'),
        "Expected #RRGGBB color, got {value:?}"
    );
    u32::from_str_radix(&value[1..], 16).context("Invalid color")
}
impl Config {
    pub fn load(directory: Option<&Path>) -> Result<Self> {
        if let Some(directory) = directory {
            ensure!(
                directory.is_dir(),
                "Configuration directory does not exist: {}",
                directory.display()
            );
        }
        let config = Self {
            window: section("window", directory)?,
            appearance: section("appearance", directory)?,
            viewer: section("viewer", directory)?,
            search: section("search", directory)?,
            highlighting: section("highlighting", directory)?,
            files: section("files", directory)?,
            keyboard: section("keyboard", directory)?,
        };
        config.validate()?;
        Ok(config)
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.files.recent_limit <= 1000,
            "files.recent_limit must be 0..1000"
        );
        ensure!(
            (8.0..=64.0).contains(&self.viewer.overview_width),
            "viewer.overview_width must be 8..64"
        );
        let w = &self.window;
        let v = &self.viewer;
        let s = &self.search;
        let a = &self.appearance;
        ensure!(
            (640.0..=7680.0).contains(&w.width) && (480.0..=4320.0).contains(&w.height),
            "window: invalid dimensions"
        );
        ensure!(
            (100.0..=1200.0).contains(&w.results_height),
            "window.results_height must be 100..1200"
        );
        ensure!(
            (8.0..=32.0).contains(&a.ui_font_size),
            "appearance.ui_font_size must be 8..32"
        );
        ensure!(
            (8.0..=32.0).contains(&a.font_size),
            "appearance.font_size must be 8..32"
        );
        ensure!(
            (16.0..=64.0).contains(&v.line_height),
            "viewer.line_height must be 16..64"
        );
        ensure!(
            (1..=16).contains(&v.tab_width) && (256..=1000000).contains(&v.max_display_chars),
            "viewer: invalid tab width or display limit"
        );
        ensure!(
            (100..=60000).contains(&v.poll_interval_ms),
            "viewer.poll_interval_ms must be 100..60000"
        );
        ensure!(
            v.cursor_surrounding_lines <= 1000,
            "viewer.cursor_surrounding_lines must be 0..1000"
        );
        ensure!(
            s.context_lines <= 100 && (1..=10000000).contains(&s.max_results),
            "search: invalid context or result limit"
        );
        ensure!(
            a.text_selection_opacity.is_finite() && (0.0..=1.0).contains(&a.text_selection_opacity),
            "appearance.text_selection_opacity must be 0.0..=1.0"
        );
        for value in [
            &a.background,
            &a.panel,
            &a.foreground,
            &a.muted,
            &a.accent,
            &a.selection,
            &a.text_selection_color,
            &a.border,
            &a.match_foreground,
            &a.match_background,
        ] {
            color(value)?;
        }
        for rule in &self.highlighting.rules {
            color(&rule.color)?;
            regex::RegexBuilder::new(&rule.pattern)
                .case_insensitive(!rule.case_sensitive)
                .build()
                .with_context(|| format!("Invalid highlight: {}", rule.name))?;
        }
        for preset in &s.presets {
            if preset.mode == SearchMode::Regex {
                regex::Regex::new(&preset.pattern)
                    .with_context(|| format!("Invalid preset: {}", preset.name))?;
            }
        }
        let mut seen = HashSet::new();
        for binding in &self.keyboard.bindings {
            ensure!(
                !binding.keys.trim().is_empty(),
                "keyboard: empty key binding"
            );
            ensure!(
                seen.insert((binding.context, binding.keys.clone())),
                "keyboard: duplicate binding {} in {:?}",
                binding.keys,
                binding.context
            );
            if binding.keys.contains('\n') {
                bail!("keyboard: invalid key sequence");
            }
        }
        Ok(())
    }
}
/// Returns the default configuration directory: `$NOX_CONFIG_DIR` if set and non-empty,
/// otherwise `~/.config/nox` on macOS/Linux (respecting `$XDG_CONFIG_HOME` on Linux) or
/// `%APPDATA%/nox` on Windows.
pub fn default_config_dir() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("NOX_CONFIG_DIR").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(path));
    }
    #[cfg(target_os = "windows")]
    let base = std::env::var_os("APPDATA")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    #[cfg(target_os = "macos")]
    let base = std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(|home| PathBuf::from(home).join(".config"));
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|value| !value.is_empty())
                .map(|home| PathBuf::from(home).join(".config"))
        });
    base.map(|base| base.join("nox"))
}

/// Summary of changes applied by [`migrate_config`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MigrationReport {
    pub created_files: Vec<String>,
    pub updated_files: Vec<String>,
    pub added_fields: usize,
    pub added_bindings: usize,
    pub schemas_updated: usize,
}

impl MigrationReport {
    pub fn has_config_changes(&self) -> bool {
        !self.created_files.is_empty() || !self.updated_files.is_empty()
    }

    pub fn summary(&self, directory: &Path) -> String {
        if !self.has_config_changes() {
            return format!(
                "Configuration in {} is up to date (updated {} schemas)",
                directory.display(),
                self.schemas_updated
            );
        }
        let mut parts = Vec::new();
        if !self.created_files.is_empty() {
            parts.push(format!("created {} file(s)", self.created_files.len()));
        }
        if !self.updated_files.is_empty() {
            parts.push(format!("updated {} file(s)", self.updated_files.len()));
        }
        if self.added_fields > 0 {
            parts.push(format!("added {} field(s)", self.added_fields));
        }
        if self.added_bindings > 0 {
            parts.push(format!("added {} keybinding(s)", self.added_bindings));
        }
        parts.push(format!("updated {} schemas", self.schemas_updated));
        format!(
            "Migrated configuration in {} ({})",
            directory.display(),
            parts.join(", ")
        )
    }
}

fn write_file_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.as_file_mut().write_all(contents)?;
    temp.as_file().sync_all()?;
    temp.persist(path)
        .map_err(|err| err.error)
        .with_context(|| format!("Failed to write {}", path.display()))?;
    Ok(())
}

fn section_schema<T: JsonSchema>() -> Result<Value> {
    let mut schema = serde_json::to_value(schema_for!(T))?;
    schema["properties"]["$schema"] = serde_json::json!({ "type": "string" });
    // Config files are overlays; any field may be omitted.
    schema.as_object_mut().unwrap().remove("required");
    Ok(schema)
}

fn all_section_schemas() -> Result<Vec<(&'static str, Value)>> {
    Ok(vec![
        ("window", section_schema::<WindowConfig>()?),
        ("appearance", section_schema::<AppearanceConfig>()?),
        ("viewer", section_schema::<ViewerConfig>()?),
        ("search", section_schema::<SearchConfig>()?),
        ("highlighting", section_schema::<HighlightConfig>()?),
        ("files", section_schema::<FilesConfig>()?),
        ("keyboard", section_schema::<KeyboardConfig>()?),
    ])
}

fn merge_missing_object_fields(existing: &mut Value, defaults: &Value) -> usize {
    let (Some(existing_map), Some(default_map)) = (existing.as_object_mut(), defaults.as_object())
    else {
        return 0;
    };
    let mut added = 0;

    // Place a missing `$schema` link first while preserving existing key order.
    if let Some(default_schema) = default_map.get("$schema")
        && !existing_map.contains_key("$schema")
    {
        let previous = std::mem::take(existing_map);
        existing_map.insert("$schema".to_string(), default_schema.clone());
        for (key, value) in previous {
            existing_map.insert(key, value);
        }
        added += 1;
    }

    for (key, default_val) in default_map {
        if key == "$schema" {
            continue;
        }
        if let Some(existing_val) = existing_map.get_mut(key) {
            if existing_val.is_object() && default_val.is_object() {
                added += merge_missing_object_fields(existing_val, default_val);
            }
        } else {
            existing_map.insert(key.clone(), default_val.clone());
            added += 1;
        }
    }
    added
}

fn resolve_schema_ref<'a>(mut schema: &'a Value, root_schema: &'a Value) -> &'a Value {
    for _ in 0..8 {
        let Some(reference) = schema.get("$ref").and_then(Value::as_str) else {
            break;
        };
        let Some(def_name) = reference.strip_prefix("#/$defs/") else {
            break;
        };
        let Some(target) = root_schema.get("$defs").and_then(|defs| defs.get(def_name)) else {
            break;
        };
        schema = target;
    }
    schema
}

fn merge_array_item_defaults(existing: &mut Value, schema: &Value, root_schema: &Value) -> usize {
    let resolved = resolve_schema_ref(schema, root_schema);
    let mut added = 0;
    match existing {
        Value::Object(map) => {
            let Some(properties) = resolved.get("properties").and_then(Value::as_object) else {
                return 0;
            };
            for (prop_name, prop_schema) in properties {
                if let Some(prop_value) = map.get_mut(prop_name) {
                    added += merge_array_item_defaults(prop_value, prop_schema, root_schema);
                }
            }
        }
        Value::Array(items) => {
            let Some(item_schema) = resolved.get("items") else {
                return 0;
            };
            let resolved_item = resolve_schema_ref(item_schema, root_schema);
            let item_properties = resolved_item.get("properties").and_then(Value::as_object);
            for item in items {
                if let (Some(item_map), Some(props)) = (item.as_object_mut(), item_properties) {
                    for (field_name, field_schema) in props {
                        if !item_map.contains_key(field_name) {
                            let resolved_field = resolve_schema_ref(field_schema, root_schema);
                            if let Some(default_val) = field_schema
                                .get("default")
                                .or_else(|| resolved_field.get("default"))
                            {
                                item_map.insert(field_name.clone(), default_val.clone());
                                added += 1;
                            }
                        }
                    }
                }
                added += merge_array_item_defaults(item, resolved_item, root_schema);
            }
        }
        _ => {}
    }
    added
}

fn normalize_key_sequence(keys: &str) -> String {
    keys.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

fn merge_missing_keyboard_bindings(existing: &mut Value, defaults: &Value) -> usize {
    let Some(existing_bindings) = existing.get_mut("bindings").and_then(Value::as_array_mut) else {
        return 0;
    };
    let Some(default_bindings) = defaults.get("bindings").and_then(Value::as_array) else {
        return 0;
    };

    // Snapshot originally configured `(context, action)` pairs so that multiple default
    // bindings for the same `(context, action)` (such as FilePath `shift-tab` and `up`
    // for `previous_line`) are all added when that action is not yet configured in that
    // context, while never colliding with any already-used `(context, keys)` slot.
    let mut existing_actions = HashSet::new();
    let mut used_keys = HashSet::new();
    for binding in existing_bindings.iter() {
        let Some(obj) = binding.as_object() else {
            continue;
        };
        let Some(context) = obj.get("context").and_then(Value::as_str) else {
            continue;
        };
        if let Some(action) = obj.get("action").and_then(Value::as_str) {
            existing_actions.insert((context.to_string(), action.to_string()));
        }
        if let Some(keys) = obj.get("keys").and_then(Value::as_str) {
            used_keys.insert((context.to_string(), normalize_key_sequence(keys)));
        }
    }

    let mut added = 0;
    for default_binding in default_bindings {
        let Some(obj) = default_binding.as_object() else {
            continue;
        };
        let (Some(context), Some(action), Some(keys)) = (
            obj.get("context").and_then(Value::as_str),
            obj.get("action").and_then(Value::as_str),
            obj.get("keys").and_then(Value::as_str),
        ) else {
            continue;
        };
        let action_slot = (context.to_string(), action.to_string());
        let key_slot = (context.to_string(), normalize_key_sequence(keys));
        if existing_actions.contains(&action_slot) || used_keys.contains(&key_slot) {
            continue;
        }
        existing_bindings.push(default_binding.clone());
        used_keys.insert(key_slot);
        added += 1;
    }
    added
}

/// Migrates an existing configuration directory in place without deleting any existing or
/// deprecated fields.
///
/// Missing section files are created from built-in defaults, missing object fields
/// (including defaulted fields on existing array items such as `search.json` presets) and
/// missing default `keyboard.json` bindings are added, and all schema files under
/// `schemas/` are updated.
pub fn migrate_config(directory: &Path) -> Result<MigrationReport> {
    if !directory.exists() {
        bail!(
            "Configuration directory does not exist: {}",
            directory.display()
        );
    }
    ensure!(
        directory.is_dir(),
        "Configuration path is not a directory: {}",
        directory.display()
    );
    let schemas_dir = directory.join("schemas");
    ensure!(
        !schemas_dir.exists() || schemas_dir.is_dir(),
        "Schemas path is not a directory: {}",
        schemas_dir.display()
    );

    let schemas = all_section_schemas()?;
    let mut planned_writes: Vec<(PathBuf, String)> = Vec::new();
    let mut report = MigrationReport::default();

    for (name, contents) in DEFAULTS {
        let file_name = format!("{name}.json");
        let path = directory.join(&file_name);
        let normalized_default = contents.replace("../schemas/", "schemas/");

        if !path.exists() {
            planned_writes.push((path, normalized_default));
            report.created_files.push(file_name);
            continue;
        }

        let mut existing: Value = serde_json::from_str(&fs::read_to_string(&path)?)
            .with_context(|| format!("Invalid JSON: {}", path.display()))?;
        ensure!(
            existing.is_object(),
            "{} must contain an object",
            path.display()
        );

        let default_value: Value = serde_json::from_str(&normalized_default)?;
        let schema = &schemas
            .iter()
            .find(|(schema_name, _)| schema_name == name)
            .expect("schema for default section")
            .1;

        let mut fields_added = merge_missing_object_fields(&mut existing, &default_value);
        fields_added += merge_array_item_defaults(&mut existing, schema, schema);
        let bindings_added = if *name == "keyboard" {
            merge_missing_keyboard_bindings(&mut existing, &default_value)
        } else {
            0
        };

        if fields_added > 0 || bindings_added > 0 {
            let serialized = format!("{}\n", serde_json::to_string_pretty(&existing)?);
            planned_writes.push((path, serialized));
            report.updated_files.push(file_name);
            report.added_fields += fields_added;
            report.added_bindings += bindings_added;
        }
    }

    for (path, contents) in planned_writes {
        write_file_atomic(&path, contents.as_bytes())?;
    }
    report.schemas_updated = write_schemas_with_sections(&schemas_dir, &schemas)?;
    Ok(report)
}

pub fn write_defaults(directory: &Path) -> Result<()> {
    ensure!(
        !directory.join("schemas/session.schema.json").exists(),
        "Refusing to overwrite existing session.schema.json"
    );
    for (name, _) in DEFAULTS {
        ensure!(
            !directory.join(format!("{name}.json")).exists(),
            "Refusing to overwrite existing {name}.json"
        );
        ensure!(
            !directory
                .join("schemas")
                .join(format!("{name}.schema.json"))
                .exists(),
            "Refusing to overwrite existing {name}.schema.json"
        );
    }
    fs::create_dir_all(directory)?;
    for (name, contents) in DEFAULTS {
        let path = directory.join(format!("{name}.json"));
        // Never overwrite a user's configuration.
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .with_context(|| {
                format!(
                    "Cannot create {} (existing files are preserved)",
                    path.display()
                )
            })?;
        file.write_all(contents.replace("../schemas/", "schemas/").as_bytes())?;
    }
    write_schemas(&directory.join("schemas"))?;
    Ok(())
}

fn write_schemas_with_sections(directory: &Path, schemas: &[(&str, Value)]) -> Result<usize> {
    fs::create_dir_all(directory)?;
    for (name, schema) in schemas {
        let contents = format!("{}\n", serde_json::to_string_pretty(schema)?);
        write_file_atomic(
            &directory.join(format!("{name}.schema.json")),
            contents.as_bytes(),
        )?;
    }
    let session_schema = schema_for!(crate::session::Session);
    let session_contents = format!("{}\n", serde_json::to_string_pretty(&session_schema)?);
    write_file_atomic(
        &directory.join("session.schema.json"),
        session_contents.as_bytes(),
    )?;
    Ok(schemas.len() + 1)
}

pub fn write_schemas(directory: &Path) -> Result<()> {
    let schemas = all_section_schemas()?;
    write_schemas_with_sections(directory, &schemas)?;
    Ok(())
}
