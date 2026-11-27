//! Typed configuration and JSON schemas share a single source of truth.
use anyhow::{Context, Result, bail, ensure};
use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema, schema_for};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fs,
    io::Write,
    path::{Path, PathBuf},
};

pub const DEFAULT_FILTER_RESIZE_SENSITIVITY: f32 = 1.0;
pub const MIN_FILTER_RESIZE_SENSITIVITY: f32 = 0.1;
pub const MAX_FILTER_RESIZE_SENSITIVITY: f32 = 10.0;

const HEX_COLOR_PATTERN: &str = r"^#[0-9A-Fa-f]{6}$";
const MIN_FONT_SIZE: u16 = 8;
const MAX_FONT_SIZE: u16 = 32;
const JSON_SCHEMA_DRAFT: &str = "https://json-schema.org/draft/2020-12/schema";
const COMMON_SCHEMA_FILE: &str = "common.schema.json";
const SESSION_SCHEMA_FILE: &str = "session.schema.json";
const LOCAL_DEF_PREFIX: &str = "#/$defs/";

struct HexColor;

impl JsonSchema for HexColor {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("HexColor")
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "pattern": HEX_COLOR_PATTERN
        })
    }
}

struct FontSize;

impl JsonSchema for FontSize {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("FontSize")
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "number",
            "format": "float",
            "minimum": MIN_FONT_SIZE,
            "maximum": MAX_FONT_SIZE
        })
    }
}

fn default_filter_resize_sensitivity() -> f32 {
    DEFAULT_FILTER_RESIZE_SENSITIVITY
}

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
    #[serde(default = "default_filter_resize_sensitivity")]
    #[schemars(range(min = 0.1, max = 10.0))]
    pub filter_resize_sensitivity: f32,
    pub show_status_bar: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AppearanceConfig {
    pub ui_font_family: String,
    #[schemars(with = "FontSize")]
    pub ui_font_size: f32,
    pub font_family: String,
    #[schemars(with = "FontSize")]
    pub font_size: f32,
    #[schemars(with = "HexColor")]
    pub background: String,
    #[schemars(with = "HexColor")]
    pub panel: String,
    #[schemars(with = "HexColor")]
    pub foreground: String,
    #[schemars(with = "HexColor")]
    pub muted: String,
    #[schemars(with = "HexColor")]
    pub accent: String,
    #[schemars(with = "HexColor")]
    pub selection: String,
    #[schemars(with = "HexColor")]
    pub text_selection_color: String,
    #[schemars(range(min = 0.0, max = 1.0))]
    pub text_selection_opacity: f32,
    #[schemars(with = "HexColor")]
    pub border: String,
    #[schemars(with = "HexColor")]
    pub match_foreground: String,
    #[schemars(with = "HexColor")]
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub select_text: Option<String>,
}

impl Preset {
    /// Returns the UTF-8 byte range of the first occurrence of `select_text` within `pattern`
    /// when `select_text` is configured, non-empty, and contained in `pattern`.
    pub fn select_text_range(&self) -> Option<std::ops::Range<usize>> {
        let select_text = self.select_text.as_deref()?;
        if select_text.is_empty() {
            return None;
        }
        let start = self.pattern.find(select_text)?;
        Some(start..start + select_text.len())
    }

    /// Returns a non-fatal validation warning when `select_text` is specified but empty or not
    /// contained in `pattern`.
    pub fn select_text_warning(&self) -> Option<String> {
        let select_text = self.select_text.as_deref()?;
        if select_text.is_empty() {
            Some(format!("Preset {:?}: select_text is empty", self.name))
        } else if !self.pattern.contains(select_text) {
            Some(format!(
                "Preset {:?}: select_text {:?} is not contained in pattern {:?}",
                self.name, select_text, self.pattern
            ))
        } else {
            None
        }
    }

    /// Returns the pattern text ready to be embedded in a regex search (escaping regex
    /// metacharacters when `self.mode == SearchMode::Literal`) along with the mapped UTF-8
    /// byte range of `select_text` within that returned string.
    pub fn regex_pattern_and_select_range(&self) -> (String, Option<std::ops::Range<usize>>) {
        if self.mode == SearchMode::Regex {
            return (self.pattern.clone(), self.select_text_range());
        }
        if let Some(raw_range) = self.select_text_range() {
            let before = regex::escape(&self.pattern[..raw_range.start]);
            let selected = regex::escape(&self.pattern[raw_range.start..raw_range.end]);
            let after = regex::escape(&self.pattern[raw_range.end..]);
            let start = before.len();
            let end = start + selected.len();
            (format!("{before}{selected}{after}"), Some(start..end))
        } else {
            (regex::escape(&self.pattern), None)
        }
    }
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
    #[schemars(with = "HexColor")]
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
    RestoreClosedTab,
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
    ResetHorizontalScroll,
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
    AppendPreset,
    IncreaseFilterHeight,
    DecreaseFilterHeight,
    ZoomIn,
    ZoomOut,
    ZoomReset,
    MoveToLineStart,
    MoveToLineEnd,
    SelectToLineStart,
    SelectToLineEnd,
    DeleteToLineStart,
    DeleteToLineEnd,
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
    #[serde(rename = "Presets && RegexMode")]
    PresetsRegexMode,
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
            Self::PresetsRegexMode => "Presets && RegexMode",
        }
    }

    pub fn predicate(self) -> &'static str {
        match self {
            Self::TextSelectedRegexMode => "RegexMode > TextSelected",
            Self::PresetsRegexMode => "RegexMode > Presets",
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
            Self::TextSelectedRegexMode | Self::PresetsRegexMode => 2,
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
            w.filter_resize_sensitivity.is_finite()
                && (MIN_FILTER_RESIZE_SENSITIVITY..=MAX_FILTER_RESIZE_SENSITIVITY)
                    .contains(&w.filter_resize_sensitivity),
            "window.filter_resize_sensitivity must be 0.1..=10.0"
        );
        ensure!(
            (f32::from(MIN_FONT_SIZE)..=f32::from(MAX_FONT_SIZE)).contains(&a.ui_font_size),
            "appearance.ui_font_size must be {MIN_FONT_SIZE}..{MAX_FONT_SIZE}"
        );
        ensure!(
            (f32::from(MIN_FONT_SIZE)..=f32::from(MAX_FONT_SIZE)).contains(&a.font_size),
            "appearance.font_size must be {MIN_FONT_SIZE}..{MAX_FONT_SIZE}"
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

    /// Returns non-fatal configuration validation warnings (for example, search presets whose
    /// `select_text` is empty or does not appear in `pattern`).
    pub fn warnings(&self) -> Vec<String> {
        self.search
            .presets
            .iter()
            .filter_map(Preset::select_text_warning)
            .collect()
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

struct RawSchema {
    name: &'static str,
    is_config_section: bool,
    schema: Value,
}

struct SchemaBundle {
    sections: Vec<(&'static str, Value)>,
    session: Value,
    common: Value,
}

fn collect_local_def_refs(value: &Value, refs: &mut BTreeSet<String>) {
    match value {
        Value::Object(map) => {
            if let Some(def_name) = map
                .get("$ref")
                .and_then(Value::as_str)
                .and_then(|reference| reference.strip_prefix(LOCAL_DEF_PREFIX))
            {
                refs.insert(def_name.to_string());
            }
            for child in map.values() {
                collect_local_def_refs(child, refs);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_local_def_refs(item, refs);
            }
        }
        _ => {}
    }
}

fn rewrite_local_def_refs(value: &mut Value, common_defs: &HashSet<String>) {
    match value {
        Value::Object(map) => {
            if let Some(reference) = map.get_mut("$ref").and_then(|v| match v {
                Value::String(s) => Some(s),
                _ => None,
            }) && let Some(def_name) = reference.strip_prefix(LOCAL_DEF_PREFIX)
                && common_defs.contains(def_name)
            {
                *reference = format!("{COMMON_SCHEMA_FILE}{LOCAL_DEF_PREFIX}{def_name}");
            }
            for child in map.values_mut() {
                rewrite_local_def_refs(child, common_defs);
            }
        }
        Value::Array(items) => {
            for item in items {
                rewrite_local_def_refs(item, common_defs);
            }
        }
        _ => {}
    }
}

fn prune_unused_local_defs(schema: &mut Value) {
    let Some(root_obj) = schema.as_object() else {
        return;
    };
    let Some(defs) = root_obj.get("$defs").and_then(Value::as_object) else {
        return;
    };

    let mut reachable = BTreeSet::new();
    for (key, value) in root_obj {
        if key != "$defs" {
            collect_local_def_refs(value, &mut reachable);
        }
    }
    loop {
        let mut discovered = Vec::new();
        for name in &reachable {
            if let Some(def_val) = defs.get(name) {
                let mut nested = BTreeSet::new();
                collect_local_def_refs(def_val, &mut nested);
                for dep in nested {
                    if !reachable.contains(&dep) {
                        discovered.push(dep);
                    }
                }
            }
        }
        if discovered.is_empty() {
            break;
        }
        reachable.extend(discovered);
    }

    let root_obj = schema.as_object_mut().expect("root schema object");
    if let Some(defs) = root_obj.get_mut("$defs").and_then(Value::as_object_mut) {
        defs.retain(|name, _| reachable.contains(name));
        if defs.is_empty() {
            root_obj.remove("$defs");
        }
    }
}

impl SchemaBundle {
    fn generate() -> Result<Self> {
        let mut raw_schemas = vec![
            RawSchema {
                name: "window",
                is_config_section: true,
                schema: serde_json::to_value(schema_for!(WindowConfig))?,
            },
            RawSchema {
                name: "appearance",
                is_config_section: true,
                schema: serde_json::to_value(schema_for!(AppearanceConfig))?,
            },
            RawSchema {
                name: "viewer",
                is_config_section: true,
                schema: serde_json::to_value(schema_for!(ViewerConfig))?,
            },
            RawSchema {
                name: "search",
                is_config_section: true,
                schema: serde_json::to_value(schema_for!(SearchConfig))?,
            },
            RawSchema {
                name: "highlighting",
                is_config_section: true,
                schema: serde_json::to_value(schema_for!(HighlightConfig))?,
            },
            RawSchema {
                name: "files",
                is_config_section: true,
                schema: serde_json::to_value(schema_for!(FilesConfig))?,
            },
            RawSchema {
                name: "keyboard",
                is_config_section: true,
                schema: serde_json::to_value(schema_for!(KeyboardConfig))?,
            },
            RawSchema {
                name: "session",
                is_config_section: false,
                schema: serde_json::to_value(schema_for!(crate::session::Session))?,
            },
        ];

        // Step 1: Extract root object schemas that also appear as `$defs` in other schemas
        // (for example `SearchConfig`, shared between `search.schema.json` and `session.schema.json`).
        let mut root_titles: HashMap<String, usize> = HashMap::new();
        for (idx, raw) in raw_schemas.iter().enumerate() {
            if let Some(title) = raw.schema.get("title").and_then(Value::as_str)
                && raw.schema.get("type") == Some(&Value::String("object".into()))
                && raw.schema.get("properties").is_some_and(Value::is_object)
            {
                root_titles.insert(title.to_string(), idx);
            }
        }

        let mut shared_roots: BTreeMap<String, usize> = BTreeMap::new();
        for (idx, raw) in raw_schemas.iter().enumerate() {
            let Some(defs) = raw.schema.get("$defs").and_then(Value::as_object) else {
                continue;
            };
            for (def_name, def_val) in defs {
                if let Some(&root_idx) = root_titles.get(def_name)
                    && root_idx != idx
                {
                    let root_schema = &raw_schemas[root_idx].schema;
                    ensure!(
                        root_schema.get("type") == def_val.get("type")
                            && root_schema.get("properties") == def_val.get("properties"),
                        "Conflicting schema definitions for shared root type {def_name:?}"
                    );
                    shared_roots.insert(def_name.clone(), root_idx);
                }
            }
        }

        let mut common_defs: BTreeMap<String, Value> = BTreeMap::new();
        let mut direct_common_defs: HashSet<String> = HashSet::new();
        let mut shared_root_indices: HashSet<usize> = HashSet::new();

        for (def_name, &root_idx) in &shared_roots {
            let root_obj = raw_schemas[root_idx]
                .schema
                .as_object()
                .expect("root schema object");
            let mut base_def = serde_json::Map::new();
            base_def.insert("type".into(), Value::String("object".into()));
            base_def.insert(
                "properties".into(),
                root_obj
                    .get("properties")
                    .cloned()
                    .expect("shared root schema properties"),
            );
            common_defs.insert(def_name.clone(), Value::Object(base_def));

            let mut rewritten = serde_json::Map::new();
            if let Some(draft) = root_obj.get("$schema") {
                rewritten.insert("$schema".into(), draft.clone());
            }
            if let Some(title) = root_obj.get("title") {
                rewritten.insert("title".into(), title.clone());
            }
            rewritten.insert(
                "$ref".into(),
                Value::String(format!("{COMMON_SCHEMA_FILE}{LOCAL_DEF_PREFIX}{def_name}")),
            );
            if raw_schemas[root_idx].is_config_section {
                rewritten.insert(
                    "properties".into(),
                    serde_json::json!({ "$schema": { "type": "string" } }),
                );
                rewritten.insert("unevaluatedProperties".into(), Value::Bool(false));
            } else {
                if let Some(required) = root_obj.get("required") {
                    rewritten.insert("required".into(), required.clone());
                }
                if root_obj.get("additionalProperties") == Some(&Value::Bool(false)) {
                    rewritten.insert("unevaluatedProperties".into(), Value::Bool(false));
                }
            }
            if let Some(defs) = root_obj.get("$defs") {
                rewritten.insert("$defs".into(), defs.clone());
            }
            raw_schemas[root_idx].schema = Value::Object(rewritten);
            shared_root_indices.insert(root_idx);

            for (idx, raw) in raw_schemas.iter_mut().enumerate() {
                if idx == root_idx {
                    continue;
                }
                if let Some(defs) = raw.schema.get_mut("$defs").and_then(Value::as_object_mut)
                    && let Some(def_val) = defs.get_mut(def_name)
                {
                    let mut wrapper = serde_json::Map::new();
                    wrapper.insert(
                        "$ref".into(),
                        Value::String(format!("{COMMON_SCHEMA_FILE}{LOCAL_DEF_PREFIX}{def_name}")),
                    );
                    if let Some(required) = def_val.get("required") {
                        wrapper.insert("required".into(), required.clone());
                    }
                    if def_val.get("additionalProperties") == Some(&Value::Bool(false)) {
                        wrapper.insert("unevaluatedProperties".into(), Value::Bool(false));
                    }
                    *def_val = Value::Object(wrapper);
                }
            }
        }

        // Step 2: Move every `$defs` entry shared across multiple schemas — plus any `$defs`
        // transitively referenced by `common.schema.json` — into `common_defs`.
        let mut def_occurrences: BTreeMap<String, Vec<Value>> = BTreeMap::new();
        for raw in &raw_schemas {
            let Some(defs) = raw.schema.get("$defs").and_then(Value::as_object) else {
                continue;
            };
            for (def_name, def_val) in defs {
                if shared_roots.contains_key(def_name) {
                    continue;
                }
                def_occurrences
                    .entry(def_name.clone())
                    .or_default()
                    .push(def_val.clone());
            }
        }

        for (def_name, entries) in &def_occurrences {
            if entries.len() >= 2 {
                ensure!(
                    entries.iter().all(|value| value == &entries[0]),
                    "Conflicting definitions for shared schema {def_name:?}"
                );
                common_defs.insert(def_name.clone(), entries[0].clone());
                direct_common_defs.insert(def_name.clone());
            }
        }

        loop {
            let mut referenced = BTreeSet::new();
            for def_val in common_defs.values() {
                collect_local_def_refs(def_val, &mut referenced);
            }
            let mut added_any = false;
            for dep in referenced {
                if common_defs.contains_key(&dep) {
                    continue;
                }
                let entries = def_occurrences.get(&dep).with_context(|| {
                    format!("Common schema references unknown definition {dep:?}")
                })?;
                ensure!(
                    entries.iter().all(|value| value == &entries[0]),
                    "Conflicting definitions for shared schema {dep:?}"
                );
                common_defs.insert(dep.clone(), entries[0].clone());
                direct_common_defs.insert(dep);
                added_any = true;
            }
            if !added_any {
                break;
            }
        }

        // Step 3: Apply overlay adjustments to non-shared-root config section schemas.
        for (idx, raw) in raw_schemas.iter_mut().enumerate() {
            if raw.is_config_section && !shared_root_indices.contains(&idx) {
                raw.schema["properties"]["$schema"] = serde_json::json!({ "type": "string" });
                // Config files are overlays; any field may be omitted.
                raw.schema.as_object_mut().unwrap().remove("required");
            }
        }

        // Step 4: Rewrite references to common defs and drop extracted/unreferenced local defs.
        for raw in &mut raw_schemas {
            if let Some(defs) = raw.schema.get_mut("$defs").and_then(Value::as_object_mut) {
                for def_name in &direct_common_defs {
                    defs.remove(def_name);
                }
            }
            rewrite_local_def_refs(&mut raw.schema, &direct_common_defs);
            prune_unused_local_defs(&mut raw.schema);
        }

        // Step 5: Assemble `common.schema.json`.
        let mut common_defs_map = serde_json::Map::new();
        for (name, schema) in common_defs {
            common_defs_map.insert(name, schema);
        }
        let mut common_root = serde_json::Map::new();
        common_root.insert("$schema".into(), Value::String(JSON_SCHEMA_DRAFT.into()));
        common_root.insert("title".into(), Value::String("Common".into()));
        common_root.insert(
            "description".into(),
            Value::String(
                "Shared JSON Schema definitions referenced by Nox configuration and session schemas."
                    .into(),
            ),
        );
        common_root.insert("$defs".into(), Value::Object(common_defs_map));

        let session = raw_schemas.pop().expect("session schema").schema;
        let sections = raw_schemas
            .into_iter()
            .map(|raw| (raw.name, raw.schema))
            .collect();

        Ok(Self {
            sections,
            session,
            common: Value::Object(common_root),
        })
    }

    fn section(&self, name: &str) -> &Value {
        &self
            .sections
            .iter()
            .find(|(section_name, _)| *section_name == name)
            .expect("schema for default section")
            .1
    }

    fn files(&self) -> Vec<(String, &Value)> {
        let mut files: Vec<(String, &Value)> = self
            .sections
            .iter()
            .map(|(name, schema)| (format!("{name}.schema.json"), schema))
            .collect();
        files.push((SESSION_SCHEMA_FILE.to_string(), &self.session));
        files.push((COMMON_SCHEMA_FILE.to_string(), &self.common));
        files
    }

    fn write_to(&self, directory: &Path) -> Result<usize> {
        fs::create_dir_all(directory)?;
        let files = self.files();
        for (file_name, schema) in &files {
            let contents = format!("{}\n", serde_json::to_string_pretty(schema)?);
            write_file_atomic(&directory.join(file_name), contents.as_bytes())?;
        }
        Ok(files.len())
    }
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

fn resolve_schema_ref<'a>(
    mut schema: &'a Value,
    mut doc: &'a Value,
    common: &'a Value,
) -> (&'a Value, &'a Value) {
    for _ in 0..8 {
        let Some(reference) = schema.get("$ref").and_then(Value::as_str) else {
            break;
        };
        if let Some(def_name) = reference.strip_prefix(LOCAL_DEF_PREFIX) {
            let Some(target) = doc.get("$defs").and_then(|defs| defs.get(def_name)) else {
                break;
            };
            schema = target;
        } else if let Some(def_name) = reference
            .strip_prefix(COMMON_SCHEMA_FILE)
            .and_then(|rest| rest.strip_prefix(LOCAL_DEF_PREFIX))
        {
            let Some(target) = common.get("$defs").and_then(|defs| defs.get(def_name)) else {
                break;
            };
            schema = target;
            doc = common;
        } else {
            break;
        }
    }
    (schema, doc)
}

fn merge_array_item_defaults(
    existing: &mut Value,
    schema: &Value,
    doc: &Value,
    common: &Value,
) -> usize {
    let (resolved, resolved_doc) = resolve_schema_ref(schema, doc, common);
    let mut added = 0;
    match existing {
        Value::Object(map) => {
            let mut visited_props = HashSet::new();
            for (source, source_doc) in [(schema, doc), (resolved, resolved_doc)] {
                let Some(properties) = source.get("properties").and_then(Value::as_object) else {
                    continue;
                };
                for (prop_name, prop_schema) in properties {
                    if visited_props.insert(prop_name.as_str())
                        && let Some(prop_value) = map.get_mut(prop_name)
                    {
                        added +=
                            merge_array_item_defaults(prop_value, prop_schema, source_doc, common);
                    }
                }
            }
        }
        Value::Array(items) => {
            let Some(item_schema) = resolved.get("items") else {
                return 0;
            };
            let (resolved_item, item_doc) = resolve_schema_ref(item_schema, resolved_doc, common);
            let item_properties = resolved_item.get("properties").and_then(Value::as_object);
            for item in items {
                if let (Some(item_map), Some(props)) = (item.as_object_mut(), item_properties) {
                    for (field_name, field_schema) in props {
                        if !item_map.contains_key(field_name) {
                            let (resolved_field, _) =
                                resolve_schema_ref(field_schema, item_doc, common);
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
                added += merge_array_item_defaults(item, resolved_item, item_doc, common);
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

    // Index built-in default keys per `(context, action)` so an untouched subset of older
    // defaults (e.g. `Log` having `home`/`end` before `secondary-up`/`secondary-down` were
    // added) receives newly introduced companion shortcuts, while any user-customized
    // `(context, action)` rebinding is preserved untouched and no `(context, keys)` slot collides.
    let mut default_action_keys: HashMap<(String, String), HashSet<String>> = HashMap::new();
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
        default_action_keys
            .entry((context.to_string(), action.to_string()))
            .or_default()
            .insert(normalize_key_sequence(keys));
    }

    let mut customized_actions = HashSet::new();
    let mut used_keys = HashSet::new();
    for binding in existing_bindings.iter() {
        let Some(obj) = binding.as_object() else {
            continue;
        };
        let Some(context) = obj.get("context").and_then(Value::as_str) else {
            continue;
        };
        let normalized_keys = obj
            .get("keys")
            .and_then(Value::as_str)
            .map(normalize_key_sequence);
        if let Some(ref keys) = normalized_keys {
            used_keys.insert((context.to_string(), keys.clone()));
        }
        if let Some(action) = obj.get("action").and_then(Value::as_str) {
            let action_slot = (context.to_string(), action.to_string());
            let is_default_key = normalized_keys.as_ref().is_some_and(|keys| {
                default_action_keys
                    .get(&action_slot)
                    .is_some_and(|default_keys| default_keys.contains(keys))
            });
            if !is_default_key {
                customized_actions.insert(action_slot);
            }
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
        if customized_actions.contains(&action_slot) || used_keys.contains(&key_slot) {
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

    let schemas = SchemaBundle::generate()?;
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
        let schema = schemas.section(name);

        let mut fields_added = merge_missing_object_fields(&mut existing, &default_value);
        fields_added += merge_array_item_defaults(&mut existing, schema, schema, &schemas.common);
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
    report.schemas_updated = schemas.write_to(&schemas_dir)?;
    Ok(report)
}

pub fn write_defaults(directory: &Path) -> Result<()> {
    let schemas = SchemaBundle::generate()?;
    let schemas_dir = directory.join("schemas");
    for (file_name, _) in schemas.files() {
        ensure!(
            !schemas_dir.join(&file_name).exists(),
            "Refusing to overwrite existing {file_name}"
        );
    }
    for (name, _) in DEFAULTS {
        ensure!(
            !directory.join(format!("{name}.json")).exists(),
            "Refusing to overwrite existing {name}.json"
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
    schemas.write_to(&schemas_dir)?;
    Ok(())
}

pub fn write_schemas(directory: &Path) -> Result<()> {
    SchemaBundle::generate()?.write_to(directory)?;
    Ok(())
}
