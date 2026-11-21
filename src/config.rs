use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use anyhow::Context;
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

/// Determines where the target MiSTer games folder resides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum StorageMode {
    #[default]
    Local,
    Sftp,
}

/// Determines where `media/` folders are created relative to scanned games.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum MediaPlacement {
    /// `<games_root>/media/` (default expected by MiSTer ConsoleMode).
    #[default]
    RootMedia,
    /// `<game_parent_dir>/media/` inside each subfolder.
    PerSubfolderMedia,
}

/// Authentication method for SSH/SFTP connections to MiSTer FPGA.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum SftpAuthMode {
    #[default]
    Password,
    PrivateKey,
}

fn default_sftp_port() -> u16 {
    22
}

fn default_sftp_user() -> String {
    "root".to_string()
}

fn default_sftp_password() -> String {
    "1".to_string()
}

fn default_remote_path() -> String {
    "/media/fat/games".to_string()
}

/// Connection parameters for SFTP/SSH access to MiSTer FPGA.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SftpConfig {
    pub host: String,
    #[serde(default = "default_sftp_port")]
    pub port: u16,
    #[serde(default = "default_sftp_user")]
    pub username: String,
    pub auth_mode: SftpAuthMode,
    #[serde(skip, default = "default_sftp_password")]
    pub password: String,
    pub private_key_path: String,
    #[serde(skip)]
    pub passphrase: String,
    #[serde(default = "default_remote_path")]
    pub remote_path: String,
}

impl Default for SftpConfig {
    fn default() -> Self {
        Self {
            host: String::new(),
            port: default_sftp_port(),
            username: default_sftp_user(),
            auth_mode: SftpAuthMode::Password,
            password: default_sftp_password(),
            private_key_path: String::new(),
            passphrase: String::new(),
            remote_path: default_remote_path(),
        }
    }
}

fn default_threshold() -> f64 {
    0.78
}

fn default_true() -> bool {
    true
}

/// Options controlling cover matching, conversion, and syncing.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SyncOptions {
    #[serde(default = "default_threshold")]
    pub threshold: f64,
    #[serde(default = "default_true")]
    pub convert_to_png: bool,
    pub overwrite_existing: bool,
    #[serde(default = "default_true")]
    pub recursive_images: bool,
    pub media_placement: MediaPlacement,
    pub dry_run: bool,
    #[serde(default = "default_true")]
    pub strict_system_match: bool,
}

impl Default for SyncOptions {
    fn default() -> Self {
        Self {
            threshold: default_threshold(),
            convert_to_png: true,
            overwrite_existing: false,
            recursive_images: true,
            media_placement: MediaPlacement::RootMedia,
            dry_run: false,
            strict_system_match: true,
        }
    }
}

/// Persistent application settings stored in the user's config directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub storage_mode: StorageMode,
    pub local_games_dir: String,
    pub images_dir: String,
    pub sftp: SftpConfig,
    pub options: SyncOptions,
    pub trusted_host_keys: HashMap<String, String>,
    pub upload_local_games_base: String,
    #[serde(default = "default_remote_path")]
    pub upload_sftp_games_base: String,
    pub upload_overwrite_games: bool,
    pub upload_wrap_disc_in_folder: bool,
    pub upload_convert_to_chd: bool,
    pub upload_chdman_path: String,
    pub upload_temp_dir: String,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            storage_mode: StorageMode::default(),
            local_games_dir: String::new(),
            images_dir: String::new(),
            sftp: SftpConfig::default(),
            options: SyncOptions::default(),
            trusted_host_keys: HashMap::new(),
            upload_local_games_base: String::new(),
            upload_sftp_games_base: default_remote_path(),
            upload_overwrite_games: false,
            upload_wrap_disc_in_folder: false,
            upload_convert_to_chd: false,
            upload_chdman_path: String::new(),
            upload_temp_dir: String::new(),
        }
    }
}

impl AppConfig {
    /// Returns the platform-specific path to `config.json`.
    pub fn config_path() -> Option<PathBuf> {
        ProjectDirs::from("com", "mister", "cover-scraper")
            .map(|dirs| dirs.config_dir().join("config.json"))
    }

    /// Loads saved configuration from disk, falling back to defaults on any error.
    pub fn load() -> Self {
        let Some(path) = Self::config_path() else {
            return Self::default();
        };
        let Ok(contents) = fs::read_to_string(path) else {
            return Self::default();
        };
        serde_json::from_str(&contents).unwrap_or_default()
    }

    /// Saves configuration to disk, creating parent directories if needed.
    pub fn save(&self) -> anyhow::Result<()> {
        let Some(path) = Self::config_path() else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("No se pudo crear carpeta de config: {}", parent.display()))?;
        }
        let json = serde_json::to_string_pretty(self)?;
        fs::write(&path, json)
            .with_context(|| format!("No se pudo guardar config en: {}", path.display()))?;
        Ok(())
    }
}
