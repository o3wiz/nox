//! Session state is separate from user configuration; writes replace it atomically.
use crate::{
    config::{Config, FilesConfig, SearchConfig},
    zoom::{MAX_ZOOM_PERCENT, MIN_ZOOM_PERCENT},
};
use anyhow::{Context, Result, ensure};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SavedTab {
    pub path: PathBuf,
    pub query: String,
    pub options: SearchConfig,
    pub bookmarks: BTreeSet<usize>,
    pub selected: usize,
    pub follow: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Session {
    pub version: u32,
    pub active: usize,
    pub tabs: Vec<SavedTab>,
    pub recent: Vec<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub results_height: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub zoom_percent: Option<u16>,
}
impl Default for Session {
    fn default() -> Self {
        Self {
            version: 1,
            active: 0,
            tabs: Vec::new(),
            recent: Vec::new(),
            results_height: None,
            zoom_percent: None,
        }
    }
}
impl Session {
    pub fn load(path: &Path, config: &Config) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let state: Self = serde_json::from_slice(&fs::read(path)?)
            .with_context(|| format!("Invalid session: {}", path.display()))?;
        ensure!(
            state.version == 1,
            "Unsupported session version {}",
            state.version
        );
        if let Some(height) = state.results_height {
            ensure!(
                height.is_finite() && (0.0..=4320.0).contains(&height),
                "Invalid session results_height {height}"
            );
        }
        if let Some(zoom_percent) = state.zoom_percent {
            ensure!(
                (MIN_ZOOM_PERCENT..=MAX_ZOOM_PERCENT).contains(&zoom_percent),
                "Invalid session zoom_percent {zoom_percent}"
            );
        }
        for tab in &state.tabs {
            let mut config = config.clone();
            config.search = tab.options.clone();
            config.validate()?;
        }
        Ok(state)
    }
    pub fn save(&self, path: &Path) -> Result<()> {
        let parent = path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        fs::create_dir_all(parent)?;
        let mut temp = tempfile::NamedTempFile::new_in(parent)?;
        serde_json::to_writer_pretty(temp.as_file_mut(), self)?;
        temp.as_file().sync_all()?;
        temp.persist(path)?;
        Ok(())
    }
}
pub fn state_path(config: &FilesConfig) -> Option<PathBuf> {
    if let Some(path) = &config.state_file {
        return Some(PathBuf::from(path));
    }
    if let Some(path) = std::env::var_os("NOX_STATE_DIR") {
        return Some(PathBuf::from(path).join("session.json"));
    }
    #[cfg(target_os = "windows")]
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
    #[cfg(target_os = "macos")]
    let base = std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join("Library/Application Support"));
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")));
    base.map(|base| base.join("nox/session.json"))
}
