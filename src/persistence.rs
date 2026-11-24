//! `~/.config/RustRadioGTK/saved_data.json` — favorites, history, playlists
//! and preferences. Field names are kept compatible with older versions.

use crate::model::Station;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

fn default_bitrate_str() -> String {
    "None".to_string()
}
fn default_audio_sink() -> String {
    "autoaudiosink".to_string()
}
fn default_analyzer_type() -> String {
    "BlocksWithHold".to_string()
}
fn default_volume() -> f64 {
    1.0
}
fn default_true() -> bool {
    true
}
fn default_width() -> i32 {
    1280
}
fn default_height() -> i32 {
    800
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct SavedData {
    #[serde(default)]
    pub favorites: Vec<Station>,
    #[serde(default)]
    pub custom_tags: Vec<String>,
    #[serde(default = "default_bitrate_str")]
    pub active_bitrate: String,
    #[serde(default)]
    pub recent_stations: Vec<Station>,
    #[serde(default)]
    pub last_sync_timestamp: String,
    #[serde(default)]
    pub playlists: Vec<Station>,
    #[serde(default = "default_audio_sink")]
    pub audio_sink: String,
    #[serde(default)]
    pub disable_video_streams: bool,
    #[serde(default)]
    pub disable_audio_buffer: bool,
    #[serde(default = "default_analyzer_type")]
    pub analyzer_type: String,
    #[serde(default = "default_true")]
    pub analyzer_enabled: bool,
    #[serde(default = "default_volume")]
    pub volume: f64,
    #[serde(default)]
    pub last_stations_loaded: usize,
    #[serde(default = "default_true")]
    pub prefer_high_bitrate: bool,
    #[serde(default = "default_true")]
    pub hide_broken: bool,
    #[serde(default = "default_true")]
    pub auto_update: bool,
    #[serde(default)]
    pub allow_local_streams: bool,
    #[serde(default = "default_width")]
    pub window_width: i32,
    #[serde(default = "default_height")]
    pub window_height: i32,
    #[serde(default)]
    pub window_maximized: bool,
}

impl Default for SavedData {
    fn default() -> Self {
        serde_json::from_str("{}").expect("defaults deserialize")
    }
}

fn config_path() -> PathBuf {
    let mut path = gtk::glib::user_config_dir();
    path.push("RustRadioGTK");
    path.push("saved_data.json");
    path
}

pub fn load() -> SavedData {
    let path = config_path();
    match std::fs::read_to_string(&path) {
        Ok(text) => match serde_json::from_str(&text) {
            Ok(data) => data,
            Err(err) => {
                eprintln!("Saved data is invalid: {}", err);
                let _ = std::fs::copy(&path, path.with_file_name("saved_data.json.corrupt"));
                SavedData::default()
            }
        },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => SavedData::default(),
        Err(err) => {
            eprintln!("Failed to read saved data: {}", err);
            SavedData::default()
        }
    }
}

/// Monotonic sequence so an older snapshot written by a slow background
/// thread can never overwrite a newer one.
static SAVE_SEQ: AtomicU64 = AtomicU64::new(0);
static LAST_WRITTEN: Mutex<u64> = Mutex::new(0);

fn write(seq: u64, data: &SavedData) {
    let mut last_written = LAST_WRITTEN.lock().unwrap_or_else(|p| p.into_inner());
    if seq <= *last_written {
        return;
    }
    let path = config_path();
    if let Some(dir) = path.parent() {
        if let Err(err) = std::fs::create_dir_all(dir) {
            eprintln!("Failed to create configuration directory: {}", err);
            return;
        }
    }
    let json = match serde_json::to_vec(data) {
        Ok(json) => json,
        Err(err) => {
            eprintln!("Failed to serialize saved data: {}", err);
            return;
        }
    };
    // Keep the previous file intact until the complete replacement is ready.
    let temp_path = path.with_file_name("saved_data.json.tmp");
    let result = (|| -> std::io::Result<()> {
        let mut file = std::fs::File::create(&temp_path)?;
        file.write_all(&json)?;
        file.sync_all()?;
        std::fs::rename(&temp_path, &path)
    })();
    match result {
        Ok(()) => *last_written = seq,
        Err(err) => {
            eprintln!("Failed to save application data: {}", err);
            let _ = std::fs::remove_file(temp_path);
        }
    }
}

/// Write on a background thread so the fsync never blocks the UI.
pub fn save_in_background(data: SavedData) {
    let seq = SAVE_SEQ.fetch_add(1, Ordering::AcqRel) + 1;
    let spawned = std::thread::Builder::new()
        .name("save-data".into())
        .spawn(move || write(seq, &data));
    if let Err(err) = spawned {
        eprintln!("Failed to start background save: {}", err);
    }
}

/// Write synchronously (application shutdown).
pub fn save_now(data: &SavedData) {
    let seq = SAVE_SEQ.fetch_add(1, Ordering::AcqRel) + 1;
    write(seq, data);
}

/// Backup file format (Preferences → Data Management).
#[derive(Serialize, Deserialize, Default)]
pub struct BackupPayload {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub home_tags: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub favourites: Option<Vec<Station>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub playlists: Option<Vec<Station>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_tags: Option<Vec<String>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_files_load_with_new_defaults() {
        let old = r#"{"favorites":[],"custom_tags":["rock"],"volume":0.5}"#;
        let data: SavedData = serde_json::from_str(old).unwrap();
        assert_eq!(data.custom_tags, ["rock"]);
        assert_eq!(data.volume, 0.5);
        assert!(data.hide_broken && data.auto_update && data.prefer_high_bitrate);
        assert!(data.analyzer_enabled);
        assert!(!data.allow_local_streams);
        assert_eq!((data.window_width, data.window_height), (1280, 800));
        assert_eq!(SavedData::default().audio_sink, "autoaudiosink");
    }

    #[test]
    fn analyzer_switch_persists() {
        let data: SavedData = serde_json::from_str(r#"{"analyzer_enabled":false}"#).unwrap();
        assert!(!data.analyzer_enabled);
        let json = serde_json::to_string(&data).unwrap();
        assert!(json.contains(r#""analyzer_enabled":false"#));
    }
}
