//! Typed configuration and JSON schemas share a single source of truth.
use anyhow::{Context, Result, bail, ensure};
use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::{collections::HashSet, fs, path::Path};

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
    Search,
    ToggleRegex,
    ToggleCaseSensitive,
    ToggleWholeWord,
    ToggleInvert,
    ToggleHighlightMatches,
    FocusSearch,
    FocusLog,
    ToggleFollow,
    ToggleBookmark,
    NextMatch,
    PreviousMatch,
    CopyLine,
    CopySelectedText,
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
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq, Hash)]
pub enum KeyContext {
    Workspace,
    Log,
    Selection,
    TextSelected,
    #[serde(rename = "TextSelected && RegexMode")]
    TextSelectedRegexMode,
    RegexMode,
    Search,
    GoTo,
    Presets,
}
impl KeyContext {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Workspace => "Workspace",
            Self::Log => "Log",
            Self::Selection => "Selection",
            Self::TextSelected => "TextSelected",
            Self::TextSelectedRegexMode => "TextSelected && RegexMode",
            Self::RegexMode => "RegexMode",
            Self::Search => "Search",
            Self::GoTo => "GoTo",
            Self::Presets => "Presets",
        }
    }

    pub fn predicate(self) -> &'static str {
        match self {
            Self::TextSelectedRegexMode => "RegexMode > TextSelected",
            other => other.as_str(),
        }
    }

    pub fn specificity(self) -> u8 {
        match self {
            Self::Workspace => 0,
            Self::RegexMode => 1,
            Self::Log | Self::Search | Self::GoTo | Self::Presets => 0,
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
        use std::io::Write;
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
pub fn write_schemas(directory: &Path) -> Result<()> {
    fs::create_dir_all(directory)?;
    macro_rules! emit { ($name:literal, $ty:ty) => {{
        let mut schema = serde_json::to_value(schema_for!($ty))?;
        schema["properties"]["$schema"] = serde_json::json!({"type":"string"});
        // Config files are overlays; any field may be omitted.
        schema.as_object_mut().unwrap().remove("required");
        fs::write(directory.join(concat!($name, ".schema.json")), format!("{}\n", serde_json::to_string_pretty(&schema)?))?;
    }}; }
    emit!("window", WindowConfig);
    emit!("appearance", AppearanceConfig);
    emit!("viewer", ViewerConfig);
    emit!("search", SearchConfig);
    emit!("highlighting", HighlightConfig);
    emit!("files", FilesConfig);
    emit!("keyboard", KeyboardConfig);
    let schema = schema_for!(crate::session::Session);
    fs::write(
        directory.join("session.schema.json"),
        format!("{}\n", serde_json::to_string_pretty(&schema)?),
    )?;
    Ok(())
}
