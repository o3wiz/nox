//! Session state is separate from user configuration; writes replace it atomically.
use crate::config::{Config, FilesConfig, SearchConfig};
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
}
impl Default for Session {
    fn default() -> Self {
        Self {
            version: 1,
            active: 0,
            tabs: Vec::new(),
            recent: Vec::new(),
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
