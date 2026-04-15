use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_media_dir")]
    pub media_dir: String,
    #[serde(default = "default_thumbnail_width")]
    pub thumbnail_width: u32,
    #[serde(default = "default_columns")]
    pub gallery_columns: usize,
}

fn default_media_dir() -> String {
    "media".to_string()
}

fn default_thumbnail_width() -> u32 {
    320
}

fn default_columns() -> usize {
    4
}

impl Default for Config {
    fn default() -> Self {
        Self {
            media_dir: default_media_dir(),
            thumbnail_width: default_thumbnail_width(),
            gallery_columns: default_columns(),
        }
    }
}

/// Returns the base directory where the executable is located.
pub fn base_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Returns the path to the appdata directory.
pub fn appdata_dir() -> PathBuf {
    base_dir().join("appdata")
}

/// Returns the path to the media directory based on config.
pub fn media_dir(config: &Config) -> PathBuf {
    let media = Path::new(&config.media_dir);
    if media.is_absolute() {
        media.to_path_buf()
    } else {
        base_dir().join(media)
    }
}

/// Returns the path to the thumbnails directory.
pub fn thumbnails_dir() -> PathBuf {
    appdata_dir().join("thumbnails")
}

/// Returns the path to the SQLite database file.
pub fn db_path() -> PathBuf {
    appdata_dir().join("micromedia.db")
}

/// Returns the path to the config file.
pub fn config_path() -> PathBuf {
    appdata_dir().join("config.toml")
}

/// Ensures all required directories exist.
pub fn ensure_dirs(config: &Config) {
    let dirs = [appdata_dir(), media_dir(config), thumbnails_dir()];
    for dir in &dirs {
        if !dir.exists() {
            fs::create_dir_all(dir).ok();
        }
    }
}

/// Loads config from disk, or creates a default one.
pub fn load_config() -> Config {
    let path = config_path();
    if path.exists() {
        let content = fs::read_to_string(&path).unwrap_or_default();
        toml::from_str(&content).unwrap_or_default()
    } else {
        let config = Config::default();
        save_config(&config);
        config
    }
}

/// Saves config to disk.
pub fn save_config(config: &Config) {
    // Ensure appdata dir exists before writing config
    let appdata = appdata_dir();
    if !appdata.exists() {
        fs::create_dir_all(&appdata).ok();
    }
    if let Ok(content) = toml::to_string_pretty(config) {
        fs::write(config_path(), content).ok();
    }
}
