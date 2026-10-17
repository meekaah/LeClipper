use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DefaultAction {
    /// Show the popup and wait for the user.
    Ask,
    SaveMp4,
    SaveGif,
    Upload,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UploadFormat {
    Mp4,
    Gif,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// e.g. "Ctrl+Shift+R"; parsed by global-hotkey.
    pub hotkey: String,
    pub capture_cursor: bool,
    pub output_dir: PathBuf,

    pub video_fps: u32,
    /// 1..=100, mapped to bits-per-pixel.
    pub video_quality: u32,

    pub gif_fps: u32,
    /// 0 = keep the recorded width.
    pub gif_max_width: u32,
    /// gifski quality, 1..=100.
    pub gif_quality: u8,

    pub default_action: DefaultAction,

    pub upload_format: UploadFormat,
    /// Imgur album id (not the hash shown in URLs of others' albums, the id of your own album).
    pub imgur_album_id: Option<String>,
    pub imgur_album_title: Option<String>,
    /// Copy `https://i.imgur.com/<id>.mp4` instead of `https://imgur.com/<id>`.
    pub copy_direct_link: bool,
    pub auto_copy_link: bool,
}

impl Default for Config {
    fn default() -> Self {
        let output_dir = directories::UserDirs::new()
            .and_then(|u| u.video_dir().map(|p| p.to_path_buf()))
            .unwrap_or_else(|| PathBuf::from("."))
            .join("screen2imgur");
        Self {
            hotkey: "Ctrl+Shift+R".into(),
            capture_cursor: true,
            output_dir,
            video_fps: 60,
            video_quality: 70,
            gif_fps: 30,
            gif_max_width: 960,
            gif_quality: 90,
            default_action: DefaultAction::Ask,
            upload_format: UploadFormat::Mp4,
            imgur_album_id: None,
            imgur_album_title: None,
            copy_direct_link: false,
            auto_copy_link: true,
        }
    }
}

pub fn app_dir() -> PathBuf {
    directories::BaseDirs::new()
        .map(|b| b.config_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."))
        .join("screen2imgur")
}

pub fn config_path() -> PathBuf {
    app_dir().join("config.toml")
}

pub fn temp_dir() -> PathBuf {
    std::env::temp_dir().join("screen2imgur")
}

impl Config {
    pub fn load() -> Self {
        match std::fs::read_to_string(config_path()) {
            Ok(s) => toml::from_str(&s).unwrap_or_else(|e| {
                crate::log(&format!("config parse error, using defaults: {e}"));
                Self::default()
            }),
            Err(_) => Self::default(),
        }
        .sanitized()
    }

    pub fn save(&self) -> Result<()> {
        std::fs::create_dir_all(app_dir())?;
        let s = toml::to_string_pretty(self)?;
        std::fs::write(config_path(), s).context("writing config.toml")
    }

    fn sanitized(mut self) -> Self {
        self.video_fps = self.video_fps.clamp(1, 120);
        self.video_quality = self.video_quality.clamp(1, 100);
        self.gif_fps = self.gif_fps.clamp(1, 50);
        self.gif_quality = self.gif_quality.clamp(1, 100);
        self
    }

    /// Target H.264 bitrate for a given frame size.
    pub fn video_bitrate(&self, width: u32, height: u32) -> u32 {
        let bpp = 0.02 + self.video_quality as f64 / 100.0 * 0.23;
        let bits = width as f64 * height as f64 * self.video_fps as f64 * bpp;
        bits.clamp(1_000_000.0, 120_000_000.0) as u32
    }
}
