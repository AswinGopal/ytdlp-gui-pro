use std::{
    fs,
    io::{self, BufReader},
    path::{Path, PathBuf},
    sync::{Mutex, PoisonError},
};

use anyhow::ensure;
use serde::{Deserialize, Serialize};

use crate::types::{Preset, Result};

/// What the per-row quality dropdown offers. Persisted from the settings
/// (gear) panel; the frontend applies it when rendering a row.
///
/// The format list itself cannot be switched off — only narrowed by the
/// container, codec and minimum-resolution filters — so the dropdown always
/// has something to pick unless those filters exclude every format of a video.
///
/// Missing keys fall back to `Default`, and keys written by older builds
/// (`show_combined`, `max_height`, …) are ignored, so an old settings file
/// keeps loading.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct QualityMenu {
    pub show_mp3: bool,
    pub show_m4a: bool,
    pub show_1080p: bool,
    pub show_best: bool,
    pub show_chapters: bool,
    pub show_timeline: bool,
    /// Lower-case container extensions to hide (`"webm"`, `"m4a"`, …).
    pub hidden_containers: Vec<String>,
    /// Lower-case codec family names to hide, as the frontend labels them.
    pub hidden_codecs: Vec<String>,
    /// Hide video formats shorter than this many pixels. Formats whose height
    /// is unknown are never hidden by it.
    pub min_height: Option<u32>,
}

impl Default for QualityMenu {
    fn default() -> Self {
        Self {
            show_mp3: true,
            show_m4a: true,
            show_1080p: true,
            show_best: true,
            show_chapters: true,
            show_timeline: true,
            hidden_containers: Vec::new(),
            hidden_codecs: Vec::new(),
            min_height: None,
        }
    }
}

impl QualityMenu {
    const MAX_FILTER_ENTRIES: usize = 64;
    const MAX_FILTER_LEN: usize = 32;

    /// Rejects a zero minimum resolution and returns the menu with its filter
    /// lists normalised (trimmed, lower-cased, de-duplicated, bounded).
    pub fn validated(mut self) -> Result<Self> {
        ensure!(self.min_height != Some(0), "Minimum resolution must be greater than zero.");

        for list in [&mut self.hidden_containers, &mut self.hidden_codecs] {
            let mut cleaned: Vec<String> = list
                .iter()
                .map(|s| s.trim().to_lowercase())
                .filter(|s| !s.is_empty() && s.len() <= Self::MAX_FILTER_LEN)
                .collect();
            cleaned.sort();
            cleaned.dedup();
            cleaned.truncate(Self::MAX_FILTER_ENTRIES);
            *list = cleaned;
        }
        Ok(self)
    }
}

#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
struct Settings {
    download_dir: Option<String>,
    last_preset: Option<Preset>,
    quality_menu: QualityMenu,
}

/// Directory holding every file the app persists (settings, database).
pub(crate) fn data_dir() -> PathBuf {
    let mut path = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
    path.push("ytdownloader");
    let _ = fs::create_dir_all(&path);
    path
}

fn settings_path() -> PathBuf {
    data_dir().join("settings.json")
}

fn load() -> Settings {
    fs::File::open(settings_path())
        .ok()
        .and_then(|f| serde_json::from_reader(BufReader::new(f)).ok())
        .unwrap_or_default()
}

/// Writes via a temporary file and a rename, so a crash mid-write can never
/// leave a truncated `settings.json` that `load` would silently discard.
fn save(s: &Settings) -> io::Result<()> {
    let path = settings_path();
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(s)?)?;
    fs::rename(&tmp, &path)
}

static WRITE_LOCK: Mutex<()> = Mutex::new(());

/// Read-modify-write under a lock, so concurrent setters cannot lose updates.
fn update(change: impl FnOnce(&mut Settings)) -> io::Result<()> {
    let _guard = WRITE_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    let mut s = load();
    change(&mut s);
    save(&s)
}

/// Returns the configured download directory, falling back to the system
/// Downloads folder, then the current working directory.
pub fn get_download_dir() -> PathBuf {
    let s = load();
    if let Some(ref p) = s.download_dir {
        let path = PathBuf::from(p);
        if path.is_dir() {
            return path;
        }
    }
    dirs::download_dir().unwrap_or_else(|| PathBuf::from("."))
}

/// Persists `path` as the common download directory.
pub fn set_download_dir(path: &Path) -> io::Result<()> {
    update(|s| s.download_dir = Some(path.to_string_lossy().to_string()))
}

/// Returns the raw stored value (for display), or the resolved fallback.
pub fn get_download_dir_display() -> String {
    get_download_dir().to_string_lossy().to_string()
}

/// Removes any stored override so the fallback (system Downloads) is used.
pub fn reset_download_dir() -> io::Result<()> {
    update(|s| s.download_dir = None)
}

/// The preset new rows start with: the last one the user picked, or `Best`.
pub fn get_last_preset() -> Preset {
    load().last_preset.unwrap_or(Preset::Best)
}

pub fn set_last_preset(p: Preset) -> io::Result<()> {
    update(|s| s.last_preset = Some(p))
}

pub fn get_quality_menu() -> QualityMenu {
    load().quality_menu
}

/// Validates, persists and returns the normalised menu.
pub fn set_quality_menu(menu: QualityMenu) -> Result<QualityMenu> {
    let menu = menu.validated()?;
    let stored = menu.clone();
    update(move |s| s.quality_menu = stored)?;
    Ok(menu)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_preset_and_range_entry_can_be_hidden() {
        let menu = QualityMenu {
            show_mp3: false,
            show_m4a: false,
            show_1080p: false,
            show_best: false,
            show_chapters: false,
            show_timeline: false,
            ..QualityMenu::default()
        };
        assert!(menu.validated().is_ok());
    }

    #[test]
    fn menu_filters_are_normalised() {
        let menu = QualityMenu {
            hidden_containers: vec![" WebM ".into(), "webm".into(), "".into(), "x".repeat(99)],
            min_height: Some(720),
            ..QualityMenu::default()
        }
        .validated()
        .unwrap();
        assert_eq!(menu.hidden_containers, ["webm"]);
        assert_eq!(menu.min_height, Some(720));
        assert!(QualityMenu { min_height: Some(0), ..QualityMenu::default() }.validated().is_err());
    }

    #[test]
    fn old_settings_files_keep_loading() {
        let s: Settings = serde_json::from_str(r#"{"download_dir":"/tmp/x"}"#).unwrap();
        assert_eq!(s.download_dir.as_deref(), Some("/tmp/x"));
        assert_eq!(s.quality_menu, QualityMenu::default());
        assert!(s.last_preset.is_none());
    }

    #[test]
    fn keys_from_older_builds_are_ignored() {
        let s: Settings = serde_json::from_str(
            r#"{"quality_menu":{"show_audio_only":false,"show_video_only":false,"max_height":480,"show_mp3":false}}"#,
        )
        .unwrap();
        assert!(!s.quality_menu.show_mp3);
        assert_eq!(s.quality_menu.min_height, None);
    }
}
