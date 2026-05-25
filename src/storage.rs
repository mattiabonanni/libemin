use std::{fs, path::PathBuf};

use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedSettings {
    pub base_url: String,
    pub language: String,
    pub username: String,
    pub insert_form_url_override: String,
}

impl Default for SavedSettings {
    fn default() -> Self {
        Self {
            base_url: String::new(),
            language: "it".to_owned(),
            username: String::new(),
            insert_form_url_override: String::new(),
        }
    }
}

pub fn load_settings() -> SavedSettings {
    let Ok(path) = settings_path() else {
        return SavedSettings::default();
    };

    let Ok(contents) = fs::read_to_string(path) else {
        return SavedSettings::default();
    };

    serde_json::from_str(&contents).unwrap_or_default()
}

pub fn save_settings(settings: &SavedSettings) -> Result<()> {
    let path = settings_path()?;

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create settings directory {}", parent.display()))?;
    }

    let data = serde_json::to_vec_pretty(settings)?;
    fs::write(&path, data)
        .with_context(|| format!("failed to write settings file {}", path.display()))?;

    Ok(())
}

pub fn has_cookie_store() -> bool {
    cookie_store_path()
        .map(|path| path.exists())
        .unwrap_or(false)
}

pub fn delete_cookie_store() -> Result<()> {
    let path = cookie_store_path()?;
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("failed to delete cookie store {}", path.display()))
        }
    }
}

fn settings_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("settings.json"))
}

pub fn cookie_store_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("cookies.json"))
}

fn config_dir() -> Result<PathBuf> {
    let dirs = ProjectDirs::from("com", "libemin", "libemin")
        .context("failed to resolve an app configuration directory")?;

    Ok(dirs.config_dir().to_path_buf())
}
