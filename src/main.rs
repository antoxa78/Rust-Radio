use adw::prelude::*;
use adw::{ActionRow, ApplicationWindow, ComboRow, HeaderBar, PreferencesGroup, PreferencesPage};
use dns_lookup::lookup_addr;
use gstreamer::prelude::ObjectExt as GstObjectExt;
use gstreamer::prelude::*;
use gtk::prelude::Cast;
use gtk::{
    Application, Box, Button, DrawingArea, Label, ListBox, MenuButton, Orientation, Paned, Popover,
    Scale, ScrolledWindow, SearchEntry, StringList, Switch,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::HashSet;
use std::io::Write;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

mod station_cache;

fn make_stats_label() -> Rc<RefCell<Option<gtk::Label>>> {
    Rc::new(RefCell::new(None))
}

static RENDER_GEN: AtomicUsize = AtomicUsize::new(0);
static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);
static ACTIVE_REQUEST_ID: AtomicU64 = AtomicU64::new(0);
static REQUEST_CANCEL: OnceLock<tokio::sync::watch::Sender<u64>> = OnceLock::new();
static SHUTTING_DOWN: AtomicBool = AtomicBool::new(false);
static HTTP_CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
static HTTP_RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
/// Discovered radio-browser API host, shared by every request. A
/// `tokio::sync::OnceCell` lets concurrent first requests wait on a single
/// discovery instead of each starting their own.
static RADIO_BROWSER_HOST: tokio::sync::OnceCell<String> = tokio::sync::OnceCell::const_new();
/// Monotonic sequence for saved-data snapshots, so an older snapshot written
/// by a background thread can never overwrite a newer one.
static SAVE_SEQ: AtomicU64 = AtomicU64::new(0);
/// Serialises saved-data writes; holds the sequence of the last snapshot written.
static SAVE_WRITE_LOCK: Mutex<u64> = Mutex::new(0);

const INITIAL_STATION_LIMIT: usize = 250;
/// Rows rendered in the first idle tick, enough to fill the visible window
/// so the list appears immediately.
const FIRST_RENDER_BATCH: usize = 30;
/// Rows rendered per subsequent idle tick.
const RENDER_BATCH: usize = 40;
const STATION_PAGE_SIZE: usize = 50;
const MAX_STATION_LIMIT: usize = 5_000;
const MAX_API_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
const MAX_PLAYLIST_BYTES: u64 = 5 * 1024 * 1024;
const MAX_PLAYLIST_STATIONS: usize = 5_000;
const MAX_BACKUP_BYTES: u64 = 10 * 1024 * 1024;

fn begin_request() -> u64 {
    let request_id = NEXT_REQUEST_ID.fetch_add(1, Ordering::AcqRel);
    ACTIVE_REQUEST_ID.store(request_id, Ordering::Release);
    RENDER_GEN.fetch_add(1, Ordering::AcqRel);
    if let Some(cancel) = REQUEST_CANCEL.get() {
        cancel.send_replace(request_id);
    }
    request_id
}

fn request_cancel_receiver(request_id: u64) -> tokio::sync::watch::Receiver<u64> {
    REQUEST_CANCEL
        .get_or_init(|| {
            let (sender, _) = tokio::sync::watch::channel(request_id);
            sender
        })
        .subscribe()
}

fn is_active_request(request_id: u64) -> bool {
    ACTIVE_REQUEST_ID.load(Ordering::Acquire) == request_id
}

/// Find the start of the value for query parameter `key` (exact name match).
fn url_param_value_start(url: &str, key: &str) -> Option<usize> {
    let query_start = url.find('?')? + 1;
    let mut pos = query_start;
    for pair in url[query_start..].split('&') {
        if let Some(rest) = pair.strip_prefix(key) {
            if rest.starts_with('=') {
                return Some(pos + key.len() + 1);
            }
        }
        pos += pair.len() + 1;
    }
    None
}

fn parse_url_usize_param(url: &str, key: &str) -> Option<usize> {
    let start = url_param_value_start(url, key)?;
    let digits: String = url[start..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse::<usize>().ok()
}

fn set_url_usize_param(mut url: String, key: &str, value: usize) -> String {
    if let Some(start) = url_param_value_start(&url, key) {
        let end = url[start..]
            .find(|c: char| !c.is_ascii_digit())
            .map(|offset| start + offset)
            .unwrap_or(url.len());
        url.replace_range(start..end, &value.to_string());
    } else {
        let separator = if url.contains('?') {
            if url.ends_with('?') || url.ends_with('&') {
                ""
            } else {
                "&"
            }
        } else {
            "?"
        };
        url.push_str(&format!("{}{}={}", separator, key, value));
    }
    url
}

fn parse_url_limit(url: &str) -> Option<usize> {
    parse_url_usize_param(url, "limit")
}

fn set_url_limit(url: String, limit: usize) -> String {
    set_url_usize_param(url, "limit", limit)
}

fn parse_url_offset(url: &str) -> Option<usize> {
    parse_url_usize_param(url, "offset")
}

fn set_url_offset(url: String, offset: usize) -> String {
    set_url_usize_param(url, "offset", offset)
}

fn http_client() -> reqwest::Client {
    HTTP_CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(30))
                .pool_idle_timeout(Duration::from_secs(120))
                .build()
                .expect("Failed to create HTTP client")
        })
        .clone()
}

fn http_runtime() -> &'static tokio::runtime::Runtime {
    HTTP_RUNTIME
        .get_or_init(|| tokio::runtime::Runtime::new().expect("Failed to create Tokio runtime"))
}

/// Start radio-browser server discovery and open a connection to the chosen
/// server in the background, so the first list the user opens doesn't pay
/// for DNS + reverse lookups + TCP/TLS handshake. Runs in its own task, so
/// cancelling a navigation request never aborts the discovery half-way.
fn prewarm_network() {
    let client = http_client();
    http_runtime().spawn(async move {
        let host = resolve_radio_browser_host().await;
        let _ = client
            .head(format!("https://{}/json/stats", host))
            .header("User-Agent", "RustRadio/1.0")
            .send()
            .await;
    });
}

fn clamp_station_limit(limit: usize) -> usize {
    limit.clamp(INITIAL_STATION_LIMIT, MAX_STATION_LIMIT)
}

fn read_limited_text(path: &std::path::Path, max_bytes: u64) -> Result<String, String> {
    let metadata = std::fs::metadata(path).map_err(|e| e.to_string())?;
    if metadata.len() > max_bytes {
        return Err(format!("File is larger than {} bytes", max_bytes));
    }
    std::fs::read_to_string(path).map_err(|e| e.to_string())
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
struct Station {
    stationuuid: String,
    name: String,
    url_resolved: String,
    tags: String,
    country: String,
    #[serde(default)]
    countrycode: String,
    #[serde(default)]
    lastcheckok: i32,
    #[serde(default, deserialize_with = "deserialize_bitrate_kbps")]
    bitrate: Option<u32>,
}

/// Some radio-browser entries list their bitrate in bit/s (e.g. `192000`)
/// instead of kbit/s. No real stream comes close to 10,000 kbps, so anything
/// above that is treated as bit/s and converted.
const BITRATE_BPS_THRESHOLD: u32 = 10_000;

fn normalize_bitrate_kbps(bitrate: u32) -> u32 {
    if bitrate > BITRATE_BPS_THRESHOLD {
        (bitrate + 500) / 1000
    } else {
        bitrate
    }
}

/// Applied everywhere stations are read: API responses, saved favorites /
/// recent / playlists, and the station cache.
fn deserialize_bitrate_kbps<'de, D>(deserializer: D) -> Result<Option<u32>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<u32>::deserialize(deserializer)?.map(normalize_bitrate_kbps))
}

/// The server filters and sorts by the raw value, so a station listed as
/// 128000 passes "≥192" and sorts first. After normalizing, re-apply the
/// request's bitrate bounds and ordering locally.
fn apply_bitrate_query_locally(url: &str, mut stations: Vec<Station>) -> Vec<Station> {
    let min = parse_url_usize_param(url, "bitrateMin");
    let max = parse_url_usize_param(url, "bitrateMax");
    if min.is_some() || max.is_some() {
        stations.retain(|station| match station.bitrate {
            Some(b) => {
                let b = b as usize;
                min.is_none_or(|min| b >= min) && max.is_none_or(|max| b <= max)
            }
            None => true,
        });
    }
    if url_param_value_start(url, "order").is_some_and(|start| url[start..].starts_with("bitrate")) {
        let descending = url_param_value_start(url, "reverse")
            .is_some_and(|start| url[start..].starts_with("true"));
        // Stable sort: stations with equal bitrate keep the server's order.
        if descending {
            stations.sort_by_key(|s| std::cmp::Reverse(s.bitrate.unwrap_or(0)));
        } else {
            stations.sort_by_key(|s| s.bitrate.unwrap_or(0));
        }
    }
    stations
}

struct StationDisplay {
    title_escaped: String,
    subtitle: String,
    /// Full, unescaped text for the row tooltip (the row itself is one line).
    tooltip: String,
    station: Station,
    is_fav: bool,
}

fn station_eq(a: &Station, b: &Station) -> bool {
    if !a.stationuuid.is_empty() && !b.stationuuid.is_empty() {
        a.stationuuid == b.stationuuid
    } else {
        a.url_resolved == b.url_resolved && a.name == b.name
    }
}

#[derive(Deserialize, Debug, Clone)]
struct BrowseItem {
    name: String,
    stationcount: u32,
    #[serde(default, alias = "iso_3166_1")]
    countrycode: String,
}

#[derive(Deserialize, Debug)]
struct RadioBrowserStats {
    // ISO-8601 datetime string: "YYYY-MM-DD HH:MM:SS"
    lastchangetime: String,
}

struct AudioMetadata {
    bitrate: Option<u32>,
    sample_rate: Option<i32>,
    stream_title: Option<String>,
    codec: Option<String>,
    bit_depth: Option<i32>,
    buffering_percent: Option<u32>,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum AnalyzerType {
    Bars,
    Wave,
    BlocksWithHold,
    DigitalVuBlocks,
}

struct AppPreferences {
    prefer_high_bitrate: bool,
    hide_broken: bool,
    auto_update: bool,
    analyzer_type: AnalyzerType,
    custom_tags: Vec<String>,
    audio_sink: String,
    disable_video_streams: bool,
    disable_audio_buffer: bool,
    volume: f64,
}

#[derive(Clone, PartialEq, Debug)]
enum ViewType {
    Tags,
    Languages,
    Countries,
    Stations,
    Favorites,
    Recent,
    Playlists,
    CustomTag(String),
}

fn station_response_view(view: &ViewType) -> ViewType {
    match view {
        ViewType::CustomTag(tag) => ViewType::CustomTag(tag.clone()),
        _ => ViewType::Stations,
    }
}

#[derive(Clone, PartialEq, Debug)]
enum CategoryFilter {
    None,
    Tag(String),
    Language(String),
    Country(String),
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum BitrateFilterSetting {
    None,
    Low,
    High,
    Flac,
}

struct UIState {
    current_view: ViewType,
    previous_view: Option<ViewType>,
    previous_url: String,
    categories: Vec<BrowseItem>,
    stations: Vec<Station>,
    favorites: Vec<Station>,
    recent_stations: Vec<Station>,
    playlists: Vec<Station>,
    current_index: Option<usize>,
    active_category: CategoryFilter,
    active_bitrate: BitrateFilterSetting,
    last_fetched_url: String,
    last_update_timestamp: String,
    total_elements_fetched: usize,
    last_stations_updated_count: usize,
    station_limit: usize,
    station_has_more_hint: bool,
    cached_tags: Option<Vec<BrowseItem>>,
    cached_languages: Option<Vec<BrowseItem>>,
    cached_countries: Option<Vec<BrowseItem>>,
}

struct PlayerState {
    pipeline: Option<gstreamer::Element>,
    bus_guard: Option<gstreamer::bus::BusWatchGuard>,
    generation: u64,
    is_paused: bool,
    current_metadata: Arc<Mutex<AudioMetadata>>,
    currently_playing_station: Option<Station>,
    playlist_view: Option<ViewType>,
}

// --- PERSISTENCE ---
fn default_bitrate_str() -> String {
    "None".to_string()
}

fn default_audio_sink() -> String {
    "autoaudiosink".to_string()
}

fn default_disable_video_streams() -> bool {
    false
}

fn default_disable_audio_buffer() -> bool {
    false
}

fn default_analyzer_type() -> String {
    "BlocksWithHold".to_string()
}

fn default_volume() -> f64 {
    1.0
}

fn default_prefer_high_bitrate() -> bool {
    true
}

fn default_hide_broken() -> bool {
    true
}

fn default_auto_update() -> bool {
    true
}

#[derive(Serialize, Deserialize)]
struct SavedData {
    favorites: Vec<Station>,
    custom_tags: Vec<String>,
    #[serde(default = "default_bitrate_str")]
    active_bitrate: String,
    #[serde(default)]
    recent_stations: Vec<Station>,
    #[serde(default)]
    last_sync_timestamp: String,
    #[serde(default)]
    playlists: Vec<Station>,
    #[serde(default = "default_audio_sink")]
    audio_sink: String,
    #[serde(default = "default_disable_video_streams")]
    disable_video_streams: bool,
    #[serde(default = "default_disable_audio_buffer")]
    disable_audio_buffer: bool,
    #[serde(default = "default_analyzer_type")]
    analyzer_type: String,
    #[serde(default = "default_volume")]
    volume: f64,
    #[serde(default)]
    last_stations_loaded: usize,
    #[serde(default = "default_prefer_high_bitrate")]
    prefer_high_bitrate: bool,
    #[serde(default = "default_hide_broken")]
    hide_broken: bool,
    #[serde(default = "default_auto_update")]
    auto_update: bool,
}

impl Default for SavedData {
    fn default() -> Self {
        Self {
            favorites: Vec::new(),
            custom_tags: Vec::new(),
            active_bitrate: default_bitrate_str(),
            recent_stations: Vec::new(),
            last_sync_timestamp: String::new(),
            playlists: Vec::new(),
            audio_sink: default_audio_sink(),
            disable_video_streams: default_disable_video_streams(),
            disable_audio_buffer: default_disable_audio_buffer(),
            analyzer_type: default_analyzer_type(),
            volume: default_volume(),
            last_stations_loaded: 0,
            prefer_high_bitrate: default_prefer_high_bitrate(),
            hide_broken: default_hide_broken(),
            auto_update: default_auto_update(),
        }
    }
}

struct AppState {
    ui: Arc<Mutex<UIState>>,
    prefs: Arc<Mutex<AppPreferences>>,
}

/// Write a saved-data snapshot atomically. Writes are serialised, and a
/// snapshot older than the last one written is skipped.
fn write_saved_data(seq: u64, data: &SavedData) {
    let mut last_written = SAVE_WRITE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if seq <= *last_written {
        return;
    }

    let mut path = gtk::glib::user_config_dir();
    path.push("RustRadioGTK");
    if let Err(err) = std::fs::create_dir_all(&path) {
        eprintln!("Failed to create configuration directory: {}", err);
        return;
    }
    path.push("saved_data.json");

    let json = match serde_json::to_vec(data) {
        Ok(json) => json,
        Err(err) => {
            eprintln!("Failed to serialize saved data: {}", err);
            return;
        }
    };

    // Keep the previous file intact until the complete replacement is ready.
    let temp_path = path.with_file_name("saved_data.json.tmp");
    let write_result = (|| -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&temp_path)?;
        file.write_all(&json)?;
        file.sync_all()?;
        std::fs::rename(&temp_path, &path)
    })();
    match write_result {
        Ok(()) => *last_written = seq,
        Err(err) => {
            eprintln!("Failed to save application data: {}", err);
            let _ = std::fs::remove_file(temp_path);
        }
    }
}

impl AppState {
    /// Save synchronously. Use where the write must finish before continuing
    /// (e.g. application shutdown).
    fn save_data(&self) {
        let seq = SAVE_SEQ.fetch_add(1, Ordering::AcqRel) + 1;
        let data = self.snapshot();
        write_saved_data(seq, &data);
    }

    /// Snapshot the state on the calling thread, then serialise and write it
    /// (including the fsync) on a background thread so the UI isn't blocked.
    fn save_data_in_background(&self) {
        let seq = SAVE_SEQ.fetch_add(1, Ordering::AcqRel) + 1;
        let data = self.snapshot();
        let spawned = std::thread::Builder::new()
            .name("save-data".to_string())
            .spawn(move || write_saved_data(seq, &data));
        if let Err(err) = spawned {
            eprintln!("Failed to start background save: {}", err);
            self.save_data();
        }
    }

    fn snapshot(&self) -> SavedData {
        let ui = self.ui.lock().unwrap();
        let favorites = ui.favorites.clone();
        let recent_stations = ui.recent_stations.clone();
        let last_sync_timestamp = ui.last_update_timestamp.clone();
        let last_stations_loaded = ui.last_stations_updated_count;
        let playlists = ui.playlists.clone();
        let active_bitrate = match ui.active_bitrate {
            BitrateFilterSetting::None => "None",
            BitrateFilterSetting::Low => "Low",
            BitrateFilterSetting::High => "High",
            BitrateFilterSetting::Flac => "Flac",
        }
        .to_string();
        drop(ui);
        let prefs = self.prefs.lock().unwrap();
        let custom_tags = prefs.custom_tags.clone();
        let audio_sink = prefs.audio_sink.clone();
        let disable_video_streams = prefs.disable_video_streams;
        let disable_audio_buffer = prefs.disable_audio_buffer;
        let volume = prefs.volume;
        let prefer_high_bitrate = prefs.prefer_high_bitrate;
        let hide_broken = prefs.hide_broken;
        let auto_update = prefs.auto_update;
        let analyzer_type = match prefs.analyzer_type {
            AnalyzerType::Bars => "Bars",
            AnalyzerType::Wave => "Wave",
            AnalyzerType::BlocksWithHold => "BlocksWithHold",
            AnalyzerType::DigitalVuBlocks => "DigitalVuBlocks",
        }
        .to_string();
        drop(prefs);

        SavedData {
            favorites,
            custom_tags,
            active_bitrate,
            recent_stations,
            last_sync_timestamp,
            playlists,
            audio_sink,
            disable_video_streams,
            disable_audio_buffer,
            analyzer_type,
            volume,
            last_stations_loaded,
            prefer_high_bitrate,
            hide_broken,
            auto_update,
        }
    }

    fn load_data() -> SavedData {
        let mut path = gtk::glib::user_config_dir();
        path.push("RustRadioGTK");
        path.push("saved_data.json");

        match std::fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str(&text) {
                Ok(data) => data,
                Err(err) => {
                    eprintln!("Saved data is invalid: {}", err);
                    let corrupt_path = path.with_file_name("saved_data.json.corrupt");
                    let _ = std::fs::copy(&path, corrupt_path);
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
}

fn main() {
    gstreamer::init().expect("Failed to initialize GStreamer.");

    let application = Application::builder()
        .application_id("io.github.antoxa78.RustRadio")
        .build();

    application.connect_activate(build_ui);
    application.run();
}

/// Calculate adaptive buffer duration based on stream bitrate
/// Lower bitrate streams need more buffering time
fn calculate_adaptive_buffer(bitrate_kbps: Option<u32>) -> i64 {
    match bitrate_kbps {
        Some(bitrate) => {
            let duration_seconds = match bitrate {
                0..=96 => 20,    // Low bitrate (64-96 kbps): 20 seconds
                97..=192 => 15,  // Medium bitrate (128-192 kbps): 15 seconds
                193..=320 => 10, // High bitrate (256-320 kbps): 10 seconds
                _ => 8,          // Very high bitrate (>320 kbps): 8 seconds
            };
            #[cfg(debug_assertions)]
            eprintln!(
                "📊 Adaptive Buffer: {} kbps → {} seconds buffer",
                bitrate, duration_seconds
            );
            duration_seconds * 1_000_000_000i64 // Convert to nanoseconds
        }
        None => {
            #[cfg(debug_assertions)]
            eprintln!("📊 Adaptive Buffer: Unknown bitrate → 10 seconds buffer (default)");
            10_000_000_000i64 // Default 10 seconds
        }
    }
}

fn is_allowed_stream_url(url: &str) -> bool {
    let Some((scheme, rest)) = url.trim().split_once("://") else {
        return false;
    };
    if !matches!(scheme.to_ascii_lowercase().as_str(), "http" | "https") {
        return false;
    }

    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.is_empty() || authority.contains('@') {
        return false;
    }
    let host = if authority.starts_with('[') {
        let Some(close) = authority.find(']') else {
            return false;
        };
        let suffix = &authority[close + 1..];
        if !suffix.is_empty()
            && (!suffix.starts_with(':')
                || suffix.len() == 1
                || suffix[1..].parse::<u16>().is_err())
        {
            return false;
        }
        &authority[1..close]
    } else {
        if authority.matches(':').count() > 1 {
            return false;
        }
        if let Some((host, port)) = authority.rsplit_once(':') {
            if host.is_empty() || port.parse::<u16>().is_err() {
                return false;
            }
            host
        } else {
            authority
        }
    };
    let normalized_host = host.trim_end_matches('.');
    let host_lower = normalized_host.to_ascii_lowercase();
    if normalized_host.is_empty()
        || host_lower == "localhost"
        || host_lower.ends_with(".localhost")
        || host_lower.ends_with(".local")
        || host_lower.ends_with(".internal")
    {
        return false;
    }
    if let Ok(ip) = normalized_host.parse::<std::net::IpAddr>() {
        if is_local_or_private_ip(ip) {
            return false;
        }
    }
    true
}

fn is_local_or_private_ip(ip: std::net::IpAddr) -> bool {
    if ip.is_loopback() || ip.is_unspecified() {
        return true;
    }
    match ip {
        std::net::IpAddr::V4(ip) => ip.is_private() || ip.is_link_local(),
        std::net::IpAddr::V6(ip) => {
            let segments = ip.segments();
            let unique_local = segments[0] & 0xfe00 == 0xfc00;
            let link_local = segments[0] & 0xffc0 == 0xfe80;
            let ipv4_mapped =
                segments[..5].iter().all(|segment| *segment == 0) && segments[5] == 0xffff;
            let mapped_private = if ipv4_mapped {
                let mapped = std::net::Ipv4Addr::new(
                    (segments[6] >> 8) as u8,
                    segments[6] as u8,
                    (segments[7] >> 8) as u8,
                    segments[7] as u8,
                );
                mapped.is_private() || mapped.is_link_local() || mapped.is_loopback()
            } else {
                false
            };
            unique_local || link_local || mapped_private
        }
    }
}

fn stream_host_port(url: &str) -> Option<(String, u16)> {
    let (scheme, rest) = url.trim().split_once("://")?;
    let default_port = if scheme.eq_ignore_ascii_case("https") {
        443
    } else {
        80
    };
    let authority = rest.split(['/', '?', '#']).next()?;
    if authority.is_empty() || authority.contains('@') {
        return None;
    }
    if authority.starts_with('[') {
        let close = authority.find(']')?;
        let host = &authority[1..close];
        let suffix = &authority[close + 1..];
        let port = if suffix.is_empty() {
            default_port
        } else {
            suffix.strip_prefix(':')?.parse().ok()?
        };
        return Some((host.to_string(), port));
    }
    if authority.matches(':').count() > 1 {
        return None;
    }
    if let Some((host, port)) = authority.rsplit_once(':') {
        Some((host.to_string(), port.parse().ok()?))
    } else {
        Some((authority.to_string(), default_port))
    }
}

async fn host_resolves_to_public(host: String, port: u16) -> bool {
    let resolved = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::net::lookup_host((host.as_str(), port)),
    )
    .await;
    match resolved {
        Ok(Ok(addresses)) => {
            let addresses: Vec<_> = addresses.collect();
            !addresses.is_empty()
                && addresses
                    .iter()
                    .all(|address| !is_local_or_private_ip(address.ip()))
        }
        _ => false,
    }
}

fn invalidate_player(player: &mut PlayerState) -> u64 {
    player.generation = player.generation.wrapping_add(1);
    player.bus_guard = None;
    if let Some(old_pipeline) = player.pipeline.take() {
        let _ = old_pipeline.set_state(gstreamer::State::Null);
    }
    player.is_paused = false;
    player.currently_playing_station = None;
    player.playlist_view = None;
    if let Ok(mut metadata) = player.current_metadata.lock() {
        metadata.bitrate = None;
        metadata.sample_rate = None;
        metadata.stream_title = None;
        metadata.codec = None;
        metadata.bit_depth = None;
        metadata.buffering_percent = None;
    }
    player.generation
}

fn station_list_for_view<'a>(ui: &'a UIState, view: &ViewType) -> &'a [Station] {
    let use_filtered_local = ui.active_bitrate != BitrateFilterSetting::None
        && matches!(
            view,
            ViewType::Favorites | ViewType::Recent | ViewType::Playlists
        );
    if use_filtered_local {
        &ui.stations
    } else {
        match view {
            ViewType::Favorites => &ui.favorites,
            ViewType::Recent => &ui.recent_stations,
            ViewType::Playlists => &ui.playlists,
            _ => &ui.stations,
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn schedule_station_fallback(
    player_lock: &Arc<Mutex<PlayerState>>,
    ui_lock: &Arc<Mutex<UIState>>,
    next_index: Option<usize>,
    expected_generation: u64,
    play_pause_btn: &Button,
    now_playing_title: &Label,
    now_playing_subtitle: &Label,
    stream_meta_label: &Label,
    stream_info_label: &Label,
    fav_toggle_btn: &Button,
    now_playing_flag_lbl: &Label,
    save_mgr: &Arc<AppState>,
    prefs: &Arc<Mutex<AppPreferences>>,
) -> bool {
    let Some(index) = next_index else {
        return false;
    };
    let view = match player_lock.lock() {
        Ok(player) => player.playlist_view.clone(),
        Err(_) => return false,
    };
    let total = {
        let ui = ui_lock.lock().unwrap();
        let view = view.unwrap_or_else(|| ui.current_view.clone());
        station_list_for_view(&ui, &view).len()
    };
    if index >= total {
        return false;
    }

    let player_rc = player_lock.clone();
    let ui_rc = ui_lock.clone();
    let btn_rc = play_pause_btn.clone();
    let title_rc = now_playing_title.clone();
    let subtitle_rc = now_playing_subtitle.clone();
    let meta_rc = stream_meta_label.clone();
    let info_rc = stream_info_label.clone();
    let fav_rc = fav_toggle_btn.clone();
    let flag_rc = now_playing_flag_lbl.clone();
    let save_rc = save_mgr.clone();
    let prefs_rc = prefs.clone();
    let player_check = player_rc.clone();
    let expected_request = ACTIVE_REQUEST_ID.load(Ordering::Acquire);
    glib::idle_add_local_once(move || {
        if !is_active_request(expected_request)
            || player_check
                .lock()
                .map(|player| player.generation != expected_generation)
                .unwrap_or(true)
        {
            return;
        }
        play_station(
            &player_rc,
            &ui_rc,
            index,
            &btn_rc,
            &title_rc,
            &subtitle_rc,
            &meta_rc,
            &info_rc,
            &fav_rc,
            &flag_rc,
            &save_rc,
            &prefs_rc,
        );
    });
    true
}

fn request_more_stations(
    list_box: &ListBox,
    load_row: &ActionRow,
    tx: &async_channel::Sender<IncomingData>,
    prefs: &Arc<Mutex<AppPreferences>>,
    ui_lock: &Arc<Mutex<UIState>>,
) {
    // Fetch only the next page (offset = stations already requested) and
    // append it, instead of re-downloading and re-rendering the whole list.
    let (url, view) = {
        let ui = ui_lock.lock().unwrap();
        if !matches!(ui.current_view, ViewType::Stations | ViewType::CustomTag(_)) {
            return;
        }
        let offset = ui.station_limit;
        if offset >= MAX_STATION_LIMIT {
            return;
        }
        let page = STATION_PAGE_SIZE.min(MAX_STATION_LIMIT - offset);
        let url = set_url_offset(set_url_limit(ui.last_fetched_url.clone(), page), offset);
        (url, ui.current_view.clone())
    };

    list_box.remove(load_row);
    let loading_row = ActionRow::builder()
        .title("Loading more stations...")
        .activatable(false)
        .build();
    let spinner = gtk::Spinner::builder().valign(gtk::Align::Center).build();
    spinner.set_size_request(16, 16);
    spinner.start();
    loading_row.add_suffix(&spinner);
    list_box.append(&loading_row);

    fetch_data_async(url, true, view, prefs.clone(), tx.clone(), true);
}

#[allow(clippy::too_many_arguments)]
fn play_station(
    player_lock: &Arc<Mutex<PlayerState>>,
    ui_lock: &Arc<Mutex<UIState>>,
    index: usize,
    play_pause_btn: &Button,
    now_playing_title: &Label,
    now_playing_subtitle: &Label,
    stream_meta_label: &Label,
    stream_info_label: &Label,
    fav_toggle_btn: &Button,
    now_playing_flag_lbl: &Label,
    save_mgr: &Arc<AppState>,
    prefs: &Arc<Mutex<AppPreferences>>,
) {
    play_station_internal(
        player_lock,
        ui_lock,
        index,
        play_pause_btn,
        now_playing_title,
        now_playing_subtitle,
        stream_meta_label,
        stream_info_label,
        fav_toggle_btn,
        now_playing_flag_lbl,
        save_mgr,
        prefs,
        false,
    );
}

#[allow(clippy::too_many_arguments)]
fn play_station_internal(
    player_lock: &Arc<Mutex<PlayerState>>,
    ui_lock: &Arc<Mutex<UIState>>,
    index: usize,
    play_pause_btn: &Button,
    now_playing_title: &Label,
    now_playing_subtitle: &Label,
    stream_meta_label: &Label,
    stream_info_label: &Label,
    fav_toggle_btn: &Button,
    now_playing_flag_lbl: &Label,
    save_mgr: &Arc<AppState>,
    prefs: &Arc<Mutex<AppPreferences>>,
    url_prevalidated: bool,
) {
    let mut ui = ui_lock.lock().unwrap();
    let is_filtered = ui.active_bitrate != BitrateFilterSetting::None;
    let station = if is_filtered
        && matches!(
            ui.current_view,
            ViewType::Favorites | ViewType::Recent | ViewType::Playlists
        ) {
        if index >= ui.stations.len() {
            return;
        }
        ui.current_index = Some(index);
        ui.stations[index].clone()
    } else {
        match ui.current_view {
            ViewType::Favorites => {
                if index >= ui.favorites.len() {
                    return;
                }
                ui.current_index = Some(index);
                ui.favorites[index].clone()
            }
            ViewType::Recent => {
                if index >= ui.recent_stations.len() {
                    return;
                }
                ui.current_index = Some(index);
                ui.recent_stations[index].clone()
            }
            ViewType::Playlists => {
                if index >= ui.playlists.len() {
                    return;
                }
                ui.current_index = Some(index);
                ui.playlists[index].clone()
            }
            _ => {
                if index >= ui.stations.len() {
                    return;
                }
                ui.current_index = Some(index);
                ui.stations[index].clone()
            }
        }
    };
    let playback_view = ui.current_view.clone();

    if !is_allowed_stream_url(&station.url_resolved) {
        drop(ui);
        let mut player = player_lock.lock().unwrap();
        invalidate_player(&mut player);
        drop(player);
        play_pause_btn.set_icon_name("media-playback-start-symbolic");
        stream_meta_label.set_text("(Unsupported stream URL)");
        stream_info_label.set_text("");
        return;
    }

    if !url_prevalidated {
        if let Some((host, port)) = stream_host_port(&station.url_resolved) {
            if host.parse::<std::net::IpAddr>().is_err() {
                drop(ui);
                let (result_tx, result_rx) = async_channel::bounded(1);
                let runtime = HTTP_RUNTIME.get_or_init(|| {
                    tokio::runtime::Runtime::new().expect("Failed to create Tokio runtime")
                });
                runtime.spawn(async move {
                    let allowed = host_resolves_to_public(host, port).await;
                    let _ = result_tx.send(allowed).await;
                });

                let expected_generation = {
                    let mut player = player_lock.lock().unwrap();
                    invalidate_player(&mut player)
                };
                let expected_request = ACTIVE_REQUEST_ID.load(Ordering::Acquire);
                play_pause_btn.set_icon_name("media-playback-start-symbolic");
                stream_meta_label.set_text("(Checking stream URL...)");
                stream_info_label.set_text("");

                let player_rc = player_lock.clone();
                let ui_rc = ui_lock.clone();
                let btn_rc = play_pause_btn.clone();
                let title_rc = now_playing_title.clone();
                let subtitle_rc = now_playing_subtitle.clone();
                let meta_rc = stream_meta_label.clone();
                let info_rc = stream_info_label.clone();
                let fav_rc = fav_toggle_btn.clone();
                let flag_rc = now_playing_flag_lbl.clone();
                let save_rc = save_mgr.clone();
                let prefs_rc = prefs.clone();
                let player_check = player_rc.clone();
                glib::MainContext::default().spawn_local(async move {
                    let allowed = result_rx.recv().await.unwrap_or(false);
                    if !is_active_request(expected_request)
                        || player_check
                            .lock()
                            .map(|player| player.generation != expected_generation)
                            .unwrap_or(true)
                    {
                        return;
                    }
                    if !allowed {
                        btn_rc.set_icon_name("media-playback-start-symbolic");
                        meta_rc.set_text("(Stream URL resolves to a local/private address)");
                        info_rc.set_text("");
                        return;
                    }
                    play_station_internal(
                        &player_rc,
                        &ui_rc,
                        index,
                        &btn_rc,
                        &title_rc,
                        &subtitle_rc,
                        &meta_rc,
                        &info_rc,
                        &fav_rc,
                        &flag_rc,
                        &save_rc,
                        &prefs_rc,
                        true,
                    );
                });
                return;
            }
        }
    }

    // Track recently played (keep last 50, no duplicates)
    // For imported stations (empty UUID), deduplicate by URL instead
    ui.recent_stations.retain(|s| {
        if s.stationuuid.is_empty() {
            s.url_resolved != station.url_resolved
        } else {
            s.stationuuid != station.stationuuid
        }
    });
    ui.recent_stations.insert(0, station.clone());
    ui.recent_stations.truncate(50);

    if ui.favorites.iter().any(|f| station_eq(f, &station)) {
        fav_toggle_btn.set_icon_name("starred-symbolic");
    } else {
        fav_toggle_btn.set_icon_name("non-starred-symbolic");
    }
    drop(ui);
    save_mgr.save_data();

    let mut player = player_lock.lock().unwrap();
    let playback_generation = invalidate_player(&mut player);
    play_pause_btn.set_icon_name("media-playback-pause-symbolic");

    now_playing_title.set_text(&station.name);
    let br_info = station
        .bitrate
        .map_or(String::new(), |b| format!(" ({} kbps)", b));
    now_playing_subtitle.set_text(&format!(
        "{} | {}{}",
        station.country, station.tags, br_info
    ));
    let flag = country_code_to_flag(&station.countrycode);
    now_playing_flag_lbl.set_text(&flag);
    now_playing_flag_lbl.set_visible(!flag.is_empty());

    {
        let mut meta = player.current_metadata.lock().unwrap();
        meta.bitrate = None;
        meta.sample_rate = None;
        meta.stream_title = None;
        meta.codec = None;
        meta.bit_depth = None;
        meta.buffering_percent = None;
    }
    stream_meta_label.set_text("(Connecting...)");
    stream_info_label.set_text("");

    let url = station.url_resolved.clone();
    let sink_name = prefs.lock().unwrap().audio_sink.clone();
    let disable_video = prefs.lock().unwrap().disable_video_streams;
    let audio_sink = gstreamer::ElementFactory::make(&sink_name)
        .build()
        .or_else(|_| gstreamer::ElementFactory::make("autoaudiosink").build())
        .ok();

    let video_sink = if disable_video {
        gstreamer::ElementFactory::make("fakesink").build().ok()
    } else {
        None
    };

    let disable_buffer = prefs.lock().unwrap().disable_audio_buffer;
    let buffer_duration: i64 = if disable_buffer {
        -1 // -1 = use GStreamer's default (no forced pre-buffering)
    } else {
        calculate_adaptive_buffer(station.bitrate)
    };
    let mut playbin_builder = gstreamer::ElementFactory::make("playbin")
        .property("uri", &url)
        .property("buffer-duration", buffer_duration); // Adaptive buffer based on bitrate
    if let Some(ref sink) = audio_sink {
        playbin_builder = playbin_builder.property("audio-sink", sink);
    }

    // Disable video streams if preference is enabled
    if let Some(ref sink) = video_sink {
        playbin_builder = playbin_builder.property("video-sink", sink);
    }

    if let Ok(pipeline) = playbin_builder.build() {
        let buffer_disabled = prefs.lock().unwrap().disable_audio_buffer;

        // Apply saved volume immediately
        let saved_vol = prefs.lock().unwrap().volume;
        pipeline.set_property("volume", saved_vol);

        // Start in PAUSED state for prebuffering, or PLAYING if buffer is disabled
        let initial_state = if buffer_disabled {
            gstreamer::State::Playing
        } else {
            gstreamer::State::Paused
        };
        if let Err(err) = pipeline.set_state(initial_state) {
            eprintln!("Failed to start playback for {}: {}", url, err);
            let _ = pipeline.set_state(gstreamer::State::Null);
            let failure_generation = invalidate_player(&mut player);
            drop(player);
            play_pause_btn.set_icon_name("media-playback-start-symbolic");
            let scheduled = schedule_station_fallback(
                player_lock,
                ui_lock,
                Some(index + 1),
                failure_generation,
                play_pause_btn,
                now_playing_title,
                now_playing_subtitle,
                stream_meta_label,
                stream_info_label,
                fav_toggle_btn,
                now_playing_flag_lbl,
                save_mgr,
                prefs,
            );
            if !scheduled {
                stream_meta_label.set_text("(Playback unavailable)");
            }
            return;
        }
        let Some(bus) = pipeline.bus() else {
            eprintln!("Failed to get the GStreamer bus for {}", url);
            let _ = pipeline.set_state(gstreamer::State::Null);
            let failure_generation = invalidate_player(&mut player);
            drop(player);
            play_pause_btn.set_icon_name("media-playback-start-symbolic");
            let scheduled = schedule_station_fallback(
                player_lock,
                ui_lock,
                Some(index + 1),
                failure_generation,
                play_pause_btn,
                now_playing_title,
                now_playing_subtitle,
                stream_meta_label,
                stream_info_label,
                fav_toggle_btn,
                now_playing_flag_lbl,
                save_mgr,
                prefs,
            );
            if !scheduled {
                stream_meta_label.set_text("(Playback unavailable)");
            }
            return;
        };
        let meta_rc = player.current_metadata.clone();
        let label_rc = stream_meta_label.clone();

        let player_fallback = player_lock.clone();
        let ui_fallback = ui_lock.clone();
        let btn_fallback = play_pause_btn.clone();
        let title_fallback = now_playing_title.clone();
        let sub_fallback = now_playing_subtitle.clone();
        let meta_lbl_fallback = stream_meta_label.clone();
        let info_lbl_fallback = stream_info_label.clone();
        let fav_fallback = fav_toggle_btn.clone();
        let flag_lbl_fallback = now_playing_flag_lbl.clone();
        let save_mgr_fallback = save_mgr.clone();
        let prefs_fallback = prefs.clone();
        let pipeline_for_buffering = Arc::new(Mutex::new(pipeline.clone()));
        let is_current_generation = {
            let player_fallback = player_fallback.clone();
            move || {
                player_fallback
                    .lock()
                    .map(|player| player.generation == playback_generation)
                    .unwrap_or(false)
            }
        };

        let guard = bus.add_watch_local(move |_, msg| {
            match msg.view() {
                gstreamer::MessageView::Buffering(buffering_msg) => {
                    if !is_current_generation() {
                        return glib::ControlFlow::Break;
                    }
                    let percent = buffering_msg.percent();
                    #[cfg(debug_assertions)]
                    eprintln!("Buffering: {}%", percent);

                    if buffer_disabled {
                        return glib::ControlFlow::Continue;
                    }

                    {
                        let mut meta = meta_rc.lock().unwrap();
                        meta.buffering_percent = Some(percent as u32);
                    }

                    // When buffering reaches 100%, start playback
                    if percent == 100 {
                        {
                            let mut meta = meta_rc.lock().unwrap();
                            meta.buffering_percent = None;
                        }
                        let should_start = player_fallback
                            .lock()
                            .map(|player| {
                                player.generation == playback_generation && !player.is_paused
                            })
                            .unwrap_or(false);
                        if should_start {
                            if let Ok(pipeline) = pipeline_for_buffering.lock() {
                                if let Err(err) = pipeline.set_state(gstreamer::State::Playing) {
                                    eprintln!("Failed to start buffered playback: {}", err);
                                    let next_index = ui_fallback
                                        .lock()
                                        .ok()
                                        .and_then(|ui| ui.current_index.map(|index| index + 1));
                                    let failure_generation = player_fallback
                                        .lock()
                                        .map(|mut player| invalidate_player(&mut player))
                                        .unwrap_or(playback_generation);
                                    let scheduled = schedule_station_fallback(
                                        &player_fallback,
                                        &ui_fallback,
                                        next_index,
                                        failure_generation,
                                        &btn_fallback,
                                        &title_fallback,
                                        &sub_fallback,
                                        &meta_lbl_fallback,
                                        &info_lbl_fallback,
                                        &fav_fallback,
                                        &flag_lbl_fallback,
                                        &save_mgr_fallback,
                                        &prefs_fallback,
                                    );
                                    if !scheduled {
                                        btn_fallback.set_icon_name("media-playback-start-symbolic");
                                        meta_lbl_fallback.set_text("(Playback unavailable)");
                                        info_lbl_fallback.set_text("");
                                    }
                                    return glib::ControlFlow::Break;
                                }
                            }
                        }
                    }
                }
                gstreamer::MessageView::Tag(tags_msg) => {
                    if !is_current_generation() {
                        return glib::ControlFlow::Break;
                    }
                    let tags = tags_msg.tags();
                    let mut meta = meta_rc.lock().unwrap();

                    if let Some(title) = tags.index::<gstreamer::tags::Title>(0) {
                        meta.stream_title = Some(title.get().to_string());
                    } else if let Some(artist) = tags.index::<gstreamer::tags::Artist>(0) {
                        meta.stream_title = Some(artist.get().to_string());
                    }
                    if let Some(bitrate) = tags.index::<gstreamer::tags::Bitrate>(0) {
                        meta.bitrate = Some(bitrate.get());
                        #[cfg(debug_assertions)]
                        eprintln!("🎵 Stream Bitrate Detected: {} kbps", bitrate.get() / 1000);
                    }
                    if meta.codec.is_none() {
                        if let Some(codec) = tags.index::<gstreamer::tags::AudioCodec>(0) {
                            meta.codec = Some(codec.get().to_string());
                        }
                    }

                    let title_str = meta
                        .stream_title
                        .clone()
                        .unwrap_or_else(|| "Live Stream".to_string());
                    label_rc.set_text(&format!("♫ {}", title_str));
                }
                gstreamer::MessageView::Error(_) | gstreamer::MessageView::Eos(..) => {
                    if !is_current_generation() {
                        return glib::ControlFlow::Break;
                    }
                    #[cfg(debug_assertions)]
                    eprintln!("Playback ended or failed. Attempting fallback track...");

                    let next_index = {
                        let player = player_fallback.lock().unwrap();
                        let playing = player.currently_playing_station.clone();
                        let playlist_view = player.playlist_view.clone();
                        drop(player);
                        let ui = ui_fallback.lock().unwrap();
                        if playlist_view.as_ref() != Some(&ui.current_view) {
                            None
                        } else {
                            playing.and_then(|station| {
                                station_list_for_view(&ui, &ui.current_view)
                                    .iter()
                                    .position(|candidate| station_eq(candidate, &station))
                                    .map(|index| index + 1)
                            })
                        }
                    };
                    let fallback_generation = {
                        let mut player = player_fallback.lock().unwrap();
                        if player.generation != playback_generation {
                            return glib::ControlFlow::Break;
                        }
                        invalidate_player(&mut player)
                    };
                    let scheduled = schedule_station_fallback(
                        &player_fallback,
                        &ui_fallback,
                        next_index,
                        fallback_generation,
                        &btn_fallback,
                        &title_fallback,
                        &sub_fallback,
                        &meta_lbl_fallback,
                        &info_lbl_fallback,
                        &fav_fallback,
                        &flag_lbl_fallback,
                        &save_mgr_fallback,
                        &prefs_fallback,
                    );
                    if !scheduled {
                        btn_fallback.set_icon_name("media-playback-start-symbolic");
                        meta_lbl_fallback.set_text("(Playback ended)");
                        info_lbl_fallback.set_text("");
                    }
                }
                _ => {}
            }
            glib::ControlFlow::Continue
        });

        let guard = match guard {
            Ok(guard) => guard,
            Err(err) => {
                eprintln!("Failed to hook GStreamer bus: {}", err);
                let _ = pipeline.set_state(gstreamer::State::Null);
                let failure_generation = invalidate_player(&mut player);
                drop(player);
                play_pause_btn.set_icon_name("media-playback-start-symbolic");
                let scheduled = schedule_station_fallback(
                    player_lock,
                    ui_lock,
                    Some(index + 1),
                    failure_generation,
                    play_pause_btn,
                    now_playing_title,
                    now_playing_subtitle,
                    stream_meta_label,
                    stream_info_label,
                    fav_toggle_btn,
                    now_playing_flag_lbl,
                    save_mgr,
                    prefs,
                );
                if !scheduled {
                    stream_meta_label.set_text("(Playback unavailable)");
                }
                return;
            }
        };

        player.currently_playing_station = Some(station.clone());
        player.playlist_view = Some(playback_view);
        player.bus_guard = Some(guard);
        player.pipeline = Some(pipeline);
    } else {
        #[cfg(debug_assertions)]
        eprintln!("Failed to create pipeline for {}, skipping...", url);
        let failure_generation = invalidate_player(&mut player);
        drop(player);
        play_pause_btn.set_icon_name("media-playback-start-symbolic");
        let scheduled = schedule_station_fallback(
            player_lock,
            ui_lock,
            Some(index + 1),
            failure_generation,
            play_pause_btn,
            now_playing_title,
            now_playing_subtitle,
            stream_meta_label,
            stream_info_label,
            fav_toggle_btn,
            now_playing_flag_lbl,
            save_mgr,
            prefs,
        );
        if !scheduled {
            stream_meta_label.set_text("(Playback unavailable)");
        }
    }
}

fn parse_m3u(content: &str) -> Vec<Station> {
    let mut stations = Vec::new();
    let mut next_name = String::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("#EXTINF:") {
            if let Some(comma) = trimmed.find(',') {
                next_name = trimmed[comma + 1..].trim().to_string();
            }
        } else if !trimmed.is_empty() && !trimmed.starts_with('#') {
            if stations.len() >= MAX_PLAYLIST_STATIONS {
                break;
            }
            if !is_allowed_stream_url(trimmed) {
                next_name.clear();
                continue;
            }
            stations.push(Station {
                stationuuid: String::new(),
                name: if next_name.is_empty() {
                    trimmed.to_string()
                } else {
                    next_name.clone()
                },
                url_resolved: trimmed.to_string(),
                tags: "imported".to_string(),
                country: String::new(),
                countrycode: String::new(),
                lastcheckok: 1,
                bitrate: None,
            });
            next_name.clear();
        }
    }
    stations
}

fn parse_pls(content: &str) -> Vec<Station> {
    let mut stations = Vec::new();
    let mut file_map: Vec<(String, String)> = Vec::new();
    let mut title_map: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let content = content.trim_start_matches('\u{FEFF}').replace("\r\n", "\n");

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('[') || trimmed.starts_with('#') {
            continue;
        }
        let upper = trimmed.to_uppercase();
        if let Some(eq) = trimmed.find('=') {
            if upper.starts_with("FILE") {
                let num_part = trimmed[4..eq].trim().to_string();
                let url = trimmed[eq + 1..].trim().to_string();
                if is_allowed_stream_url(&url) {
                    file_map.push((num_part, url));
                }
            } else if upper.starts_with("TITLE") {
                let num_part = trimmed[5..eq].trim().to_string();
                let name = trimmed[eq + 1..].trim().to_string();
                if !name.is_empty() {
                    title_map.insert(num_part, name);
                }
            }
        }
    }

    for (num, url) in file_map {
        if stations.len() >= MAX_PLAYLIST_STATIONS {
            break;
        }
        let name = title_map.get(&num).cloned().unwrap_or_else(|| url.clone());
        stations.push(Station {
            stationuuid: String::new(),
            name,
            url_resolved: url,
            tags: "imported".to_string(),
            country: String::new(),
            countrycode: String::new(),
            lastcheckok: 1,
            bitrate: None,
        });
    }
    stations
}

fn country_code_to_flag(code: &str) -> String {
    code.chars()
        .filter_map(|c| {
            if c.is_ascii_alphabetic() {
                Some(
                    char::from_u32(0x1F1E6 + (c.to_ascii_uppercase() as u32 - 'A' as u32)).unwrap(),
                )
            } else {
                None
            }
        })
        .collect()
}

fn format_time(duration: Duration) -> String {
    let total_secs = duration.as_secs();
    format!("{:02}:{:02}", total_secs / 60, total_secs % 60)
}

fn local_time_string() -> String {
    glib::DateTime::now_local()
        .ok()
        .and_then(|now| now.format("%Y-%m-%d %H:%M:%S").ok())
        .map(|formatted| formatted.to_string())
        .unwrap_or_else(|| "Unknown".to_string())
}

/// Escape text for widgets that interpret Pango markup (AdwActionRow titles
/// and subtitles do by default). Station names, tags and URLs come from the
/// server or imported playlists and may contain '&', '<', '>'.
fn markup_text(text: &str) -> String {
    glib::markup_escape_text(text).to_string()
}

fn unix_time_local_string(secs: u64) -> String {
    i64::try_from(secs)
        .ok()
        .and_then(|secs| glib::DateTime::from_unix_local(secs).ok())
        .and_then(|time| time.format("%Y-%m-%d %H:%M:%S").ok())
        .map(|formatted| formatted.to_string())
        .unwrap_or_else(|| "Unknown".to_string())
}

enum IncomingData {
    Categories {
        request_id: u64,
        view: ViewType,
        items: Vec<BrowseItem>,
    },
    Stations {
        request_id: u64,
        view: ViewType,
        stations: Vec<Station>,
        sync_time: String,
        is_load_more: bool,
        limit: usize,
        preserve_index: bool,
        has_more_hint: bool,
        /// Background refresh of a list already on screen: redraw in place,
        /// keeping the scroll position.
        keep_scroll: bool,
    },
    Stats {
        request_id: u64,
        sync_time: String,
    },
    Error {
        request_id: u64,
        is_load_more: bool,
        message: String,
    },
}

impl IncomingData {
    fn request_id(&self) -> u64 {
        match self {
            Self::Categories { request_id, .. }
            | Self::Stations { request_id, .. }
            | Self::Stats { request_id, .. }
            | Self::Error { request_id, .. } => *request_id,
        }
    }

    fn is_load_more(&self) -> bool {
        matches!(
            self,
            Self::Stations {
                is_load_more: true,
                ..
            } | Self::Error {
                is_load_more: true,
                ..
            }
        )
    }

    fn keeps_scroll(&self) -> bool {
        matches!(
            self,
            Self::Stations {
                keep_scroll: true,
                ..
            }
        )
    }

    fn changes_list(&self) -> bool {
        matches!(
            self,
            Self::Categories { .. } | Self::Stations { .. } | Self::Error { .. }
        )
    }
}

fn send_local_categories(
    tx: &async_channel::Sender<IncomingData>,
    view: ViewType,
    items: Vec<BrowseItem>,
) {
    let request_id = begin_request();
    let data = IncomingData::Categories {
        request_id,
        view,
        items,
    };
    send_local_data(tx, data);
}

fn send_local_stations(
    tx: &async_channel::Sender<IncomingData>,
    view: ViewType,
    stations: Vec<Station>,
    sync_time: String,
) {
    send_local_stations_with_limit(tx, view, stations, sync_time, INITIAL_STATION_LIMIT);
}

fn send_local_stations_with_limit(
    tx: &async_channel::Sender<IncomingData>,
    view: ViewType,
    stations: Vec<Station>,
    sync_time: String,
    limit: usize,
) {
    send_local_stations_with_limit_hint(tx, view, stations, sync_time, limit, false);
}

fn send_local_stations_with_limit_hint(
    tx: &async_channel::Sender<IncomingData>,
    view: ViewType,
    stations: Vec<Station>,
    sync_time: String,
    limit: usize,
    has_more_hint: bool,
) {
    let request_id = begin_request();
    let data = IncomingData::Stations {
        request_id,
        view,
        stations,
        sync_time,
        is_load_more: false,
        limit: clamp_station_limit(limit),
        preserve_index: false,
        has_more_hint,
        keep_scroll: false,
    };
    send_local_data(tx, data);
}

fn send_local_data(tx: &async_channel::Sender<IncomingData>, data: IncomingData) {
    match tx.try_send(data) {
        Ok(()) | Err(async_channel::TrySendError::Closed(_)) => {}
        Err(async_channel::TrySendError::Full(data)) => {
            let tx = tx.clone();
            glib::MainContext::default().spawn_local(async move {
                let _ = tx.send(data).await;
            });
        }
    }
}

async fn send_network_data(
    tx: &async_channel::Sender<IncomingData>,
    cancel: &mut tokio::sync::watch::Receiver<u64>,
    request_id: u64,
    data: IncomingData,
) -> bool {
    if *cancel.borrow() != request_id {
        return false;
    }
    tokio::select! {
        result = tx.send(data) => result.is_ok(),
        _ = cancel.changed() => false,
    }
}

/// Return the radio-browser API host to use. Discovery runs once per session;
/// concurrent callers wait on the same discovery. The fallback host is cached
/// too, so a failed lookup isn't repeated (and waited on) for every request.
async fn resolve_radio_browser_host() -> String {
    RADIO_BROWSER_HOST
        .get_or_init(discover_radio_browser_host)
        .await
        .clone()
}

/// Resolve `all.api.radio-browser.info` via DNS, reverse-lookup every IP in
/// parallel to get the actual server hostnames, and return a randomly chosen
/// one. Falls back to `de1.api.radio-browser.info` on any error.
async fn discover_radio_browser_host() -> String {
    const FALLBACK_HOST: &str = "de1.api.radio-browser.info";
    use std::net::ToSocketAddrs;

    let ips = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::task::spawn_blocking(|| {
            ("all.api.radio-browser.info", 443u16)
                .to_socket_addrs()
                .map(|iter| iter.map(|sa| sa.ip()).collect::<Vec<_>>())
        }),
    )
    .await;

    let ips = match ips {
        Ok(Ok(Ok(v))) if !v.is_empty() => v,
        _ => {
            #[cfg(debug_assertions)]
            eprintln!("🌐 Radio-Browser DNS: forward lookup failed, using de1 fallback");
            return FALLBACK_HOST.to_string();
        }
    };

    // Reverse-lookup all IPs at once, bounded as a whole, so one slow or
    // unresponsive resolver can't add seconds per server.
    let mut lookups = tokio::task::JoinSet::new();
    for ip in ips {
        lookups.spawn_blocking(move || lookup_addr(&ip));
    }
    let mut names: Vec<String> = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(3), async {
        while let Some(result) = lookups.join_next().await {
            if let Ok(Ok(hostname)) = result {
                let host = hostname.trim_end_matches('.').to_string();
                if host.ends_with(".api.radio-browser.info") && !names.contains(&host) {
                    names.push(host);
                }
            }
        }
    })
    .await;
    // Blocking lookups that are still running finish on their own; their
    // results are simply ignored.
    lookups.detach_all();

    if names.is_empty() {
        #[cfg(debug_assertions)]
        eprintln!(
            "🌐 Radio-Browser DNS: reverse lookup returned no valid names, using de1 fallback"
        );
        return FALLBACK_HOST.to_string();
    }
    names.sort();

    // Pick a server randomly using the current time as a seed.
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    use std::time::SystemTime;
    let mut h = DefaultHasher::new();
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos()
        .hash(&mut h);
    let chosen = names[(h.finish() as usize) % names.len()].clone();
    #[cfg(debug_assertions)]
    eprintln!(
        "🌐 Radio-Browser DNS: discovered {:?}, using {}",
        names, chosen
    );
    chosen
}

/// Replace the `de1.api.radio-browser.info` placeholder in a URL with the
/// discovered host.  Safe to call even when the URL was already rewritten.
fn with_host(url: &str, host: &str) -> String {
    url.replace("de1.api.radio-browser.info", host)
}

async fn response_json_limited<T: DeserializeOwned>(
    response: reqwest::Response,
) -> Result<T, String> {
    let response = response.error_for_status().map_err(|err| err.to_string())?;
    if response
        .content_length()
        .is_some_and(|length| length > MAX_API_RESPONSE_BYTES as u64)
    {
        return Err("Response is too large".to_string());
    }
    let body = response.bytes().await.map_err(|err| err.to_string())?;
    if body.len() > MAX_API_RESPONSE_BYTES {
        return Err("Response is too large".to_string());
    }
    serde_json::from_slice(&body).map_err(|err| err.to_string())
}

fn fetch_data_async(
    url: String,
    is_stations: bool,
    target_view: ViewType,
    prefs: Arc<Mutex<AppPreferences>>,
    tx: async_channel::Sender<IncomingData>,
    is_load_more: bool,
) {
    fetch_data_async_internal(
        url,
        is_stations,
        target_view,
        prefs,
        tx,
        is_load_more,
        false,
    );
}

fn fetch_data_async_preserve_index(
    url: String,
    is_stations: bool,
    target_view: ViewType,
    prefs: Arc<Mutex<AppPreferences>>,
    tx: async_channel::Sender<IncomingData>,
    is_load_more: bool,
) {
    fetch_data_async_internal(url, is_stations, target_view, prefs, tx, is_load_more, true);
}

fn fetch_data_async_internal(
    mut url: String,
    is_stations: bool,
    target_view: ViewType,
    prefs: Arc<Mutex<AppPreferences>>,
    tx: async_channel::Sender<IncomingData>,
    is_load_more: bool,
    preserve_index: bool,
) {
    if SHUTTING_DOWN.load(Ordering::Acquire) {
        return;
    }
    let request_id = begin_request();
    if SHUTTING_DOWN.load(Ordering::Acquire) {
        return;
    }
    let mut cancel = request_cancel_receiver(request_id);
    let client = http_client();
    http_runtime().spawn(async move {
        if !is_active_request(request_id) {
            return;
        }

        let (hide_broken, prefer_high_bitrate) = {
            let config = prefs.lock().unwrap();
            (config.hide_broken, config.prefer_high_bitrate)
        };

        // A load-more request carries `offset=` and fetches only the next
        // page; `target_limit` is the total number of stations requested from
        // the server once that page is appended.
        let page_offset = if is_load_more {
            parse_url_offset(&url)
        } else {
            None
        };
        let (mut request_limit, mut target_limit) = match page_offset {
            Some(offset) => {
                let page = parse_url_limit(&url)
                    .unwrap_or(STATION_PAGE_SIZE)
                    .clamp(1, STATION_PAGE_SIZE);
                (page, clamp_station_limit(offset + page))
            }
            None => {
                let limit = if is_load_more || preserve_index {
                    clamp_station_limit(parse_url_limit(&url).unwrap_or(INITIAL_STATION_LIMIT))
                } else {
                    INITIAL_STATION_LIMIT
                };
                (limit, limit)
            }
        };

        if is_stations {
            url = set_url_limit(url, request_limit);
            let sep = |u: &str| if u.contains('?') { "&" } else { "?" };
            if hide_broken && !url.contains("lastcheckok=") {
                url.push_str(&format!("{}lastcheckok=1", sep(&url)));
            }
            if prefer_high_bitrate && !url.contains("order=") {
                url.push_str(&format!("{}order=bitrate&reverse=true", sep(&url)));
            }
        }

        // Station lists: serve from the cache (memory, then disk) before any
        // network work. A fresh entry needs no request at all; a stale one is
        // shown immediately and refreshed below.
        let cache_key = is_stations.then(|| station_cache::key_for(&url));
        let mut cached: Option<station_cache::CacheEntry> = None;
        if let Some(key) = cache_key.clone().filter(|_| !is_load_more) {
            let lookup = tokio::task::spawn_blocking(move || station_cache::get(&key));
            let entry = tokio::select! {
                entry = lookup => entry.ok().flatten(),
                _ = cancel.changed() => return,
            };
            if let Some(entry) = entry {
                if !preserve_index {
                    let fresh = entry.is_fresh();
                    if !send_network_data(
                        &tx,
                        &mut cancel,
                        request_id,
                        IncomingData::Stations {
                            request_id,
                            view: target_view.clone(),
                            stations: entry.stations.clone(),
                            sync_time: format!("{} (cached)", unix_time_local_string(entry.fetched_at)),
                            is_load_more: false,
                            limit: entry.limit,
                            preserve_index: false,
                            has_more_hint: entry.has_more,
                            keep_scroll: false,
                        },
                    )
                    .await
                    {
                        return;
                    }
                    if fresh {
                        return;
                    }
                    // Refresh the same number of stations the user had loaded.
                    request_limit = clamp_station_limit(entry.limit);
                    target_limit = request_limit;
                    url = set_url_limit(url, request_limit);
                }
                cached = Some(entry);
            }
        }
        // A background refresh (stale cache or auto-update) redraws in place
        // and keeps the selection, and is skipped if nothing changed.
        let is_refresh = cached.is_some();

        // Discover the best available server via DNS before building URLs.
        let host = tokio::select! {
            host = resolve_radio_browser_host() => host,
            _ = cancel.changed() => return,
        };
        url = with_host(&url, &host);
        if !is_active_request(request_id) {
            return;
        }

        let user_agent = "RustRadio/1.0";
        if is_stations {
            let stations_result = tokio::select! {
                result = client.get(&url).header("User-Agent", user_agent).send() => result,
                _ = cancel.changed() => return,
            };
            match stations_result {
                Ok(response) => {
                    let parsed = tokio::select! {
                        result = response_json_limited::<Vec<Station>>(response) => result,
                        _ = cancel.changed() => return,
                    };
                    match parsed {
                        Ok(stations) => {
                            let has_more_hint = stations.len() >= request_limit;
                            let stations: Vec<Station> =
                                stations
                                    .into_iter()
                                    .filter(|station| is_allowed_stream_url(&station.url_resolved))
                                    .take(MAX_STATION_LIMIT)
                                    .collect();
                            let stations = apply_bitrate_query_locally(&url, stations);
                            if let Some(key) = cache_key.clone() {
                                let stored = if is_load_more {
                                    station_cache::append(&key, &stations, target_limit, has_more_hint)
                                } else {
                                    station_cache::put(&key, stations.clone(), target_limit, has_more_hint);
                                    true
                                };
                                if stored {
                                    tokio::task::spawn_blocking(move || station_cache::persist(&key));
                                }
                            }
                            if let Some(previous) = &cached {
                                if previous.stations == stations && previous.has_more == has_more_hint {
                                    // Unchanged: the cache timestamp was renewed above;
                                    // nothing to redraw.
                                    return;
                                }
                            }
                            if !send_network_data(
                                &tx,
                                &mut cancel,
                                request_id,
                                IncomingData::Stations {
                                    request_id,
                                    view: target_view.clone(),
                                    stations,
                                    sync_time: "Unknown".to_string(),
                                    is_load_more,
                                    limit: target_limit,
                                    preserve_index: preserve_index || is_refresh,
                                    has_more_hint,
                                    keep_scroll: is_refresh,
                                },
                            )
                            .await
                            {
                                return;
                            }

                            // The optional stats request must not delay the station list.
                            let stats_client = client.clone();
                            let stats_url = format!("https://{}/json/stats", host);
                            let stats_tx = tx.clone();
                            let mut stats_cancel = cancel.clone();
                            tokio::spawn(async move {
                                let stats_result = tokio::select! {
                                    result = stats_client
                                        .get(&stats_url)
                                        .header("User-Agent", user_agent)
                                        .send() => result,
                                    _ = stats_cancel.changed() => return,
                                };
                                if let Ok(response) = stats_result {
                                    let parsed = tokio::select! {
                                        result = response_json_limited::<RadioBrowserStats>(response) => result,
                                        _ = stats_cancel.changed() => return,
                                    };
                                    if let Ok(stats) = parsed
                                    {
                                        let _ = send_network_data(
                                            &stats_tx,
                                            &mut stats_cancel,
                                            request_id,
                                            IncomingData::Stats {
                                                request_id,
                                                sync_time: stats.lastchangetime,
                                            },
                                        )
                                        .await;
                                    }
                                }
                            });
                        }
                        Err(_e) => {
                            #[cfg(debug_assertions)]
                            eprintln!("Failed to parse stations JSON: {}", _e);
                            if is_refresh {
                                return;
                            }
                            let _ = send_network_data(
                                &tx,
                                &mut cancel,
                                request_id,
                                IncomingData::Error {
                                    request_id,
                                    is_load_more,
                                    message: "Failed to parse station list".to_string(),
                                },
                            )
                            .await;
                        }
                    }
                }
                Err(_e) => {
                    #[cfg(debug_assertions)]
                    eprintln!("Failed to fetch stations: {}", _e);
                    if is_refresh {
                        return;
                    }
                    let _ = send_network_data(
                        &tx,
                        &mut cancel,
                        request_id,
                        IncomingData::Error {
                            request_id,
                            is_load_more,
                            message: "Failed to fetch stations".to_string(),
                        },
                    )
                    .await;
                }
            }
        } else {
            let categories_result = tokio::select! {
                result = client.get(&url).header("User-Agent", user_agent).send() => result,
                _ = cancel.changed() => return,
            };
            match categories_result {
                Ok(response) => match tokio::select! {
                    result = response_json_limited::<Vec<BrowseItem>>(response) => result,
                    _ = cancel.changed() => return,
                } {
                    Ok(cats) => {
                        let filtered = cats
                            .into_iter()
                            .filter(|c| !c.name.trim().is_empty())
                            .take(100)
                            .collect();
                        let _ = send_network_data(
                            &tx,
                            &mut cancel,
                            request_id,
                            IncomingData::Categories {
                                request_id,
                                view: target_view.clone(),
                                items: filtered,
                            },
                        )
                        .await;
                    }
                    Err(_e) => {
                        #[cfg(debug_assertions)]
                        eprintln!("Failed to parse categories JSON: {}", _e);
                        let _ = send_network_data(
                            &tx,
                            &mut cancel,
                            request_id,
                            IncomingData::Error {
                                request_id,
                                is_load_more: false,
                                message: "Failed to parse categories".to_string(),
                            },
                        )
                        .await;
                    }
                },
                Err(_e) => {
                    #[cfg(debug_assertions)]
                    eprintln!("Failed to fetch categories: {}", _e);
                    let _ = send_network_data(
                        &tx,
                        &mut cancel,
                        request_id,
                        IncomingData::Error {
                            request_id,
                            is_load_more: false,
                            message: "Failed to fetch categories".to_string(),
                        },
                    )
                    .await;
                }
            }
        }
    });
}

fn build_filtered_url(category: &CategoryFilter, bitrate: BitrateFilterSetting) -> String {
    let base_endpoint = match category {
        CategoryFilter::None => {
            "https://de1.api.radio-browser.info/json/stations/search?".to_string()
        }
        CategoryFilter::Tag(name) => format!(
            "https://de1.api.radio-browser.info/json/stations/search?tag={}&",
            urlencoding::encode(name)
        ),
        CategoryFilter::Language(name) => format!(
            "https://de1.api.radio-browser.info/json/stations/search?language={}&",
            urlencoding::encode(name)
        ),
        CategoryFilter::Country(name) => format!(
            "https://de1.api.radio-browser.info/json/stations/search?country={}&",
            urlencoding::encode(name)
        ),
    };

    match bitrate {
        BitrateFilterSetting::None => format!("{}order=bitrate&reverse=true", base_endpoint),
        BitrateFilterSetting::Low => {
            format!("{}bitrateMax=160&order=bitrate&reverse=true", base_endpoint)
        }
        BitrateFilterSetting::High => {
            format!("{}bitrateMin=192&order=bitrate&reverse=true", base_endpoint)
        }
        BitrateFilterSetting::Flac => {
            if let CategoryFilter::Tag(_) = category {
                format!("{}bitrateMin=500&order=bitrate&reverse=true", base_endpoint)
            } else {
                format!("{}tag=flac&order=bitrate&reverse=true", base_endpoint)
            }
        }
    }
}

fn filter_stations_local(
    mut stations: Vec<Station>,
    bitrate: BitrateFilterSetting,
) -> Vec<Station> {
    match bitrate {
        BitrateFilterSetting::None => return stations,
        BitrateFilterSetting::Low => {
            stations.retain(|s| s.bitrate.is_none_or(|b| b <= 160));
            stations.sort_by_key(|b| std::cmp::Reverse(b.bitrate.unwrap_or(0)));
        }
        BitrateFilterSetting::High => {
            stations.retain(|s| s.bitrate.is_none_or(|b| b >= 192));
            stations.sort_by_key(|b| std::cmp::Reverse(b.bitrate.unwrap_or(0)));
        }
        BitrateFilterSetting::Flac => {
            stations.retain(|s| {
                s.bitrate.is_some_and(|b| b >= 500) || s.tags.to_lowercase().contains("flac")
            });
            stations.sort_by_key(|b| std::cmp::Reverse(b.bitrate.unwrap_or(0)));
        }
    }
    stations
}

fn tag_to_icon(tag: &str) -> &'static str {
    let t = tag.to_lowercase();

    // --- Audio quality / format ---
    if t.contains("flac")
        || t.contains("lossless")
        || t.contains("hifi")
        || t.contains("hi-fi")
        || t.contains("audiophile")
        || t.contains("320")
        || t.contains("high quality")
    {
        return "audio-headphones-symbolic";
    }

    // --- Genres: electronic / club ---
    if t.contains("electron")
        || t.contains("techno")
        || t.contains("house")
        || t.contains("trance")
        || t.contains("edm")
        || t.contains("dance")
        || t.contains("dubstep")
        || t.contains("drum and bass")
        || t.contains("drum & bass")
        || t.contains("dnb")
        || t.contains("d&b")
        || t.contains("jungle")
        || t.contains("breakbeat")
        || t.contains("hardcore")
        || t.contains("rave")
    {
        return "utilities-terminal-symbolic";
    }

    // --- Genres: hip-hop / urban ---
    if t.contains("hip-hop")
        || t.contains("hiphop")
        || t.contains("hip hop")
        || t.contains("rap")
        || t.contains("trap")
        || t.contains("grime")
        || t.contains("drill")
    {
        return "microphone-sensitivity-high-symbolic";
    }

    // --- Genres: rock / metal ---
    if t.contains("metal")
        || t.contains("punk")
        || t.contains("grunge")
        || t.contains("hardcore")
        || t.contains("thrash")
        || t.contains("doom")
        || t.contains("black metal")
        || t.contains("heavy")
    {
        return "media-optical-cd-audio-symbolic";
    }
    if t.contains("rock") {
        return "media-optical-cd-audio-symbolic";
    }

    // --- Genres: classical / orchestral ---
    if t.contains("classical")
        || t.contains("orchestra")
        || t.contains("symphony")
        || t.contains("opera")
        || t.contains("chamber")
        || t.contains("baroque")
        || t.contains("choral")
        || t.contains("philharmonic")
    {
        return "emblem-music-symbolic";
    }

    // --- Genres: jazz / swing / big band ---
    if t.contains("jazz")
        || t.contains("swing")
        || t.contains("big band")
        || t.contains("bebop")
        || t.contains("dixieland")
        || t.contains("blues")
        || t.contains("boogie")
    {
        return "audio-x-generic-symbolic";
    }

    // --- Genres: soul / R&B / funk ---
    if t.contains("soul")
        || t.contains("funk")
        || t.contains("rnb")
        || t.contains("r&b")
        || t.contains("motown")
        || t.contains("gospel")
        || t.contains("rhythm")
    {
        return "emblem-favorite-symbolic";
    }

    // --- Genres: ambient / chill / new age ---
    if t.contains("ambient")
        || t.contains("chill")
        || t.contains("lofi")
        || t.contains("lo-fi")
        || t.contains("relax")
        || t.contains("meditation")
        || t.contains("sleep")
        || t.contains("new age")
        || t.contains("newage")
        || t.contains("nature")
        || t.contains("spa")
        || t.contains("healing")
    {
        return "weather-clear-night-symbolic";
    }

    // --- Genres: pop ---
    if t.contains("pop") {
        return "starred-symbolic";
    }

    // --- Genres: country / folk ---
    if t.contains("country")
        || t.contains("folk")
        || t.contains("bluegrass")
        || t.contains("acoustic")
        || t.contains("singer-songwriter")
        || t.contains("americana")
    {
        return "find-location-symbolic";
    }

    // --- Genres: Latin / tropical ---
    if t.contains("latin")
        || t.contains("salsa")
        || t.contains("reggaeton")
        || t.contains("cumbia")
        || t.contains("bachata")
        || t.contains("bossa")
        || t.contains("samba")
        || t.contains("merengue")
        || t.contains("tango")
        || t.contains("flamenco")
        || t.contains("fado")
    {
        return "emoji-activities-symbolic";
    }

    // --- Genres: reggae / ska ---
    if t.contains("reggae") || t.contains("ska") || t.contains("dub") || t.contains("dancehall") {
        return "weather-clear-symbolic";
    }

    // --- Genres: world / cultural ---
    if t.contains("world")
        || t.contains("international")
        || t.contains("global")
        || t.contains("multicultural")
    {
        return "applications-internet-symbolic";
    }

    // --- Genres: instrumental ---
    if t.contains("instrumental")
        || t.contains("piano")
        || t.contains("guitar")
        || t.contains("violin")
        || t.contains("cello")
        || t.contains("trumpet")
        || t.contains("saxophone")
        || t.contains("orchestra")
    {
        return "emblem-music-symbolic";
    }

    // --- Talk / news / speech ---
    if t.contains("news")
        || t.contains("talk")
        || t.contains("speech")
        || t.contains("podcast")
        || t.contains("interview")
        || t.contains("debate")
        || t.contains("current affairs")
    {
        return "dialog-information-symbolic";
    }

    // --- Sports ---
    if t.contains("sport")
        || t.contains("football")
        || t.contains("soccer")
        || t.contains("basketball")
        || t.contains("baseball")
        || t.contains("cricket")
        || t.contains("rugby")
        || t.contains("tennis")
    {
        return "applications-games-symbolic";
    }

    // --- Religious ---
    if t.contains("christian")
        || t.contains("gospel")
        || t.contains("spiritual")
        || t.contains("religion")
        || t.contains("worship")
        || t.contains("church")
        || t.contains("catholic")
        || t.contains("orthodox")
        || t.contains("islamic")
        || t.contains("quran")
        || t.contains("jewish")
        || t.contains("buddhist")
    {
        return "emblem-default-symbolic";
    }

    // --- Children / education ---
    if t.contains("child")
        || t.contains("kids")
        || t.contains("school")
        || t.contains("education")
        || t.contains("learn")
        || t.contains("student")
        || t.contains("college")
        || t.contains("university")
        || t.contains("campus")
    {
        return "face-smile-symbolic";
    }

    // --- Comedy / entertainment ---
    if t.contains("comedy") || t.contains("humor") || t.contains("humour") || t.contains("satire") {
        return "face-laugh-symbolic";
    }

    // --- Decades / era ---
    if t.contains("50s")
        || t.contains("60s")
        || t.contains("70s")
        || t.contains("80s")
        || t.contains("90s")
        || t.contains("2000s")
        || t.contains("oldies")
        || t.contains("vintage")
        || t.contains("retro")
        || t.contains("golden")
        || t.contains("classic hits")
        || t.contains("throwback")
    {
        return "document-open-recent-symbolic";
    }

    // --- Indie / alternative ---
    if t.contains("indie") || t.contains("alternative") {
        return "media-record-symbolic";
    }

    // --- Live / events ---
    if t.contains("live") || t.contains("concert") || t.contains("festival") {
        return "media-record-symbolic";
    }

    // --- Charts / variety ---
    if t.contains("hits")
        || t.contains("chart")
        || t.contains("top 40")
        || t.contains("variety")
        || t.contains("mix")
    {
        return "view-list-symbolic";
    }

    // --- Seasonal ---
    if t.contains("christmas")
        || t.contains("holiday")
        || t.contains("seasonal")
        || t.contains("xmas")
    {
        return "starred-symbolic";
    }

    // --- Regional: East Asian ---
    if t.contains("japanese") || t.contains("j-pop") || t.contains("j-rock") || t.contains("anime")
    {
        return "applications-internet-symbolic";
    }
    if t.contains("korean") || t.contains("k-pop") || t.contains("k-drama") {
        return "applications-internet-symbolic";
    }
    if t.contains("chinese")
        || t.contains("mandarin")
        || t.contains("cantonese")
        || t.contains("taiwanese")
    {
        return "applications-internet-symbolic";
    }

    // --- Regional: South / Southeast Asian ---
    if t.contains("indian")
        || t.contains("hindi")
        || t.contains("bollywood")
        || t.contains("bhangra")
        || t.contains("carnatic")
        || t.contains("filmi")
    {
        return "emoji-activities-symbolic";
    }
    if t.contains("bengali")
        || t.contains("tamil")
        || t.contains("telugu")
        || t.contains("malayalam")
        || t.contains("punjabi")
    {
        return "emoji-activities-symbolic";
    }
    if t.contains("thai")
        || t.contains("vietnamese")
        || t.contains("indonesian")
        || t.contains("philippine")
        || t.contains("malay")
    {
        return "emoji-activities-symbolic";
    }

    // --- Regional: Middle Eastern ---
    if t.contains("arabic")
        || t.contains("arab")
        || t.contains("khaleeji")
        || t.contains("middle east")
        || t.contains("levant")
    {
        return "weather-clear-symbolic";
    }
    if t.contains("persian") || t.contains("farsi") || t.contains("iranian") {
        return "weather-clear-symbolic";
    }
    if t.contains("turkish") || t.contains("turk") {
        return "weather-clear-symbolic";
    }
    if t.contains("kurdish") {
        return "weather-clear-symbolic";
    }

    // --- Regional: African ---
    if t.contains("african")
        || t.contains("afro")
        || t.contains("afrobeat")
        || t.contains("afropop")
        || t.contains("highlife")
        || t.contains("juju")
        || t.contains("mbalax")
    {
        return "find-location-symbolic";
    }

    // --- Regional: Eastern European / Slavic ---
    if t.contains("russian")
        || t.contains("ukraine")
        || t.contains("ukrainian")
        || t.contains("polish")
        || t.contains("czech")
        || t.contains("slovak")
        || t.contains("balkan")
        || t.contains("serbian")
        || t.contains("croatian")
        || t.contains("bulgarian")
        || t.contains("romanian")
    {
        return "emblem-music-symbolic";
    }

    // --- Regional: Western / Northern European ---
    if t.contains("french") || t.contains("chanson") {
        return "emblem-favorite-symbolic";
    }
    if t.contains("german")
        || t.contains("deutsch")
        || t.contains("schlager")
        || t.contains("volksmusik")
    {
        return "emblem-music-symbolic";
    }
    if t.contains("spanish") || t.contains("espanol") {
        return "emoji-activities-symbolic";
    }
    if t.contains("italian") || t.contains("italiano") || t.contains("cantautore") {
        return "emblem-favorite-symbolic";
    }
    if t.contains("greek") || t.contains("laika") || t.contains("rebetiko") {
        return "weather-clear-symbolic";
    }
    if t.contains("celtic")
        || t.contains("irish")
        || t.contains("scottish")
        || t.contains("welsh")
        || t.contains("breton")
    {
        return "find-location-symbolic";
    }
    if t.contains("nordic")
        || t.contains("scandinavian")
        || t.contains("swedish")
        || t.contains("norwegian")
        || t.contains("danish")
        || t.contains("finnish")
    {
        return "weather-clear-night-symbolic";
    }

    // --- Missing genre gaps ---
    if t.contains("soundtrack")
        || t.contains("film")
        || t.contains("movie")
        || t.contains("cinema")
        || t.contains("score")
    {
        return "video-display-symbolic";
    }
    if t.contains("industrial") || t.contains("noise") {
        return "utilities-terminal-symbolic";
    }
    if t.contains("new wave")
        || t.contains("post-punk")
        || t.contains("prog")
        || t.contains("progressive")
    {
        return "media-record-symbolic";
    }
    if t.contains("lounge")
        || t.contains("easy listening")
        || t.contains("beautiful music")
        || t.contains("adult contemporary")
    {
        return "starred-symbolic";
    }
    if t.contains("roots") {
        return "find-location-symbolic";
    }
    if t.contains("jazz fusion") {
        return "audio-x-generic-symbolic";
    }
    if t.contains("trip-hop")
        || t.contains("triphop")
        || t.contains("downtempo")
        || t.contains("down tempo")
    {
        return "weather-clear-night-symbolic";
    }
    if t.contains("a cappella")
        || t.contains("acappella")
        || t.contains("vocal")
        || t.contains("choir")
    {
        return "audio-input-microphone-symbolic";
    }
    if t.contains("karaoke") || t.contains("instrumentals") {
        return "media-record-symbolic";
    }
    if t.contains("audiobook") || t.contains("audio book") || t.contains("audio drama") {
        return "dialog-information-symbolic";
    }
    if t.contains("public")
        || t.contains("npr")
        || t.contains("community")
        || t.contains("traffic")
        || t.contains("weather")
        || t.contains("emergency")
    {
        return "dialog-information-symbolic";
    }
    if t.contains("top") || t.contains("greatest") || t.contains("request") || t.contains("today") {
        return "starred-symbolic";
    }
    if t.contains("urban") || t.contains("contemporary") {
        return "emblem-favorite-symbolic";
    }
    if t.contains("political") || t.contains("politics") || t.contains("government") {
        return "dialog-information-symbolic";
    }

    // --- General music / radio (last resort before fallback) ---
    if t.contains("music") || t.contains("musica") || t.contains("musik") {
        return "audio-x-generic-symbolic";
    }
    if t.contains("radio") || t.contains("broadcast") || t.contains("stream") {
        return "audio-input-microphone-symbolic";
    }

    "audio-x-generic-symbolic"
}

fn build_ui(app: &Application) {
    SHUTTING_DOWN.store(false, Ordering::Release);
    // Start server discovery + connection setup while the UI is being built.
    prewarm_network();
    adw::init().unwrap();

    let provider = gtk::CssProvider::new();
    provider.load_from_data(
        "
        list row:selected { background-color: alpha(currentColor, 0.12); }
    ",
    );
    gtk::style_context_add_provider_for_display(
        &gtk::gdk::Display::default()
            .expect("Could not connect to system display context tracker."),
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );

    // --- REHYDRATE FROM DISK ---
    let saved_data = AppState::load_data();

    let current_metadata = Arc::new(Mutex::new(AudioMetadata {
        bitrate: None,
        sample_rate: None,
        stream_title: None,
        codec: None,
        bit_depth: None,
        buffering_percent: None,
    }));
    let player_state = Arc::new(Mutex::new(PlayerState {
        pipeline: None,
        bus_guard: None,
        generation: 0,
        is_paused: false,
        current_metadata,
        currently_playing_station: None,
        playlist_view: None,
    }));

    let app_prefs = Arc::new(Mutex::new(AppPreferences {
        prefer_high_bitrate: saved_data.prefer_high_bitrate,
        hide_broken: saved_data.hide_broken,
        auto_update: saved_data.auto_update,
        analyzer_type: match saved_data.analyzer_type.as_str() {
            "Bars" => AnalyzerType::Bars,
            "Wave" => AnalyzerType::Wave,
            "DigitalVuBlocks" => AnalyzerType::DigitalVuBlocks,
            _ => AnalyzerType::BlocksWithHold,
        },
        custom_tags: saved_data.custom_tags,
        audio_sink: saved_data.audio_sink,
        disable_video_streams: saved_data.disable_video_streams,
        disable_audio_buffer: saved_data.disable_audio_buffer,
        volume: saved_data.volume,
    }));

    let last_sync_ts = if saved_data.last_sync_timestamp.is_empty() {
        "Never Gathered".to_string()
    } else {
        saved_data.last_sync_timestamp.clone()
    };

    let ui_state = Arc::new(Mutex::new(UIState {
        current_view: ViewType::Tags,
        previous_view: None,
        previous_url: String::new(),
        categories: Vec::new(),
        stations: Vec::new(),
        favorites: saved_data.favorites,
        recent_stations: saved_data.recent_stations,
        playlists: saved_data.playlists,
        current_index: None,
        active_category: CategoryFilter::None,
        active_bitrate: match saved_data.active_bitrate.as_str() {
            "Low"  => BitrateFilterSetting::Low,
            "High" => BitrateFilterSetting::High,
            "Flac" => BitrateFilterSetting::Flac,
            _      => BitrateFilterSetting::None,
        },
        last_fetched_url: "https://de1.api.radio-browser.info/json/tags?order=stationcount&reverse=true&hidebroken=true".to_string(),
        last_update_timestamp: last_sync_ts.clone(),
        total_elements_fetched: saved_data.last_stations_loaded,
        last_stations_updated_count: saved_data.last_stations_loaded,
        station_limit: INITIAL_STATION_LIMIT,
        station_has_more_hint: false,
        cached_tags: None,
        cached_languages: None,
        cached_countries: None,
    }));

    let app_state_manager = Arc::new(AppState {
        ui: ui_state.clone(),
        prefs: app_prefs.clone(),
    });

    let content_box = Box::new(Orientation::Vertical, 0);

    // --- Header Bar & Context Menus ---
    let header_bar = HeaderBar::new();
    let menu_btn = MenuButton::builder()
        .icon_name("open-menu-symbolic")
        .build();
    let menu_popover = Popover::new();
    let menu_box = Box::new(Orientation::Vertical, 6);

    let settings_btn = Button::builder()
        .label("Preferences")
        .has_frame(false)
        .build();
    menu_box.append(&settings_btn);
    menu_popover.set_child(Some(&menu_box));
    menu_btn.set_popover(Some(&menu_popover));

    // Search entry removed from headerbar and built for the main view
    let search_entry = SearchEntry::builder()
        .placeholder_text("Search stations…")
        .width_request(300)
        .margin_start(16)
        .margin_end(16)
        .margin_top(8)
        .margin_bottom(4)
        .build();
    search_entry.set_tooltip_text(Some("Search global radio stations"));

    header_bar.pack_end(&menu_btn);
    content_box.append(&header_bar);

    // --- Left Navigation Sidebar ---
    let sidebar = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(0)
        .margin_top(8)
        .margin_bottom(8)
        .build();

    let make_nav_btn = |icon: &str, label_text: &str| -> Button {
        let hbox = Box::builder()
            .orientation(Orientation::Horizontal)
            .spacing(8)
            .margin_start(10)
            .halign(gtk::Align::Start)
            .build();
        let icon_img = gtk::Image::from_icon_name(icon);
        icon_img.set_icon_size(gtk::IconSize::Normal);
        let lbl = Label::builder().label(label_text).build();
        hbox.append(&icon_img);
        hbox.append(&lbl);
        Button::builder()
            .child(&hbox)
            .css_classes(vec!["flat".to_string()])
            .hexpand(true)
            .margin_top(2)
            .margin_bottom(2)
            .build()
    };

    let tag_btn = make_nav_btn("media-optical-cd-audio-symbolic", "Browse By Tag");
    let votes_sidebar_btn = make_nav_btn("view-sort-descending-symbolic", "Top Voted");
    let lang_btn = make_nav_btn("preferences-desktop-locale-symbolic", "Languages");
    let country_btn = make_nav_btn("find-location-symbolic", "Countries");
    let fav_menu_btn = make_nav_btn("starred-symbolic", "Favourites");
    let recent_nav_btn = make_nav_btn("document-open-recent-symbolic", "Recent");
    let playlists_nav_btn = make_nav_btn("view-list-symbolic", "Playlists");
    let custom_tags_btn = make_nav_btn("bookmark-new-symbolic", "Custom Tags");

    sidebar.append(&tag_btn);
    sidebar.append(&votes_sidebar_btn);
    sidebar.append(&lang_btn);
    sidebar.append(&country_btn);
    sidebar.append(&fav_menu_btn);
    sidebar.append(&recent_nav_btn);
    sidebar.append(&playlists_nav_btn);
    sidebar.append(&custom_tags_btn);

    let custom_tag_buttons_box = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(0)
        .visible(false)
        .build();
    sidebar.append(&custom_tag_buttons_box);

    let app_name_lbl = Label::builder()
        .label("Rust Radio")
        .css_classes(vec!["title-4".to_string()])
        .halign(gtk::Align::Start)
        .margin_start(12)
        .margin_bottom(4)
        .build();
    sidebar.prepend(&app_name_lbl);

    // Right content area
    let right_content = Box::builder()
        .orientation(Orientation::Vertical)
        .hexpand(true)
        .build();

    let sidebar_scroll = ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .vexpand(true)
        .child(&sidebar)
        .width_request(160)
        .build();

    let body_paned = Paned::builder()
        .orientation(Orientation::Horizontal)
        .start_child(&sidebar_scroll)
        .end_child(&right_content)
        .position(160)
        .vexpand(true)
        .shrink_start_child(true)
        .shrink_end_child(false)
        .build();
    content_box.append(&body_paned);

    // --- Filter Shelves (compact icon buttons) ---
    let filter_shelf = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(0)
        .margin_bottom(6)
        .margin_top(6)
        .margin_start(12)
        .margin_end(12)
        .build();

    let row2_category_box = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(12)
        .halign(gtk::Align::Fill)
        .build();

    let sort_area = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(0)
        .halign(gtk::Align::Start)
        .build();
    let sort_row = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(4)
        .halign(gtk::Align::Start)
        .build();
    let sort_desc = Label::builder()
        .label("Sort:")
        .css_classes(vec!["caption".to_string(), "dim-label".to_string()])
        .build();
    sort_row.append(&sort_desc);

    let make_sort_btn = |icon: &str, label: &str| -> Button {
        let hbox = Box::builder()
            .orientation(Orientation::Horizontal)
            .spacing(4)
            .build();
        let img = gtk::Image::from_icon_name(icon);
        img.set_icon_size(gtk::IconSize::Inherit);
        let lbl = Label::builder()
            .label(label)
            .css_classes(vec!["caption".to_string()])
            .build();
        hbox.append(&img);
        hbox.append(&lbl);
        Button::builder()
            .child(&hbox)
            .css_classes(vec!["flat".to_string()])
            .build()
    };
    let sort_abc_btn = make_sort_btn("format-justify-left-symbolic", "A–Z");
    sort_row.append(&sort_abc_btn);

    let br_area = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(4)
        .halign(gtk::Align::Start)
        .build();
    let br_desc = Label::builder()
        .label("Bitrate:")
        .css_classes(vec!["caption".to_string(), "dim-label".to_string()])
        .build();
    br_area.append(&br_desc);

    let bitrate_menu_btn = MenuButton::builder()
        .label(match ui_state.lock().unwrap().active_bitrate {
            BitrateFilterSetting::None => "⊟ Bitrate",
            BitrateFilterSetting::Low => "⊟ Low ≤160",
            BitrateFilterSetting::High => "⊟ High ≥192",
            BitrateFilterSetting::Flac => "⊟ FLAC",
        })
        .css_classes(vec!["flat".to_string()])
        .build();
    br_area.append(&bitrate_menu_btn);

    let bitrate_popover = Popover::new();
    let bitrate_options_box = Box::new(Orientation::Vertical, 4);
    let any_br_btn = Button::builder()
        .label("Any Bitrate")
        .has_frame(false)
        .build();
    let low_br_btn = Button::builder()
        .label("Low (≤160 kbps)")
        .has_frame(false)
        .build();
    let high_br_btn = Button::builder()
        .label("High (≥192 kbps)")
        .has_frame(false)
        .build();
    let flac_br_btn = Button::builder()
        .label("Flac (Lossless)")
        .has_frame(false)
        .build();
    bitrate_options_box.append(&any_br_btn);
    bitrate_options_box.append(&low_br_btn);
    bitrate_options_box.append(&high_br_btn);
    bitrate_options_box.append(&flac_br_btn);
    bitrate_popover.set_child(Some(&bitrate_options_box));
    bitrate_menu_btn.set_popover(Some(&bitrate_popover));

    let combined_row = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(12)
        .halign(gtk::Align::Start)
        .build();
    combined_row.append(&sort_row);
    combined_row.append(&br_area);

    let back_btn = make_sort_btn("go-previous-symbolic", "Back");
    sort_area.append(&back_btn);

    row2_category_box.append(&sort_area);

    let header_search = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(8)
        .margin_start(12)
        .margin_end(12)
        .margin_top(6)
        .margin_bottom(4)
        .halign(gtk::Align::Start)
        .build();
    header_search.append(&search_entry);
    header_search.append(&combined_row);
    right_content.append(&header_search);

    let shelf_spacer = Box::new(Orientation::Horizontal, 0);
    shelf_spacer.set_hexpand(true);
    row2_category_box.append(&shelf_spacer);

    let import_playlist_btn = Button::builder()
        .label("Import Playlists")
        .css_classes(vec!["flat".to_string()])
        .halign(gtk::Align::End)
        .build();
    import_playlist_btn.set_visible(false);
    row2_category_box.append(&import_playlist_btn);
    let import_btn_clone = import_playlist_btn.clone();

    let clear_recents_btn = Button::builder()
        .label("Clear Recents")
        .css_classes(vec!["flat".to_string()])
        .halign(gtk::Align::End)
        .build();
    clear_recents_btn.set_visible(false);
    row2_category_box.append(&clear_recents_btn);

    filter_shelf.append(&row2_category_box);
    right_content.append(&filter_shelf);

    let counter_panel = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(12)
        .halign(gtk::Align::End)
        .margin_end(10)
        .build();

    let stations_count_label = Label::builder()
        .label("0 entries displayed")
        .css_classes(vec!["caption".to_string(), "dim-label".to_string()])
        .build();
    let last_updated_label = Label::builder()
        .label("Last update: Never")
        .css_classes(vec!["caption".to_string(), "dim-label".to_string()])
        .build();
    {
        let u = ui_state.lock().unwrap();
        if u.last_update_timestamp != "Never Gathered" {
            last_updated_label.set_text(&format!("DB synced: {}", u.last_update_timestamp));
        }
    }

    counter_panel.append(&stations_count_label);
    counter_panel.append(&last_updated_label);

    let now_browsing_label = Label::builder()
        .label("")
        .css_classes(vec!["title-4".to_string()])
        .halign(gtk::Align::Start)
        .hexpand(true)
        .margin_start(12)
        .build();

    let browsing_row = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(12)
        .margin_top(4)
        .margin_bottom(2)
        .build();
    browsing_row.append(&now_browsing_label);
    browsing_row.append(&counter_panel);
    right_content.append(&browsing_row);

    let list_box = ListBox::builder()
        .margin_top(6)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .css_classes(vec!["boxed-list".to_string()])
        .build();

    let scrolled_window = ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .vexpand(true)
        .child(&list_box)
        .build();

    right_content.append(&scrolled_window);

    let prev_btn = Button::builder()
        .icon_name("media-skip-backward-symbolic")
        .css_classes(vec!["circular".to_string()])
        .build();
    let play_pause_btn = Button::builder()
        .icon_name("media-playback-start-symbolic")
        .css_classes(vec!["circular".to_string(), "suggested-action".to_string()])
        .build();
    let next_btn = Button::builder()
        .icon_name("media-skip-forward-symbolic")
        .css_classes(vec!["circular".to_string()])
        .build();

    let now_playing_title = Label::builder()
        .label("No Station Selected")
        .css_classes(vec!["title-2".to_string()])
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .max_width_chars(24)
        .halign(gtk::Align::Start)
        .build();
    let now_playing_subtitle = Label::builder()
        .label("Select a station to start listening")
        .css_classes(vec!["body".to_string(), "dim-label".to_string()])
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .halign(gtk::Align::Start)
        .build();
    let stream_meta_label = Label::builder()
        .label("")
        .css_classes(vec!["body".to_string(), "dim-label".to_string()])
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .max_width_chars(40)
        .valign(gtk::Align::Center)
        .build();
    let stream_info_label = Label::builder()
        .label("")
        .css_classes(vec!["body".to_string(), "dim-label".to_string()])
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .max_width_chars(40)
        .valign(gtk::Align::Center)
        .build();
    let time_label = Label::builder()
        .label("0:00")
        .css_classes(vec![
            "title-2".to_string(),
            "numeric".to_string(),
            "dim-label".to_string(),
        ])
        .valign(gtk::Align::Center)
        .halign(gtk::Align::Start)
        .build();
    let drawing_analyzer = DrawingArea::builder()
        .content_height(60)
        .content_width(320)
        .margin_top(12)
        .margin_bottom(12)
        .halign(gtk::Align::Center)
        .build();

    let wave_heights = Arc::new(Mutex::new(vec![4.0; 64]));
    let peak_heights = Arc::new(Mutex::new(vec![4.0; 64]));
    let peak_decay_counters = Arc::new(Mutex::new(vec![0u8; 64]));

    let wave_heights_render = wave_heights.clone();
    let peak_heights_render = peak_heights.clone();
    let app_prefs_render = app_prefs.clone();

    drawing_analyzer.set_draw_func(move |widget, cr, width, height| {
        let bars = wave_heights_render.lock().unwrap();
        let peaks = peak_heights_render.lock().unwrap();
        let total_bars = bars.len();
        let current_pref = app_prefs_render.lock().unwrap();

        let ctx = widget.style_context();
        let accent = ctx
            .lookup_color("accent_bg_color")
            .or_else(|| ctx.lookup_color("theme_selected_bg_color"))
            .unwrap_or(gtk::gdk::RGBA::new(0.2, 0.44, 0.85, 0.95));
        let (ar, ag, ab) = (
            accent.red() as f64,
            accent.green() as f64,
            accent.blue() as f64,
        );
        cr.set_source_rgba(ar, ag, ab, 0.95);
        let accent_dim = gtk::gdk::RGBA::new(accent.red(), accent.green(), accent.blue(), 0.40);

        let spacing = 2.0;
        let bar_width = (width as f64 - (spacing * (total_bars - 1) as f64)) / total_bars as f64;

        match current_pref.analyzer_type {
            AnalyzerType::Bars => {
                for (i, &val) in bars.iter().enumerate() {
                    let x = i as f64 * (bar_width + spacing);
                    let bar_h = (val / 50.0) * height as f64;
                    let y = (height as f64 - bar_h) / 2.0;

                    cr.new_sub_path();
                    cr.arc(
                        x + bar_width / 2.0,
                        y,
                        bar_width / 2.0,
                        std::f64::consts::PI,
                        0.0,
                    );
                    cr.arc(
                        x + bar_width / 2.0,
                        y + bar_h,
                        bar_width / 2.0,
                        0.0,
                        std::f64::consts::PI,
                    );
                    cr.close_path();
                    let _ = cr.fill();
                }
            }
            AnalyzerType::Wave => {
                cr.set_line_width(2.0);
                cr.move_to(0.0, height as f64 / 2.0);
                for (i, &val) in bars.iter().enumerate() {
                    let x = i as f64 * (bar_width + spacing) + (bar_width / 2.0);
                    let offset = (val - 20.0) * 0.6;
                    let y = (height as f64 / 2.0) + offset;
                    cr.line_to(x, y);
                }
                cr.line_to(width as f64, height as f64 / 2.0);
                let _ = cr.stroke();
            }
            AnalyzerType::BlocksWithHold => {
                let dot_height = 2.0;
                let dot_gap = 1.0;
                let max_dots = (height as f64 / (dot_height + dot_gap)) as usize;
                if max_dots == 0 {
                    return;
                }

                for (i, &val) in bars.iter().enumerate() {
                    let x = i as f64 * (bar_width + spacing);
                    let active_dots =
                        (((val / 50.0) * height as f64) / (dot_height + dot_gap)) as usize;
                    let active_dots = active_dots.min(max_dots).max(1);

                    for j in 0..active_dots {
                        let y = height as f64 - (j as f64 * (dot_height + dot_gap)) - dot_height;
                        cr.rectangle(x, y, bar_width, dot_height);
                        let _ = cr.fill();
                    }

                    let peak_dot_idx =
                        (((peaks[i] / 50.0) * height as f64) / (dot_height + dot_gap)) as usize;
                    let peak_dot_idx = peak_dot_idx.min(max_dots - 1);
                    let p_y =
                        height as f64 - (peak_dot_idx as f64 * (dot_height + dot_gap)) - dot_height;

                    let (adr, adg, adb) = (
                        accent_dim.red() as f64,
                        accent_dim.green() as f64,
                        accent_dim.blue() as f64,
                    );
                    let ada = accent_dim.alpha() as f64;
                    cr.set_source_rgba(adr, adg, adb, ada);
                    cr.rectangle(x, p_y, bar_width, dot_height);
                    let _ = cr.fill();

                    cr.set_source_rgba(ar, ag, ab, 0.95);
                }
            }
            AnalyzerType::DigitalVuBlocks => {
                let dot_height = 2.0;
                let dot_gap = 1.0;
                let max_dots = (height as f64 / (dot_height + dot_gap)) as usize;
                if max_dots == 0 {
                    return;
                }

                for (i, &val) in bars.iter().enumerate() {
                    let x = i as f64 * (bar_width + spacing);
                    let active_dots =
                        (((val / 50.0) * height as f64) / (dot_height + dot_gap)) as usize;
                    let active_dots = active_dots.min(max_dots).max(1);

                    for j in 0..active_dots {
                        let y = height as f64 - (j as f64 * (dot_height + dot_gap)) - dot_height;
                        cr.rectangle(x, y, bar_width, dot_height);
                        let _ = cr.fill();
                    }
                }
            }
        }
    });

    let bottom_bar_sep = gtk::Separator::new(Orientation::Horizontal);
    content_box.append(&bottom_bar_sep);

    let bottom_bar = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(0)
        .margin_top(0)
        .margin_bottom(0)
        .height_request(88)
        .hexpand(true)
        .build();
    content_box.append(&bottom_bar);

    let now_playing_left = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(10)
        .halign(gtk::Align::Start)
        .valign(gtk::Align::Center)
        .margin_start(16)
        .margin_end(8)
        .build();
    let now_playing_flag_lbl = Label::builder()
        .label("")
        .css_classes(vec!["title-1".to_string()])
        .valign(gtk::Align::Center)
        .visible(false)
        .build();
    now_playing_left.append(&now_playing_flag_lbl);
    let now_playing_text_box = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(2)
        .valign(gtk::Align::Center)
        .build();
    let title_row = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(8)
        .halign(gtk::Align::Start)
        .valign(gtk::Align::Center)
        .build();
    title_row.append(&now_playing_title);
    now_playing_text_box.append(&title_row);
    stream_meta_label.set_halign(gtk::Align::Start);
    now_playing_text_box.append(&stream_meta_label);
    stream_info_label.set_halign(gtk::Align::Start);
    now_playing_text_box.append(&stream_info_label);
    now_playing_left.append(&now_playing_text_box);

    let fav_toggle_btn = Button::builder()
        .icon_name("non-starred-symbolic")
        .css_classes(vec!["flat".to_string()])
        .valign(gtk::Align::Center)
        .build();

    let volume_icon = gtk::Image::from_icon_name("audio-volume-high-symbolic");
    volume_icon.set_valign(gtk::Align::Center);

    let volume_scale = Scale::builder()
        .orientation(Orientation::Horizontal)
        .adjustment(&gtk::Adjustment::new(1.0, 0.0, 1.0, 0.05, 0.1, 0.0))
        .width_request(100)
        .valign(gtk::Align::Center)
        .draw_value(false)
        .build();
    let saved_volume = app_prefs.lock().unwrap().volume;
    volume_scale.set_value(saved_volume);

    let centre_controls = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(8)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .build();
    centre_controls.append(&time_label);
    centre_controls.append(&prev_btn);
    centre_controls.append(&play_pause_btn);
    centre_controls.append(&next_btn);
    centre_controls.append(&fav_toggle_btn);
    centre_controls.append(&volume_icon);
    centre_controls.append(&volume_scale);

    let centre_box = gtk::CenterBox::builder()
        .hexpand(true)
        .valign(gtk::Align::Fill)
        .build();
    centre_box.set_start_widget(Some(&now_playing_left));
    centre_box.set_center_widget(Some(&centre_controls));
    bottom_bar.append(&centre_box);

    let now_playing_right = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(8)
        .halign(gtk::Align::End)
        .valign(gtk::Align::Center)
        .margin_start(32)
        .margin_end(16)
        .build();
    now_playing_right.append(&drawing_analyzer);
    centre_box.set_end_widget(Some(&now_playing_right));

    let (tx, rx) = async_channel::bounded::<IncomingData>(64);

    let window_parent_hook = content_box.clone();
    let prefs_window_ref = app_prefs.clone();
    let ui_telemetry_ref = ui_state.clone();
    let tx_sync_prefs = tx.clone();
    let prefs_sync_prefs = app_prefs.clone();
    let ui_sync_prefs = ui_state.clone();
    let sync_label_prefs = last_updated_label.clone();
    let ui_backup_ref = ui_state.clone();
    let prefs_backup_ref = app_prefs.clone();
    let ui_restore_ref = ui_state.clone();
    let prefs_restore_ref = app_prefs.clone();
    let app_state_restore_ref = app_state_manager.clone();
    let tx_restore_ref = tx.clone();
    let custom_tags_restore_ref = custom_tag_buttons_box.clone();
    let app_state_prefs_close_ref = app_state_manager.clone();
    let stats_label_holder = make_stats_label();
    let stats_label_btn = stats_label_holder.clone();
    let stats_label_chan = stats_label_holder.clone();

    settings_btn.connect_clicked(move |_| {
        menu_popover.popdown();
        let prefs_window = adw::PreferencesWindow::builder()
            .title("Preferences")
            .transient_for(&window_parent_hook.root().unwrap().downcast::<gtk::Window>().unwrap())
            .modal(true).build();

        let page = PreferencesPage::new();
        let group = PreferencesGroup::builder().title("System Processing &amp; UI Aesthetics").build();
        let current_state = prefs_window_ref.lock().unwrap();
        let current_ui = ui_telemetry_ref.lock().unwrap();

        let stats_group = PreferencesGroup::builder()
            .title("Database Statistics &amp; Cache Lifecycle Telemetry")
            .build();
        let stats_display = Label::builder()
            .label(format!(
                "Total Working Stations Loaded: {}\nLast Radio-Browser Sync: {}\nTotal Categories / Stations Fetched: {}",
                current_ui.last_stations_updated_count,
                current_ui.last_update_timestamp,
                current_ui.total_elements_fetched
            ))
            .css_classes(vec!["caption".to_string()])
            .halign(gtk::Align::Start)
            .wrap(true)
            .build();
        stats_group.add(&stats_display);
        *stats_label_btn.borrow_mut() = Some(stats_display.clone());

        let sync_row = ActionRow::builder().title("Update Station List Now").build();
        let sync_btn = Button::builder()
            .label("Sync")
            .valign(gtk::Align::Center)
            .build();
        let sync_tx = tx_sync_prefs.clone();
        let sync_prefs = prefs_sync_prefs.clone();
        let sync_ui = ui_sync_prefs.clone();
        let sync_lbl = sync_label_prefs.clone();
        sync_btn.connect_clicked(move |_| {
            sync_lbl.set_text("Syncing...");
            let mut ui = sync_ui.lock().unwrap();
            let view = ui.current_view.clone();
            if matches!(view, ViewType::Favorites | ViewType::Recent | ViewType::Playlists) {
                let stations = match view {
                    ViewType::Favorites => ui.favorites.clone(),
                    ViewType::Recent => ui.recent_stations.clone(),
                    ViewType::Playlists => ui.playlists.clone(),
                    _ => unreachable!(),
                };
                let stations = filter_stations_local(stations, ui.active_bitrate);
                drop(ui);
                sync_lbl.set_text("Local list refreshed");
                send_local_stations(&sync_tx, view, stations, String::new());
                return;
            }
            let url = build_filtered_url(&ui.active_category, ui.active_bitrate);
            ui.last_fetched_url = url.clone();
            drop(ui);
            fetch_data_async(
                url,
                true,
                station_response_view(&view),
                sync_prefs.clone(),
                sync_tx.clone(),
                false,
            );
        });
        sync_row.add_suffix(&sync_btn);

        let high_quality_row = ActionRow::builder().title("Prefer High Bitrate Streams").build();
        let hq_switch = Switch::builder().active(current_state.prefer_high_bitrate).valign(gtk::Align::Center).build();
        let hq_save = prefs_window_ref.clone();
        hq_switch.connect_active_notify(move |sw| { hq_save.lock().unwrap().prefer_high_bitrate = sw.is_active(); });
        high_quality_row.add_suffix(&hq_switch);

        let broken_row = ActionRow::builder().title("Hide Broken Stations").build();
        let broken_switch = Switch::builder().active(current_state.hide_broken).valign(gtk::Align::Center).build();
        let broken_save = prefs_window_ref.clone();
        broken_switch.connect_active_notify(move |sw| { broken_save.lock().unwrap().hide_broken = sw.is_active(); });
        broken_row.add_suffix(&broken_switch);

        let auto_update_row = ActionRow::builder().title("Automatically Background Update (6h Intervallic)").build();
        let auto_switch = Switch::builder().active(current_state.auto_update).valign(gtk::Align::Center).build();
        let auto_save = prefs_window_ref.clone();
        auto_switch.connect_active_notify(move |sw| { auto_save.lock().unwrap().auto_update = sw.is_active(); });
        auto_update_row.add_suffix(&auto_switch);

        let list_models_analyzer = StringList::new(&["Classic Audio Bars", "Oscilloscope Wave", "Digital VU Matrix (With Peak Hold)", "Digital VU Blocks (No Hold)"]);
        let current_type_idx = match current_state.analyzer_type {
            AnalyzerType::Bars => 0,
            AnalyzerType::Wave => 1,
            AnalyzerType::BlocksWithHold => 2,
            AnalyzerType::DigitalVuBlocks => 3,
        };
        let analyzer_row = ComboRow::builder()
            .title("Waveform Analyzer Theme View")
            .model(&list_models_analyzer)
            .selected(current_type_idx)
            .build();
        let analyzer_save = prefs_window_ref.clone();
        analyzer_row.connect_selected_notify(move |row| {
            let mut guard = analyzer_save.lock().unwrap();
            guard.analyzer_type = match row.selected() {
                0 => AnalyzerType::Bars,
                1 => AnalyzerType::Wave,
                2 => AnalyzerType::BlocksWithHold,
                _ => AnalyzerType::DigitalVuBlocks,
            };
        });

        drop(current_state);
        drop(current_ui);

        let audio_group = PreferencesGroup::builder().title("Audio Output").build();
        let sink_options = ["Auto (default)", "PulseAudio", "PipeWire", "ALSA", "JACK", "OSS"];
        let sink_ids     = ["autoaudiosink", "pulsesink", "pipewiresink", "alsasink", "jackaudiosink", "osssink"];
        let current_sink = prefs_window_ref.lock().unwrap().audio_sink.clone();
        let current_sink_idx = sink_ids.iter().position(|&s| s == current_sink).unwrap_or(0) as u32;
        let sink_model = StringList::new(&sink_options);
        let sink_row = ComboRow::builder()
            .title("Audio Output Device")
            .subtitle("Takes effect on the next station play")
            .model(&sink_model)
            .selected(current_sink_idx)
            .build();
        let sink_save = prefs_window_ref.clone();
        sink_row.connect_selected_notify(move |row| {
            let idx = row.selected() as usize;
            if let Some(&id) = sink_ids.get(idx) {
                sink_save.lock().unwrap().audio_sink = id.to_string();
            }
        });
        audio_group.add(&sink_row);

        let disable_video_active = prefs_window_ref.lock().unwrap().disable_video_streams;
        let disable_video_row = ActionRow::builder().title("Disable Video Streams (Audio Only)").build();
        let video_switch = Switch::builder().active(disable_video_active).valign(gtk::Align::Center).build();
        let video_save = prefs_window_ref.clone();
        video_switch.connect_active_notify(move |sw| { video_save.lock().unwrap().disable_video_streams = sw.is_active(); });
        disable_video_row.add_suffix(&video_switch);

        let disable_buffer_active = prefs_window_ref.lock().unwrap().disable_audio_buffer;
        let disable_buffer_row = ActionRow::builder().title("Disable Audio Buffer (Faster Start)").build();
        let buffer_switch = Switch::builder().active(disable_buffer_active).valign(gtk::Align::Center).build();
        let buffer_save = prefs_window_ref.clone();
        buffer_switch.connect_active_notify(move |sw| { buffer_save.lock().unwrap().disable_audio_buffer = sw.is_active(); });
        disable_buffer_row.add_suffix(&buffer_switch);
        audio_group.add(&disable_buffer_row);

        group.add(&sync_row);
        group.add(&high_quality_row);
        group.add(&disable_video_row);
        group.add(&broken_row);
        group.add(&auto_update_row);
        group.add(&analyzer_row);

        page.add(&stats_group);
        page.add(&audio_group);

        let about_group = PreferencesGroup::builder().title("About").build();
        let about_row = ActionRow::builder()
            .title(format!("Rust Radio v{}", env!("CARGO_PKG_VERSION")))
            .subtitle(option_env!("BUILD_DATETIME").unwrap_or("unknown"))
            .activatable(true)
            .build();
        let parent_window = window_parent_hook.root().unwrap().downcast::<gtk::Window>().unwrap();
        about_row.connect_activated(move |_| {
            let dialog = gtk::Window::builder()
                .title("About")
                .transient_for(&parent_window)
                .modal(true)
                .default_width(420)
                .default_height(360)
                .resizable(false)
                .build();
            let content = Box::builder()
                .orientation(Orientation::Vertical)
                .spacing(12)
                .margin_top(24).margin_bottom(24).margin_start(24).margin_end(24)
                .build();
            let logo = gtk::Picture::builder()
                .width_request(128)
                .height_request(128)
                .halign(gtk::Align::Center)
                .valign(gtk::Align::Center)
                .keep_aspect_ratio(true)
                .can_shrink(true)
                .build();
            let icon_path = if std::path::Path::new("/usr/share/icons/hicolor/256x256/apps/rust-radio.png").exists() {
                "/usr/share/icons/hicolor/256x256/apps/rust-radio.png"
            } else {
                "assets/icon-256.png"
            };
            logo.set_filename(Some(icon_path));
            let app_label = Label::builder()
                .label(format!("Rust Radio v{}", env!("CARGO_PKG_VERSION")))
                .css_classes(vec!["title-1".to_string()])
                .halign(gtk::Align::Center)
                .build();
            let desc_label = Label::builder()
                .label("A GTK4 internet radio player that browses and plays\nstations from the radio-browser.info database.")
                .css_classes(vec!["body".to_string()])
                .halign(gtk::Align::Center)
                .justify(gtk::Justification::Center)
                .wrap(true)
                .build();
            let build_label = Label::builder()
                .label(option_env!("BUILD_DATETIME").unwrap_or("unknown"))
                .css_classes(vec!["caption".to_string()])
                .halign(gtk::Align::Center)
                .build();
            let copyright_label = Label::builder()
                .label("© 2026 Antoxa")
                .css_classes(vec!["caption".to_string()])
                .halign(gtk::Align::Center)
                .build();
            let license_label = Label::builder()
                .label("Licensed under the GNU General Public License v3.0")
                .css_classes(vec!["caption".to_string()])
                .halign(gtk::Align::Center)
                .build();
            let website_link = Label::builder()
                .label("<a href=\"https://github.com/antoxa78/Rust-Radio\">github.com/antoxa78/Rust-Radio</a>")
                .use_markup(true)
                .css_classes(vec!["caption".to_string()])
                .halign(gtk::Align::Center)
                .build();
            content.append(&logo);
            content.append(&app_label);
            content.append(&desc_label);
            content.append(&build_label);
            content.append(&copyright_label);
            content.append(&license_label);
            content.append(&website_link);
            dialog.set_child(Some(&content));
            dialog.present();
        });
        about_group.add(&about_row);
        page.add(&group);

        // --- Backup & Restore Group ---
        let backup_restore_group = PreferencesGroup::builder()
            .title("Data Management")
            .description("Backup or restore your application data categories to/from a JSON file.")
            .build();

        // --- Backup Row ---
        let backup_row = ActionRow::builder()
            .title("Backup Application Data")
            .subtitle("Export selected categories to a JSON file")
            .build();
        let backup_btn = Button::builder()
            .label("Backup…")
            .valign(gtk::Align::Center)
            .build();
        {
            let ui_bk = ui_backup_ref.clone();
            let prefs_bk = prefs_backup_ref.clone();
            let prefs_win_bk = prefs_window.clone();
            backup_btn.connect_clicked(move |_| {
                let dialog_win = gtk::Window::builder()
                    .title("Backup Application Data")
                    .transient_for(&prefs_win_bk)
                    .modal(true)
                    .default_width(340)
                    .resizable(false)
                    .build();
                let vbox = Box::builder()
                    .orientation(Orientation::Vertical)
                    .spacing(12)
                    .margin_top(20).margin_bottom(20).margin_start(20).margin_end(20)
                    .build();
                let title_lbl = Label::builder()
                    .label("Select categories to backup:")
                    .halign(gtk::Align::Start)
                    .css_classes(vec!["heading".to_string()])
                    .build();
                vbox.append(&title_lbl);

                let chk_home_tags   = gtk::CheckButton::with_label("Home Tags");
                let chk_favourites  = gtk::CheckButton::with_label("Favourites");
                let chk_playlists   = gtk::CheckButton::with_label("Playlists");
                let chk_custom_tags = gtk::CheckButton::with_label("Custom Tags");
                chk_home_tags.set_active(true);
                chk_favourites.set_active(true);
                chk_playlists.set_active(true);
                chk_custom_tags.set_active(true);
                vbox.append(&chk_home_tags);
                vbox.append(&chk_favourites);
                vbox.append(&chk_playlists);
                vbox.append(&chk_custom_tags);

                let btn_row = Box::builder()
                    .orientation(Orientation::Horizontal)
                    .spacing(8)
                    .halign(gtk::Align::End)
                    .margin_top(8)
                    .build();
                let cancel_btn = Button::builder().label("Cancel").build();
                let export_btn = Button::builder()
                    .label("Save File…")
                    .css_classes(vec!["suggested-action".to_string()])
                    .build();
                btn_row.append(&cancel_btn);
                btn_row.append(&export_btn);
                vbox.append(&btn_row);
                dialog_win.set_child(Some(&vbox));

                let dw_cancel = dialog_win.clone();
                cancel_btn.connect_clicked(move |_| { dw_cancel.close(); });

                let dw_export = dialog_win.clone();
                let ui_exp = ui_bk.clone();
                let prefs_exp = prefs_bk.clone();
                let chk_ht = chk_home_tags.clone();
                let chk_fv = chk_favourites.clone();
                let chk_pl = chk_playlists.clone();
                let chk_ct = chk_custom_tags.clone();
                export_btn.connect_clicked(move |_| {
                    #[derive(Serialize)]
                    struct BackupPayload {
                        #[serde(skip_serializing_if = "Option::is_none")]
                        home_tags: Option<Vec<String>>,
                        #[serde(skip_serializing_if = "Option::is_none")]
                        favourites: Option<Vec<Station>>,
                        #[serde(skip_serializing_if = "Option::is_none")]
                        playlists: Option<Vec<Station>>,
                        #[serde(skip_serializing_if = "Option::is_none")]
                        custom_tags: Option<Vec<String>>,
                    }
                    let ui  = ui_exp.lock().unwrap();
                    let prf = prefs_exp.lock().unwrap();
                    let payload = BackupPayload {
                        home_tags:   if chk_ht.is_active() {
                            Some(ui.cached_tags.clone().unwrap_or_else(|| {
                                if matches!(ui.current_view, ViewType::Tags) {
                                    ui.categories.clone()
                                } else {
                                    Vec::new()
                                }
                            }).into_iter().map(|c| c.name).collect())
                        } else { None },
                        favourites:  if chk_fv.is_active() { Some(ui.favorites.clone()) } else { None },
                        playlists:   if chk_pl.is_active() { Some(ui.playlists.clone()) } else { None },
                        custom_tags: if chk_ct.is_active() { Some(prf.custom_tags.clone()) } else { None },
                    };
                    drop(ui); drop(prf);
                    if let Ok(json) = serde_json::to_string_pretty(&payload) {
                        let chooser = gtk::FileChooserDialog::new(
                            Some("Save Backup"),
                            Some(&dw_export),
                            gtk::FileChooserAction::Save,
                            &[("Cancel", gtk::ResponseType::Cancel), ("Save", gtk::ResponseType::Accept)],
                        );
                        chooser.set_current_name("rust_radio_backup.json");
                        let json_clone = json.clone();
                        let dw_done = dw_export.clone();
                        chooser.connect_response(move |dlg, resp| {
                            if resp == gtk::ResponseType::Accept {
                                if let Some(file) = dlg.file() {
                                    if let Some(path) = file.path() {
                                        let _ = std::fs::write(&path, &json_clone);
                                    }
                                }
                                dw_done.close();
                            }
                            dlg.close();
                        });
                        chooser.present();
                    }
                });

                dialog_win.present();
            });
        }
        backup_row.add_suffix(&backup_btn);

        // --- Restore Row ---
        let restore_row = ActionRow::builder()
            .title("Restore Application Data")
            .subtitle("Import selected categories from a JSON backup file")
            .build();
        let restore_btn = Button::builder()
            .label("Restore…")
            .valign(gtk::Align::Center)
            .build();
        {
            let ui_rs = ui_restore_ref.clone();
            let prefs_rs = prefs_restore_ref.clone();
            let save_rs = app_state_restore_ref.clone();
            let prefs_win_rs = prefs_window.clone();
            let tx_restore_button = tx_restore_ref.clone();
            let custom_tags_restore_button = custom_tags_restore_ref.clone();
            restore_btn.connect_clicked(move |_| {
                let ui_rs2    = ui_rs.clone();
                let prefs_rs2 = prefs_rs.clone();
                let save_rs2  = save_rs.clone();
                let parent_rs = prefs_win_rs.clone();
                let chooser = gtk::FileChooserDialog::new(
                    Some("Open Backup File"),
                    Some(&prefs_win_rs),
                    gtk::FileChooserAction::Open,
                    &[("Cancel", gtk::ResponseType::Cancel), ("Open", gtk::ResponseType::Accept)],
                );
                let filter = gtk::FileFilter::new();
                filter.add_pattern("*.json");
                filter.set_name(Some("JSON files"));
                chooser.add_filter(&filter);
                let tx_restore = tx_restore_button.clone();
                let custom_tags_restore = custom_tags_restore_button.clone();
                chooser.connect_response(move |dlg, resp| {
                    if resp == gtk::ResponseType::Accept {
                    if let Some(file) = dlg.file() {
                        if let Some(path) = file.path() {
                            let text = match read_limited_text(&path, MAX_BACKUP_BYTES) {
                                Ok(text) => text,
                                Err(err) => {
                                    eprintln!("Failed to read backup: {}", err);
                                    return;
                                }
                            };
                            #[derive(Deserialize, Default)]
                            struct BackupPayload {
                                home_tags:   Option<Vec<String>>,
                                favourites:  Option<Vec<Station>>,
                                playlists:   Option<Vec<Station>>,
                                custom_tags: Option<Vec<String>>,
                            }
                            if let Ok(mut payload) = serde_json::from_str::<BackupPayload>(&text) {
                                if let Some(ref mut stations) = payload.favourites {
                                    stations.truncate(MAX_PLAYLIST_STATIONS);
                                }
                                if let Some(ref mut stations) = payload.playlists {
                                    stations.truncate(MAX_PLAYLIST_STATIONS);
                                }
                                if let Some(ref mut tags) = payload.custom_tags {
                                    tags.truncate(MAX_PLAYLIST_STATIONS);
                                }
                                let dialog_win = gtk::Window::builder()
                                    .title("Restore Application Data")
                                    .transient_for(&parent_rs)
                                    .modal(true)
                                    .default_width(360)
                                    .resizable(false)
                                    .build();
                                let vbox = Box::builder()
                                    .orientation(Orientation::Vertical)
                                    .spacing(12)
                                    .margin_top(20).margin_bottom(20).margin_start(20).margin_end(20)
                                    .build();
                                let title_lbl = Label::builder()
                                    .label("Select categories to restore:")
                                    .halign(gtk::Align::Start)
                                    .css_classes(vec!["heading".to_string()])
                                    .build();
                                vbox.append(&title_lbl);

                                let chk_ht = gtk::CheckButton::with_label("Home Tags");
                                let chk_fv = gtk::CheckButton::with_label("Favourites");
                                let chk_pl = gtk::CheckButton::with_label("Playlists");
                                let chk_ct = gtk::CheckButton::with_label("Custom Tags");

                                chk_ht.set_active(payload.home_tags.is_some());
                                chk_fv.set_active(payload.favourites.is_some());
                                chk_pl.set_active(payload.playlists.is_some());
                                chk_ct.set_active(payload.custom_tags.is_some());
                                chk_ht.set_sensitive(payload.home_tags.is_some());
                                chk_fv.set_sensitive(payload.favourites.is_some());
                                chk_pl.set_sensitive(payload.playlists.is_some());
                                chk_ct.set_sensitive(payload.custom_tags.is_some());
                                vbox.append(&chk_ht);
                                vbox.append(&chk_fv);
                                vbox.append(&chk_pl);
                                vbox.append(&chk_ct);

                                let warn_lbl = Label::builder()
                                    .label("⚠ Restoring will overwrite existing data in the selected categories.")
                                    .halign(gtk::Align::Start)
                                    .wrap(true)
                                    .css_classes(vec!["caption".to_string(), "dim-label".to_string()])
                                    .build();
                                vbox.append(&warn_lbl);

                                let btn_row = Box::builder()
                                    .orientation(Orientation::Horizontal)
                                    .spacing(8)
                                    .halign(gtk::Align::End)
                                    .margin_top(8)
                                    .build();
                                let cancel_btn = Button::builder().label("Cancel").build();
                                let apply_btn = Button::builder()
                                    .label("Restore")
                                    .css_classes(vec!["destructive-action".to_string()])
                                    .build();
                                btn_row.append(&cancel_btn);
                                btn_row.append(&apply_btn);
                                vbox.append(&btn_row);
                                dialog_win.set_child(Some(&vbox));

                                let dw_cancel = dialog_win.clone();
                                cancel_btn.connect_clicked(move |_| { dw_cancel.close(); });

                                let dw_apply = dialog_win.clone();
                                let ui_ap    = ui_rs2.clone();
                                let prefs_ap = prefs_rs2.clone();
                                let save_ap  = save_rs2.clone();
                                let ht_data  = payload.home_tags.clone();
                                let fv_data  = payload.favourites.clone();
                                let pl_data  = payload.playlists.clone();
                                let ct_data  = payload.custom_tags.clone();
                                let tx_ap = tx_restore.clone();
                                let custom_tags_restore_ap = custom_tags_restore.clone();
                                apply_btn.connect_clicked(move |_| {
                                    let mut ui = ui_ap.lock().unwrap();
                                    if chk_ht.is_active() {
                                        if let Some(ref tags) = ht_data {
                                            ui.cached_tags = Some(tags.iter().map(|name| BrowseItem { name: name.clone(), stationcount: 0, countrycode: String::new() }).collect());
                                        }
                                    }
                                    if chk_fv.is_active() {
                                        if let Some(ref favs) = fv_data {
                                            ui.favorites = favs
                                                .iter()
                                                .filter(|station| is_allowed_stream_url(&station.url_resolved))
                                                .cloned()
                                                .collect();
                                        }
                                    }
                                    if chk_pl.is_active() {
                                        if let Some(ref pls) = pl_data {
                                            ui.playlists = pls
                                                .iter()
                                                .filter(|station| is_allowed_stream_url(&station.url_resolved))
                                                .cloned()
                                                .collect();
                                        }
                                    }
                                    drop(ui);
                                    if chk_ct.is_active() {
                                        if let Some(ref tags) = ct_data {
                                            prefs_ap.lock().unwrap().custom_tags = tags.clone();
                                        }
                                    }
                                    save_ap.save_data();
                                    while let Some(child) = custom_tags_restore_ap.first_child() {
                                        custom_tags_restore_ap.remove(&child);
                                    }
                                    custom_tags_restore_ap.set_visible(false);
                                    let current_view = ui_ap.lock().unwrap().current_view.clone();
                                    match current_view {
                                        ViewType::Tags => {
                                            let items = ui_ap
                                                .lock()
                                                .unwrap()
                                                .cached_tags
                                                .clone()
                                                .unwrap_or_default();
                                            send_local_categories(&tx_ap, ViewType::Tags, items);
                                        }
                                        ViewType::Favorites => {
                                            let mut ui = ui_ap.lock().unwrap();
                                            let stations = if ui.active_bitrate == BitrateFilterSetting::None {
                                                ui.favorites.clone()
                                            } else {
                                                let filtered = filter_stations_local(ui.favorites.clone(), ui.active_bitrate);
                                                ui.stations = filtered.clone();
                                                filtered
                                            };
                                            drop(ui);
                                            send_local_stations(&tx_ap, ViewType::Favorites, stations, String::new());
                                        }
                                        ViewType::Recent => {
                                            let mut ui = ui_ap.lock().unwrap();
                                            let stations = if ui.active_bitrate == BitrateFilterSetting::None {
                                                ui.recent_stations.clone()
                                            } else {
                                                let filtered = filter_stations_local(ui.recent_stations.clone(), ui.active_bitrate);
                                                ui.stations = filtered.clone();
                                                filtered
                                            };
                                            drop(ui);
                                            send_local_stations(&tx_ap, ViewType::Recent, stations, String::new());
                                        }
                                        ViewType::Playlists => {
                                            let mut ui = ui_ap.lock().unwrap();
                                            let stations = if ui.active_bitrate == BitrateFilterSetting::None {
                                                ui.playlists.clone()
                                            } else {
                                                let filtered = filter_stations_local(ui.playlists.clone(), ui.active_bitrate);
                                                ui.stations = filtered.clone();
                                                filtered
                                            };
                                            drop(ui);
                                            send_local_stations(&tx_ap, ViewType::Playlists, stations, String::new());
                                        }
                                        view @ (ViewType::Stations | ViewType::CustomTag(_)) => {
                                            let ui = ui_ap.lock().unwrap();
                                            let stations = ui.stations.clone();
                                            let limit = ui.station_limit;
                                            let has_more_hint = ui.station_has_more_hint;
                                            drop(ui);
                                            send_local_stations_with_limit_hint(
                                                &tx_ap,
                                                view,
                                                stations,
                                                String::new(),
                                                limit,
                                                has_more_hint,
                                            );
                                        }
                                        view @ (ViewType::Languages | ViewType::Countries) => {
                                            let (view, url) = match view {
                                                ViewType::Languages => (
                                                    ViewType::Languages,
                                                    "https://de1.api.radio-browser.info/json/languages?order=stationcount&reverse=true".to_string(),
                                                ),
                                                ViewType::Countries => (
                                                    ViewType::Countries,
                                                    "https://de1.api.radio-browser.info/json/countries?order=stationcount&reverse=true".to_string(),
                                                ),
                                                _ => unreachable!(),
                                            };
                                            ui_ap.lock().unwrap().last_fetched_url = url.clone();
                                            fetch_data_async(url, false, view, prefs_ap.clone(), tx_ap.clone(), false);
                                        }
                                    }
                                    dw_apply.close();
                                });

                                dialog_win.present();
                            }
                        }
                    }
                    dlg.close();
                    }
                });
                chooser.present();
            });
        }
        restore_row.add_suffix(&restore_btn);

        backup_restore_group.add(&backup_row);
        backup_restore_group.add(&restore_row);
        page.add(&backup_restore_group);

        page.add(&about_group);
        prefs_window.add(&page);

        let prefs_close_save = app_state_prefs_close_ref.clone();
        prefs_window.connect_close_request(move |_| {
            prefs_close_save.save_data();
            adw::glib::signal::Propagation::Proceed
        });

        prefs_window.present();
    });

    let prefs_custom_tags_toggle = app_prefs.clone();
    let tx_custom_tags_toggle = tx.clone();
    let ui_custom_tags_toggle = ui_state.clone();
    let custom_tag_btns_toggle = custom_tag_buttons_box.clone();
    let state_toggle_save = app_state_manager.clone();
    let list_box_tags = list_box.clone();
    let browsing_tags = now_browsing_label.clone();
    // Pre-clone for the four bitrate handlers that come after this closure
    let br_btn_for_any = bitrate_menu_btn.clone();
    let br_btn_for_low = bitrate_menu_btn.clone();
    let br_btn_for_high = bitrate_menu_btn.clone();
    let br_btn_for_flac = bitrate_menu_btn.clone();
    let save_mgr_br_any = app_state_manager.clone();
    let save_mgr_br_low = app_state_manager.clone();
    let save_mgr_br_high = app_state_manager.clone();
    let save_mgr_br_flac = app_state_manager.clone();
    let import_btn_tags = import_btn_clone.clone();
    let clear_recents_tags = clear_recents_btn.clone();
    let import_btn_tag_inner = import_btn_clone.clone();
    let clear_recents_tag_inner = clear_recents_btn.clone();
    let br_area_ct = br_area.clone();
    custom_tags_btn.connect_clicked(move |_| {
        let currently_visible = custom_tag_btns_toggle.is_visible();
        if currently_visible {
            custom_tag_btns_toggle.set_visible(false);
            return;
        }
        br_area_ct.set_visible(true);
        import_btn_tags.set_visible(false);
        clear_recents_tags.set_visible(false);
        while let Some(child) = custom_tag_btns_toggle.first_child() {
            custom_tag_btns_toggle.remove(&child);
        }
        let tags = prefs_custom_tags_toggle.lock().unwrap().custom_tags.clone();
        for tag in tags {
            let row_box = Box::builder()
                .orientation(Orientation::Horizontal)
                .spacing(0)
                .hexpand(true)
                .build();
            let hbox = Box::builder()
                .orientation(Orientation::Horizontal)
                .spacing(6)
                .margin_start(20)
                .halign(gtk::Align::Start)
                .build();
            let icon_img = gtk::Image::from_icon_name(tag_to_icon(&tag));
            icon_img.set_icon_size(gtk::IconSize::Normal);
            let lbl = Label::builder()
                .label(&tag)
                .css_classes(vec!["caption".to_string()])
                .build();
            hbox.append(&icon_img);
            hbox.append(&lbl);
            let btn = Button::builder()
                .child(&hbox)
                .css_classes(vec!["flat".to_string()])
                .hexpand(true)
                .tooltip_text(&tag)
                .margin_top(1)
                .margin_bottom(1)
                .build();
            let tag_click = tag.clone();
            let tx_c = tx_custom_tags_toggle.clone();
            let ui_c = ui_custom_tags_toggle.clone();
            let prefs_c = prefs_custom_tags_toggle.clone();
            let br_btn_c = bitrate_menu_btn.clone();
            let btns_container = custom_tag_btns_toggle.clone();
            let import_btn_tag_btn = import_btn_tag_inner.clone();
            let clear_recents_tag_btn = clear_recents_tag_inner.clone();
            let list_box_tag_btn = list_box_tags.clone();
            let browsing_tag_btn = browsing_tags.clone();
            let br_area_tag = br_area_ct.clone();
            btn.connect_clicked(move |clicked_btn| {
                br_area_tag.set_visible(true);
                import_btn_tag_btn.set_visible(false);
                clear_recents_tag_btn.set_visible(false);
                // Reset all custom tag button icons to their original tag icon
                let n = btns_container.observe_children().n_items();
                for i in 0..n {
                    if let Some(child) = btns_container.observe_children().item(i) {
                        if let Ok(row) = child.downcast::<Box>() {
                            if let Some(cb) =
                                row.first_child().and_then(|c| c.downcast::<Button>().ok())
                            {
                                let tag_name = cb.tooltip_text().unwrap_or_default();
                                if let Some(box_child) = cb.child() {
                                    if let Some(hb) = box_child.downcast_ref::<Box>() {
                                        if let Some(img) = hb
                                            .first_child()
                                            .and_then(|i| i.downcast::<gtk::Image>().ok())
                                        {
                                            img.set_icon_name(Some(tag_to_icon(&tag_name)));
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                // Highlight the active button with a play icon
                if let Some(box_child) = clicked_btn.child() {
                    if let Some(hbox) = box_child.downcast_ref::<Box>() {
                        if let Some(img) = hbox.first_child() {
                            if let Some(icon) = img.downcast_ref::<gtk::Image>() {
                                icon.set_icon_name(Some("media-playback-start-symbolic"));
                            }
                        }
                    }
                }
                let mut ui = ui_c.lock().unwrap();
                ui.previous_view = Some(ui.current_view.clone());
                ui.previous_url = ui.last_fetched_url.clone();
                ui.active_category = CategoryFilter::Tag(tag_click.clone());
                ui.current_view = ViewType::CustomTag(tag_click.clone());
                let url = build_filtered_url(&ui.active_category, ui.active_bitrate);
                ui.last_fetched_url = url.clone();
                // Sync bitrate label
                br_btn_c.set_label(match ui.active_bitrate {
                    BitrateFilterSetting::None => "⊟ Bitrate",
                    BitrateFilterSetting::Low => "⊟ Low ≤160",
                    BitrateFilterSetting::High => "⊟ High ≥192",
                    BitrateFilterSetting::Flac => "⊟ FLAC",
                });
                drop(ui);
                while let Some(child) = list_box_tag_btn.first_child() {
                    list_box_tag_btn.remove(&child);
                }
                browsing_tag_btn.set_text("Loading stations...");
                fetch_data_async(
                    url,
                    true,
                    ViewType::CustomTag(tag_click.clone()),
                    prefs_c.clone(),
                    tx_c.clone(),
                    false,
                );
            });
            let rm_btn = Button::builder()
                .icon_name("window-close-symbolic")
                .css_classes(vec!["flat".to_string()])
                .margin_top(1)
                .margin_bottom(1)
                .tooltip_text("Remove tag")
                .build();
            let tag_rm = tag.clone();
            let prefs_rm_toggle = prefs_custom_tags_toggle.clone();
            let row_box_rm = row_box.clone();
            let container_rm = custom_tag_btns_toggle.clone();
            let save_mgr_toggle_rm = state_toggle_save.clone();
            rm_btn.connect_clicked(move |_| {
                prefs_rm_toggle
                    .lock()
                    .unwrap()
                    .custom_tags
                    .retain(|t| t != &tag_rm);
                save_mgr_toggle_rm.save_data();
                container_rm.remove(&row_box_rm);
            });
            row_box.append(&btn);
            row_box.append(&rm_btn);
            custom_tag_btns_toggle.append(&row_box);
        }
        custom_tag_btns_toggle.set_visible(true);
    });

    let tx_tag = tx.clone();
    let prefs_tag = app_prefs.clone();
    let ui_tag = ui_state.clone();
    tag_btn.connect_clicked(move |_| {
        let mut ui = ui_tag.lock().unwrap();
        if let Some(cached) = ui.cached_tags.clone() {
            ui.last_fetched_url = "https://de1.api.radio-browser.info/json/tags?order=stationcount&reverse=true&hidebroken=true".to_string();
            ui.active_category = CategoryFilter::None;
            drop(ui);
            send_local_categories(&tx_tag, ViewType::Tags, cached);
            return;
        }
        let target_url = "https://de1.api.radio-browser.info/json/tags?order=stationcount&reverse=true&hidebroken=true".to_string();
        ui.last_fetched_url = target_url.clone();
        ui.active_category = CategoryFilter::None;
        drop(ui);
        fetch_data_async(target_url, false, ViewType::Tags, prefs_tag.clone(), tx_tag.clone(), false);
    });

    let back_ui = ui_state.clone();
    let back_tx = tx.clone();
    let back_prefs = app_prefs.clone();
    let back_browsing = now_browsing_label.clone();
    let back_list = list_box.clone();
    back_btn.connect_clicked(move |_| {
        let mut ui = back_ui.lock().unwrap();
        if let Some(prev) = ui.previous_view.take() {
            let url = ui.previous_url.clone();
            match prev {
                ViewType::Tags | ViewType::Languages | ViewType::Countries => {
                    ui.current_view = prev.clone();
                    ui.active_category = CategoryFilter::None;
                    ui.last_fetched_url = url.clone();
                    let cached = match &prev {
                        ViewType::Tags => ui.cached_tags.clone(),
                        ViewType::Languages => ui.cached_languages.clone(),
                        ViewType::Countries => ui.cached_countries.clone(),
                        _ => None,
                    };
                    if let Some(items) = cached {
                        drop(ui);
                        send_local_categories(&back_tx, prev, items);
                    } else {
                        drop(ui);
                        fetch_data_async(
                            url,
                            false,
                            prev,
                            back_prefs.clone(),
                            back_tx.clone(),
                            false,
                        );
                    }
                }
                ViewType::CustomTag(ref tag) => {
                    let tag = tag.clone();
                    ui.active_category = CategoryFilter::Tag(tag.clone());
                    let cat_url = build_filtered_url(&ui.active_category, ui.active_bitrate);
                    ui.current_view = ViewType::CustomTag(tag.clone());
                    ui.last_fetched_url = cat_url.clone();
                    drop(ui);
                    back_browsing.set_text(&format!("Now Browsing Custom Tags > {}", tag));
                    while let Some(child) = back_list.first_child() {
                        back_list.remove(&child);
                    }
                    back_browsing.set_text("Loading stations...");
                    fetch_data_async(
                        cat_url,
                        true,
                        ViewType::CustomTag(tag),
                        back_prefs.clone(),
                        back_tx.clone(),
                        false,
                    );
                }
                ViewType::Stations => {
                    let cat_url = if url.contains("/json/stations/") {
                        url.clone()
                    } else {
                        build_filtered_url(&ui.active_category, ui.active_bitrate)
                    };
                    ui.current_view = ViewType::Stations;
                    ui.last_fetched_url = cat_url.clone();
                    drop(ui);
                    fetch_data_async(
                        cat_url,
                        true,
                        ViewType::Stations,
                        back_prefs.clone(),
                        back_tx.clone(),
                        false,
                    );
                }
                ViewType::Favorites => {
                    let stations = if ui.active_bitrate == BitrateFilterSetting::None {
                        ui.favorites.clone()
                    } else {
                        let filtered =
                            filter_stations_local(ui.favorites.clone(), ui.active_bitrate);
                        ui.stations = filtered.clone();
                        filtered
                    };
                    ui.current_view = ViewType::Favorites;
                    ui.current_index = None;
                    ui.active_category = CategoryFilter::None;
                    ui.last_fetched_url = url.clone();
                    drop(ui);
                    back_browsing.set_text("Favorite Stations");
                    send_local_stations(&back_tx, ViewType::Favorites, stations, String::new());
                }
                ViewType::Recent => {
                    let stations = if ui.active_bitrate == BitrateFilterSetting::None {
                        ui.recent_stations.clone()
                    } else {
                        let filtered =
                            filter_stations_local(ui.recent_stations.clone(), ui.active_bitrate);
                        ui.stations = filtered.clone();
                        filtered
                    };
                    ui.current_view = ViewType::Recent;
                    ui.current_index = None;
                    ui.active_category = CategoryFilter::None;
                    ui.last_fetched_url = url.clone();
                    drop(ui);
                    back_browsing.set_text("Recently Played");
                    send_local_stations(&back_tx, ViewType::Recent, stations, String::new());
                }
                ViewType::Playlists => {
                    let stations = if ui.active_bitrate == BitrateFilterSetting::None {
                        ui.playlists.clone()
                    } else {
                        let filtered =
                            filter_stations_local(ui.playlists.clone(), ui.active_bitrate);
                        ui.stations = filtered.clone();
                        filtered
                    };
                    ui.current_view = ViewType::Playlists;
                    ui.current_index = None;
                    ui.active_category = CategoryFilter::None;
                    ui.last_fetched_url = url.clone();
                    drop(ui);
                    back_browsing.set_text("Imported Playlist");
                    send_local_stations(&back_tx, ViewType::Playlists, stations, String::new());
                }
            }
        }
    });

    let tx_lang = tx.clone();
    let prefs_lang = app_prefs.clone();
    let ui_lang = ui_state.clone();
    lang_btn.connect_clicked(move |_| {
        let mut ui = ui_lang.lock().unwrap();
        if let Some(cached) = ui.cached_languages.clone() {
            ui.last_fetched_url =
                "https://de1.api.radio-browser.info/json/languages?order=stationcount&reverse=true"
                    .to_string();
            ui.active_category = CategoryFilter::None;
            drop(ui);
            send_local_categories(&tx_lang, ViewType::Languages, cached);
            return;
        }
        let target_url =
            "https://de1.api.radio-browser.info/json/languages?order=stationcount&reverse=true"
                .to_string();
        ui.last_fetched_url = target_url.clone();
        ui.active_category = CategoryFilter::None;
        drop(ui);
        fetch_data_async(
            target_url,
            false,
            ViewType::Languages,
            prefs_lang.clone(),
            tx_lang.clone(),
            false,
        );
    });

    let tx_country = tx.clone();
    let prefs_country = app_prefs.clone();
    let ui_country = ui_state.clone();
    country_btn.connect_clicked(move |_| {
        let mut ui = ui_country.lock().unwrap();
        if let Some(cached) = ui.cached_countries.clone() {
            ui.last_fetched_url =
                "https://de1.api.radio-browser.info/json/countries?order=stationcount&reverse=true"
                    .to_string();
            ui.active_category = CategoryFilter::None;
            drop(ui);
            send_local_categories(&tx_country, ViewType::Countries, cached);
            return;
        }
        let target_url =
            "https://de1.api.radio-browser.info/json/countries?order=stationcount&reverse=true"
                .to_string();
        ui.last_fetched_url = target_url.clone();
        ui.active_category = CategoryFilter::None;
        drop(ui);
        fetch_data_async(
            target_url,
            false,
            ViewType::Countries,
            prefs_country.clone(),
            tx_country.clone(),
            false,
        );
    });

    let list_box_search = list_box.clone();
    let browsing_search = now_browsing_label.clone();
    let tx_search = tx.clone();
    let prefs_search = app_prefs.clone();
    let ui_search = ui_state.clone();
    search_entry.connect_activate(move |entry| {
        let query = entry.text().to_string();
        if !query.is_empty() {
            let target_url = format!(
                "https://de1.api.radio-browser.info/json/stations/byname/{}",
                urlencoding::encode(&query)
            );
            let mut ui = ui_search.lock().unwrap();
            ui.previous_view = Some(ui.current_view.clone());
            ui.previous_url = ui.last_fetched_url.clone();
            ui.current_view = ViewType::Stations;
            ui.last_fetched_url = target_url.clone();
            ui.active_category = CategoryFilter::None;
            drop(ui);
            while let Some(child) = list_box_search.first_child() {
                list_box_search.remove(&child);
            }
            browsing_search.set_text("Loading stations...");
            fetch_data_async(
                target_url,
                true,
                ViewType::Stations,
                prefs_search.clone(),
                tx_search.clone(),
                false,
            );
        }
    });

    let sort_ascending = std::cell::Cell::new(true);
    let ui_abc_sort = ui_state.clone();
    let tx_abc_sort = tx.clone();
    sort_abc_btn.connect_clicked(move |_| {
        let asc = sort_ascending.get();
        let mut ui = ui_abc_sort.lock().unwrap();
        match ui.current_view {
            ViewType::Tags | ViewType::Languages | ViewType::Countries => {
                if asc {
                    ui.categories.sort_by_key(|a| a.name.to_lowercase());
                } else {
                    ui.categories
                        .sort_by_key(|b| std::cmp::Reverse(b.name.to_lowercase()));
                }
                send_local_categories(&tx_abc_sort, ui.current_view.clone(), ui.categories.clone());
            }
            ViewType::Stations | ViewType::CustomTag(_) => {
                if asc {
                    ui.stations.sort_by_key(|a| a.name.to_lowercase());
                } else {
                    ui.stations
                        .sort_by_key(|b| std::cmp::Reverse(b.name.to_lowercase()));
                }
                send_local_stations_with_limit_hint(
                    &tx_abc_sort,
                    ui.current_view.clone(),
                    ui.stations.clone(),
                    ui.last_update_timestamp.clone(),
                    ui.station_limit,
                    ui.station_has_more_hint,
                );
            }
            ViewType::Playlists => {
                if asc {
                    ui.playlists.sort_by_key(|a| a.name.to_lowercase());
                } else {
                    ui.playlists
                        .sort_by_key(|b| std::cmp::Reverse(b.name.to_lowercase()));
                }
                let stations = if ui.active_bitrate == BitrateFilterSetting::None {
                    ui.playlists.clone()
                } else {
                    let filtered = filter_stations_local(ui.playlists.clone(), ui.active_bitrate);
                    ui.stations = filtered.clone();
                    filtered
                };
                send_local_stations(
                    &tx_abc_sort,
                    ViewType::Playlists,
                    stations,
                    ui.last_update_timestamp.clone(),
                );
            }
            ViewType::Favorites => {
                if asc {
                    ui.favorites.sort_by_key(|a| a.name.to_lowercase());
                } else {
                    ui.favorites
                        .sort_by_key(|b| std::cmp::Reverse(b.name.to_lowercase()));
                }
                let stations = if ui.active_bitrate == BitrateFilterSetting::None {
                    ui.favorites.clone()
                } else {
                    let filtered = filter_stations_local(ui.favorites.clone(), ui.active_bitrate);
                    ui.stations = filtered.clone();
                    filtered
                };
                send_local_stations(
                    &tx_abc_sort,
                    ViewType::Favorites,
                    stations,
                    ui.last_update_timestamp.clone(),
                );
            }
            ViewType::Recent => {
                if asc {
                    ui.recent_stations.sort_by_key(|a| a.name.to_lowercase());
                } else {
                    ui.recent_stations
                        .sort_by_key(|b| std::cmp::Reverse(b.name.to_lowercase()));
                }
                let stations = if ui.active_bitrate == BitrateFilterSetting::None {
                    ui.recent_stations.clone()
                } else {
                    let filtered =
                        filter_stations_local(ui.recent_stations.clone(), ui.active_bitrate);
                    ui.stations = filtered.clone();
                    filtered
                };
                send_local_stations(
                    &tx_abc_sort,
                    ViewType::Recent,
                    stations,
                    ui.last_update_timestamp.clone(),
                );
            }
        }
        sort_ascending.set(!asc);
    });

    let list_box_votes = list_box.clone();
    let tx_votes = tx.clone();
    let prefs_votes = app_prefs.clone();
    let ui_votes = ui_state.clone();
    let browsing_votes = now_browsing_label.clone();
    votes_sidebar_btn.connect_clicked(move |_| {
        let target_url = "https://de1.api.radio-browser.info/json/stations/topvote/150".to_string();
        let mut ui = ui_votes.lock().unwrap();
        ui.previous_view = Some(ui.current_view.clone());
        ui.previous_url = ui.last_fetched_url.clone();
        ui.current_view = ViewType::Stations;
        ui.last_fetched_url = target_url.clone();
        ui.active_category = CategoryFilter::None;
        drop(ui);
        browsing_votes.set_text("Top Voted");
        while let Some(child) = list_box_votes.first_child() {
            list_box_votes.remove(&child);
        }
        fetch_data_async(
            target_url,
            true,
            ViewType::Stations,
            prefs_votes.clone(),
            tx_votes.clone(),
            false,
        );
    });

    let ui_fav_menu = ui_state.clone();
    let list_box_fav = list_box.clone();
    let count_lbl_fav = stations_count_label.clone();
    let browsing_fav = now_browsing_label.clone();
    let state_fav_rm_save = app_state_manager.clone();
    let import_btn_fav = import_btn_clone.clone();
    let clear_recents_fav = clear_recents_btn.clone();
    let br_area_fav = br_area.clone();
    fav_menu_btn.connect_clicked(move |_| {
        begin_request();
        br_area_fav.set_visible(true);
        import_btn_fav.set_visible(false);
        clear_recents_fav.set_visible(false);
        while let Some(child) = list_box_fav.first_child() {
            list_box_fav.remove(&child);
        }
        let mut ui = ui_fav_menu.lock().unwrap();
        ui.previous_view = Some(ui.current_view.clone());
        ui.previous_url = ui.last_fetched_url.clone();
        ui.current_view = ViewType::Favorites;
        ui.current_index = None;
        browsing_fav.set_text("Favorite Stations");
        let display_list: Vec<Station> = if ui.active_bitrate != BitrateFilterSetting::None {
            let filtered = filter_stations_local(ui.favorites.clone(), ui.active_bitrate);
            let flen = filtered.len();
            count_lbl_fav.set_text(&format!("{} favorite stations displayed", flen));
            ui.stations = filtered.clone();
            filtered
        } else {
            let count = ui.favorites.len();
            count_lbl_fav.set_text(&format!("{} favorite stations displayed", count));
            ui.favorites.clone()
        };
        drop(ui);

        for station in &display_list {
            let br_badge = station
                .bitrate
                .map_or(String::new(), |b| format!(" [{} kbps]", b));
            let full_subtitle = format!("{} | {}{}", station.country, station.tags, br_badge);
            let row = ActionRow::builder()
                .title(markup_text(station.name.trim()))
                .subtitle(markup_text(&full_subtitle))
                .title_lines(1)
                .subtitle_lines(1)
                .tooltip_text(format!("{}\n{}", station.name.trim(), full_subtitle))
                .activatable(true)
                .build();
            let remove_fav_btn = Button::builder()
                .icon_name("edit-delete-symbolic")
                .css_classes(vec!["flat".to_string()])
                .valign(gtk::Align::Center)
                .tooltip_text("Remove from favourites")
                .build();
            let station_to_remove = station.clone();
            let ui_rm_fav = ui_fav_menu.clone();
            let list_rm_fav = list_box_fav.clone();
            let count_rm_fav = count_lbl_fav.clone();
            let row_clone = row.clone();
            let save_mgr_fav_rm = state_fav_rm_save.clone();
            remove_fav_btn.connect_clicked(move |_| {
                begin_request();
                let mut ui = ui_rm_fav.lock().unwrap();
                ui.favorites.retain(|f| !station_eq(f, &station_to_remove));
                ui.current_index = None;
                let display_count = if ui.active_bitrate == BitrateFilterSetting::None {
                    ui.favorites.len()
                } else {
                    ui.stations = filter_stations_local(ui.favorites.clone(), ui.active_bitrate);
                    ui.stations.len()
                };
                drop(ui);
                save_mgr_fav_rm.save_data();
                list_rm_fav.remove(&row_clone);
                count_rm_fav.set_text(&format!("{} favorite stations displayed", display_count));
            });
            row.add_suffix(&remove_fav_btn);
            list_box_fav.append(&row);
        }
    });

    let ui_recent_menu = ui_state.clone();
    let list_box_recent = list_box.clone();
    let count_lbl_recent = stations_count_label.clone();
    let browsing_recent = now_browsing_label.clone();
    let import_btn_rec = import_btn_clone.clone();
    let clear_recents_rec = clear_recents_btn.clone();
    let br_area_rec = br_area.clone();
    recent_nav_btn.connect_clicked(move |_| {
        begin_request();
        br_area_rec.set_visible(true);
        import_btn_rec.set_visible(false);
        clear_recents_rec.set_visible(true);
        while let Some(child) = list_box_recent.first_child() {
            list_box_recent.remove(&child);
        }
        let mut ui = ui_recent_menu.lock().unwrap();
        ui.previous_view = Some(ui.current_view.clone());
        ui.previous_url = ui.last_fetched_url.clone();
        ui.current_view = ViewType::Recent;
        ui.current_index = None;
        browsing_recent.set_text("Recently Played");
        let display_list: Vec<Station> = if ui.active_bitrate != BitrateFilterSetting::None {
            let filtered = filter_stations_local(ui.recent_stations.clone(), ui.active_bitrate);
            let flen = filtered.len();
            count_lbl_recent.set_text(&format!("{} recently played stations", flen));
            ui.stations = filtered.clone();
            filtered
        } else {
            let count = ui.recent_stations.len();
            count_lbl_recent.set_text(&format!("{} recently played stations", count));
            ui.recent_stations.clone()
        };
        drop(ui);
        for station in &display_list {
            let br_badge = station
                .bitrate
                .map_or(String::new(), |b| format!(" [{} kbps]", b));
            let full_subtitle = format!("{} | {}{}", station.country, station.tags, br_badge);
            let row = ActionRow::builder()
                .title(markup_text(station.name.trim()))
                .subtitle(markup_text(&full_subtitle))
                .title_lines(1)
                .subtitle_lines(1)
                .tooltip_text(format!("{}\n{}", station.name.trim(), full_subtitle))
                .activatable(true)
                .build();
            list_box_recent.append(&row);
        }
    });

    let ui_playlists_view = ui_state.clone();
    let list_box_playlists = list_box.clone();
    let count_lbl_playlists = stations_count_label.clone();
    let browsing_playlists = now_browsing_label.clone();
    let import_btn_pl = import_playlist_btn.clone();
    let clear_recents_pl = clear_recents_btn.clone();
    let br_area_pl = br_area.clone();
    let tx_pl = tx.clone();
    playlists_nav_btn.connect_clicked(move |_| {
        br_area_pl.set_visible(false);
        import_btn_pl.set_visible(true);
        clear_recents_pl.set_visible(false);
        browsing_playlists.set_text("Imported Playlist");
        while let Some(child) = list_box_playlists.first_child() {
            list_box_playlists.remove(&child);
        }
        let mut view = ui_playlists_view.lock().unwrap();
        view.previous_view = Some(view.current_view.clone());
        view.previous_url = view.last_fetched_url.clone();
        view.current_view = ViewType::Playlists;
        view.current_index = None;
        view.active_category = CategoryFilter::None;
        let stations = if view.playlists.is_empty() {
            count_lbl_playlists.set_text("No playlist imported");
            Vec::new()
        } else if view.active_bitrate != BitrateFilterSetting::None {
            let filtered = filter_stations_local(view.playlists.clone(), view.active_bitrate);
            let flen = filtered.len();
            count_lbl_playlists.set_text(&format!("{} stations in playlist", flen));
            view.stations = filtered.clone();
            filtered
        } else {
            count_lbl_playlists.set_text(&format!("{} stations in playlist", view.playlists.len()));
            view.playlists.clone()
        };
        drop(view);
        send_local_stations(&tx_pl, ViewType::Playlists, stations, String::new());
    });

    let ui_imp = ui_state.clone();
    let list_imp = list_box.clone();
    let count_imp = stations_count_label.clone();
    let save_mgr_imp = app_state_manager.clone();
    import_playlist_btn.clone().connect_clicked(move |_| {
        let filter = gtk::FileFilter::new();
        filter.add_pattern("*.m3u");
        filter.add_pattern("*.M3U");
        filter.add_pattern("*.pls");
        filter.add_pattern("*.PLS");
        let chooser = gtk::FileChooserNative::builder()
            .title("Import Playlist")
            .action(gtk::FileChooserAction::Open)
            .build();
        chooser.add_filter(&filter);
        if let Some(root) = import_playlist_btn.root() {
            if let Some(parent) = root.downcast_ref::<gtk::Window>() {
                chooser.set_transient_for(Some(parent));
            }
        }
        let vf = ui_imp.clone();
        let lf = list_imp.clone();
        let cf = count_imp.clone();
        let sm = save_mgr_imp.clone();
        let chooser_keep = chooser.clone();
        chooser.connect_response(move |dialog, response| {
            let _keep = &chooser_keep;
            if response == gtk::ResponseType::Accept {
                if let Some(file) = dialog.file() {
                    if let Some(path) = file.path() {
                        begin_request();
                        let content = match read_limited_text(&path, MAX_PLAYLIST_BYTES) {
                            Ok(content) => content,
                            Err(err) => {
                                eprintln!("Failed to read playlist: {}", err);
                                return;
                            }
                        };
                        let parsed = if path
                            .extension()
                            .map(|e| e == "m3u" || e == "M3U")
                            .unwrap_or(false)
                        {
                            parse_m3u(&content)
                        } else {
                            parse_pls(&content)
                        };
                        if !parsed.is_empty() {
                            let mut v = vf.lock().unwrap();
                            let remaining = MAX_PLAYLIST_STATIONS.saturating_sub(v.playlists.len());
                            v.playlists.extend(parsed.into_iter().take(remaining));
                            v.current_view = ViewType::Playlists;
                            v.current_index = None;
                            v.active_category = CategoryFilter::None;
                            let stations = if v.active_bitrate == BitrateFilterSetting::None {
                                v.playlists.clone()
                            } else {
                                filter_stations_local(v.playlists.clone(), v.active_bitrate)
                            };
                            v.stations = stations.clone();
                            drop(v);
                            sm.save_data();
                            while let Some(child) = lf.first_child() {
                                lf.remove(&child);
                            }
                            let vv = vf.lock().unwrap();
                            cf.set_text(&format!("{} stations in playlist", stations.len()));
                            drop(vv);
                            for s in stations.iter() {
                                let row = ActionRow::builder()
                                    .title(markup_text(s.name.trim()))
                                    .subtitle(markup_text(&s.url_resolved))
                                    .title_lines(1)
                                    .subtitle_lines(1)
                                    .tooltip_text(format!("{}\n{}", s.name.trim(), s.url_resolved))
                                    .activatable(true)
                                    .build();
                                let rm_btn = Button::builder()
                                    .icon_name("edit-delete-symbolic")
                                    .css_classes(vec!["flat".to_string()])
                                    .valign(gtk::Align::Center)
                                    .tooltip_text("Remove from playlist")
                                    .build();
                                let view_rm = vf.clone();
                                let list_rm = lf.clone();
                                let count_rm = cf.clone();
                                let row_clone = row.clone();
                                let save_rm = sm.clone();
                                let station_to_remove = s.clone();
                                rm_btn.connect_clicked(move |_| {
                                    begin_request();
                                    let mut v = view_rm.lock().unwrap();
                                    let pos = if station_to_remove.stationuuid.is_empty() {
                                        v.playlists.iter().position(|p| {
                                            p.url_resolved == station_to_remove.url_resolved
                                                && p.name == station_to_remove.name
                                        })
                                    } else {
                                        v.playlists.iter().position(|p| {
                                            p.stationuuid == station_to_remove.stationuuid
                                        })
                                    };
                                    if let Some(pos) = pos {
                                        v.playlists.remove(pos);
                                        v.current_view = ViewType::Playlists;
                                        v.current_index = None;
                                        if v.active_bitrate != BitrateFilterSetting::None {
                                            v.stations = filter_stations_local(
                                                v.playlists.clone(),
                                                v.active_bitrate,
                                            );
                                        }
                                        drop(v);
                                        save_rm.save_data();
                                        list_rm.remove(&row_clone);
                                        let remaining = view_rm.lock().unwrap().playlists.len();
                                        count_rm.set_text(&format!(
                                            "{} stations in playlist",
                                            remaining
                                        ));
                                    }
                                });
                                row.add_suffix(&rm_btn);
                                lf.append(&row);
                            }
                        }
                    }
                }
            }
        });
        chooser.show();
    });

    let clear_recents_ui = ui_state.clone();
    let clear_recents_list = list_box.clone();
    let clear_recents_count = stations_count_label.clone();
    let clear_recents_save = app_state_manager.clone();
    clear_recents_btn.connect_clicked(move |_| {
        begin_request();
        let mut ui = clear_recents_ui.lock().unwrap();
        ui.recent_stations.clear();
        ui.stations.clear();
        ui.current_index = None;
        drop(ui);
        clear_recents_save.save_data();
        while let Some(child) = clear_recents_list.first_child() {
            clear_recents_list.remove(&child);
        }
        clear_recents_count.set_text("0 recently played stations");
    });

    let list_box_br = list_box.clone();
    let browsing_br = now_browsing_label.clone();
    let tx_any = tx.clone();
    let prefs_any = app_prefs.clone();
    let pop_any = bitrate_popover.clone();
    let ui_any = ui_state.clone();
    any_br_btn.connect_clicked(move |_| {
        pop_any.popdown();
        let mut ui = ui_any.lock().unwrap();
        ui.active_bitrate = BitrateFilterSetting::None;
        let view = ui.current_view.clone();
        if matches!(
            view,
            ViewType::Favorites | ViewType::Recent | ViewType::Playlists
        ) {
            let stations = match view {
                ViewType::Favorites => ui.favorites.clone(),
                ViewType::Recent => ui.recent_stations.clone(),
                ViewType::Playlists => ui.playlists.clone(),
                _ => unreachable!(),
            };
            let filtered = filter_stations_local(stations, BitrateFilterSetting::None);
            drop(ui);
            br_btn_for_any.set_label("⊟ Bitrate");
            send_local_stations(&tx_any, view.clone(), filtered, String::new());
            save_mgr_br_any.save_data();
            return;
        }
        let url = build_filtered_url(&ui.active_category, ui.active_bitrate);
        ui.last_fetched_url = url.clone();
        drop(ui);
        br_btn_for_any.set_label("⊟ Bitrate");
        save_mgr_br_any.save_data();
        while let Some(child) = list_box_br.first_child() {
            list_box_br.remove(&child);
        }
        browsing_br.set_text("Loading stations...");
        fetch_data_async(
            url,
            true,
            station_response_view(&view),
            prefs_any.clone(),
            tx_any.clone(),
            false,
        );
    });

    let list_box_low = list_box.clone();
    let browsing_low = now_browsing_label.clone();
    let tx_low = tx.clone();
    let prefs_low = app_prefs.clone();
    let pop_low = bitrate_popover.clone();
    let ui_low = ui_state.clone();
    low_br_btn.connect_clicked(move |_| {
        pop_low.popdown();
        let mut ui = ui_low.lock().unwrap();
        ui.active_bitrate = BitrateFilterSetting::Low;
        let view = ui.current_view.clone();
        if matches!(
            view,
            ViewType::Favorites | ViewType::Recent | ViewType::Playlists
        ) {
            let stations = match view {
                ViewType::Favorites => ui.favorites.clone(),
                ViewType::Recent => ui.recent_stations.clone(),
                ViewType::Playlists => ui.playlists.clone(),
                _ => unreachable!(),
            };
            let filtered = filter_stations_local(stations, BitrateFilterSetting::Low);
            drop(ui);
            br_btn_for_low.set_label("⊟ Low ≤160");
            send_local_stations(&tx_low, view.clone(), filtered, String::new());
            save_mgr_br_low.save_data();
            return;
        }
        let url = build_filtered_url(&ui.active_category, ui.active_bitrate);
        ui.last_fetched_url = url.clone();
        drop(ui);
        br_btn_for_low.set_label("⊟ Low ≤160");
        save_mgr_br_low.save_data();
        while let Some(child) = list_box_low.first_child() {
            list_box_low.remove(&child);
        }
        browsing_low.set_text("Loading stations...");
        fetch_data_async(
            url,
            true,
            station_response_view(&view),
            prefs_low.clone(),
            tx_low.clone(),
            false,
        );
    });

    let list_box_high = list_box.clone();
    let browsing_high = now_browsing_label.clone();
    let tx_high = tx.clone();
    let prefs_high = app_prefs.clone();
    let pop_high = bitrate_popover.clone();
    let ui_high = ui_state.clone();
    high_br_btn.connect_clicked(move |_| {
        pop_high.popdown();
        let mut ui = ui_high.lock().unwrap();
        ui.active_bitrate = BitrateFilterSetting::High;
        let view = ui.current_view.clone();
        if matches!(
            view,
            ViewType::Favorites | ViewType::Recent | ViewType::Playlists
        ) {
            let stations = match view {
                ViewType::Favorites => ui.favorites.clone(),
                ViewType::Recent => ui.recent_stations.clone(),
                ViewType::Playlists => ui.playlists.clone(),
                _ => unreachable!(),
            };
            let filtered = filter_stations_local(stations, BitrateFilterSetting::High);
            drop(ui);
            br_btn_for_high.set_label("⊟ High ≥192");
            send_local_stations(&tx_high, view.clone(), filtered, String::new());
            save_mgr_br_high.save_data();
            return;
        }
        let url = build_filtered_url(&ui.active_category, ui.active_bitrate);
        ui.last_fetched_url = url.clone();
        drop(ui);
        br_btn_for_high.set_label("⊟ High ≥192");
        save_mgr_br_high.save_data();
        while let Some(child) = list_box_high.first_child() {
            list_box_high.remove(&child);
        }
        browsing_high.set_text("Loading stations...");
        fetch_data_async(
            url,
            true,
            station_response_view(&view),
            prefs_high.clone(),
            tx_high.clone(),
            false,
        );
    });

    let list_box_flac = list_box.clone();
    let browsing_flac = now_browsing_label.clone();
    let tx_flac = tx.clone();
    let prefs_flac = app_prefs.clone();
    let pop_flac = bitrate_popover.clone();
    let ui_flac = ui_state.clone();
    flac_br_btn.connect_clicked(move |_| {
        pop_flac.popdown();
        let mut ui = ui_flac.lock().unwrap();
        ui.active_bitrate = BitrateFilterSetting::Flac;
        let view = ui.current_view.clone();
        if matches!(
            view,
            ViewType::Favorites | ViewType::Recent | ViewType::Playlists
        ) {
            let stations = match view {
                ViewType::Favorites => ui.favorites.clone(),
                ViewType::Recent => ui.recent_stations.clone(),
                ViewType::Playlists => ui.playlists.clone(),
                _ => unreachable!(),
            };
            let filtered = filter_stations_local(stations, BitrateFilterSetting::Flac);
            drop(ui);
            br_btn_for_flac.set_label("⊟ FLAC");
            send_local_stations(&tx_flac, view.clone(), filtered, String::new());
            save_mgr_br_flac.save_data();
            return;
        }
        let url = build_filtered_url(&ui.active_category, ui.active_bitrate);
        ui.last_fetched_url = url.clone();
        drop(ui);
        br_btn_for_flac.set_label("⊟ FLAC");
        save_mgr_br_flac.save_data();
        while let Some(child) = list_box_flac.first_child() {
            list_box_flac.remove(&child);
        }
        browsing_flac.set_text("Loading stations...");
        fetch_data_async(
            url,
            true,
            station_response_view(&view),
            prefs_flac.clone(),
            tx_flac.clone(),
            false,
        );
    });

    let state_row_click = player_state.clone();
    let ui_row_click = ui_state.clone();
    let tx_row_click = tx.clone();
    let prefs_row_click = app_prefs.clone();
    let ui_play_row_click = play_pause_btn.clone();
    let title_row_click = now_playing_title.clone();
    let sub_row_click = now_playing_subtitle.clone();
    let meta_row_click = stream_meta_label.clone();
    let info_row_click = stream_info_label.clone();
    let fav_toggle_click = fav_toggle_btn.clone();
    let flag_lbl_row_click = now_playing_flag_lbl.clone();
    let save_mgr_row_click = app_state_manager.clone();
    let prefs_play_row_click = app_prefs.clone();
    let list_box_loading = list_box.clone();
    let browsing_lbl_loading = now_browsing_label.clone();

    list_box.connect_row_activated(move |_, row| {
        let row_index = row.index();
        if row_index < 0 {
            return;
        }
        let index = row_index as usize;
        let mut ui = ui_row_click.lock().unwrap();
        let mode = ui.current_view.clone();

        match mode {
            ViewType::Stations
            | ViewType::Favorites
            | ViewType::Recent
            | ViewType::Playlists
            | ViewType::CustomTag(_) => {
                drop(ui);
                play_station(
                    &state_row_click,
                    &ui_row_click,
                    index,
                    &ui_play_row_click,
                    &title_row_click,
                    &sub_row_click,
                    &meta_row_click,
                    &info_row_click,
                    &fav_toggle_click,
                    &flag_lbl_row_click,
                    &save_mgr_row_click,
                    &prefs_play_row_click,
                );
            }
            ViewType::Tags => {
                let Some(name) = ui.categories.get(index).map(|item| item.name.clone()) else {
                    return;
                };
                ui.previous_view = Some(ui.current_view.clone());
                ui.previous_url = ui.last_fetched_url.clone();
                ui.active_category = CategoryFilter::Tag(name);
                let url = build_filtered_url(&ui.active_category, ui.active_bitrate);
                ui.last_fetched_url = url.clone();
                drop(ui);
                while let Some(child) = list_box_loading.first_child() {
                    list_box_loading.remove(&child);
                }
                browsing_lbl_loading.set_text("Loading stations...");
                fetch_data_async(
                    url,
                    true,
                    ViewType::Stations,
                    prefs_row_click.clone(),
                    tx_row_click.clone(),
                    false,
                );
            }
            ViewType::Languages => {
                let Some(name) = ui.categories.get(index).map(|item| item.name.clone()) else {
                    return;
                };
                ui.previous_view = Some(ui.current_view.clone());
                ui.previous_url = ui.last_fetched_url.clone();
                ui.active_category = CategoryFilter::Language(name);
                let url = build_filtered_url(&ui.active_category, ui.active_bitrate);
                ui.last_fetched_url = url.clone();
                drop(ui);
                while let Some(child) = list_box_loading.first_child() {
                    list_box_loading.remove(&child);
                }
                browsing_lbl_loading.set_text("Loading stations...");
                fetch_data_async(
                    url,
                    true,
                    ViewType::Stations,
                    prefs_row_click.clone(),
                    tx_row_click.clone(),
                    false,
                );
            }
            ViewType::Countries => {
                let Some(name) = ui.categories.get(index).map(|item| item.name.clone()) else {
                    return;
                };
                ui.previous_view = Some(ui.current_view.clone());
                ui.previous_url = ui.last_fetched_url.clone();
                ui.active_category = CategoryFilter::Country(name);
                let url = build_filtered_url(&ui.active_category, ui.active_bitrate);
                ui.last_fetched_url = url.clone();
                drop(ui);
                while let Some(child) = list_box_loading.first_child() {
                    list_box_loading.remove(&child);
                }
                browsing_lbl_loading.set_text("Loading stations...");
                fetch_data_async(
                    url,
                    true,
                    ViewType::Stations,
                    prefs_row_click.clone(),
                    tx_row_click.clone(),
                    false,
                );
            }
        }
    });

    let state_play_pause = player_state.clone();
    let ui_play_pause = ui_state.clone();
    let btn_play_pause_clone = play_pause_btn.clone();
    let np_title_ctrl = now_playing_title.clone();
    let np_sub_ctrl = now_playing_subtitle.clone();
    let meta_lbl_ctrl = stream_meta_label.clone();
    let info_lbl_ctrl = stream_info_label.clone();
    let fav_toggle_ctrl = fav_toggle_btn.clone();
    let flag_lbl_ctrl = now_playing_flag_lbl.clone();
    let save_mgr_play_pause = app_state_manager.clone();
    let prefs_play_pause = app_prefs.clone();

    play_pause_btn.connect_clicked(move |btn| {
        let (current, mode, max_len) = {
            let ui = ui_play_pause.lock().unwrap();
            let fb = ui.active_bitrate != BitrateFilterSetting::None;
            let max_len = match ui.current_view {
                ViewType::Favorites => {
                    if fb {
                        ui.stations.len()
                    } else {
                        ui.favorites.len()
                    }
                }
                ViewType::Recent => {
                    if fb {
                        ui.stations.len()
                    } else {
                        ui.recent_stations.len()
                    }
                }
                ViewType::Playlists => {
                    if fb {
                        ui.stations.len()
                    } else {
                        ui.playlists.len()
                    }
                }
                _ => ui.stations.len(),
            };
            (ui.current_index, ui.current_view.clone(), max_len)
        };
        let mut state = state_play_pause.lock().unwrap();
        if state.currently_playing_station.is_some() {
            match state.pipeline.clone() {
                Some(pipeline) => {
                    if !state.is_paused {
                        match pipeline.set_state(gstreamer::State::Paused) {
                            Ok(_) => {
                                state.is_paused = true;
                                btn.set_icon_name("media-playback-start-symbolic");
                            }
                            Err(err) => {
                                eprintln!("Failed to pause playback: {}", err);
                                invalidate_player(&mut state);
                                btn.set_icon_name("media-playback-start-symbolic");
                                meta_lbl_ctrl.set_text("(Playback unavailable)");
                                info_lbl_ctrl.set_text("");
                            }
                        }
                    } else {
                        match pipeline.set_state(gstreamer::State::Playing) {
                            Ok(_) => {
                                state.is_paused = false;
                                btn.set_icon_name("media-playback-pause-symbolic");
                            }
                            Err(err) => {
                                eprintln!("Failed to resume playback: {}", err);
                                invalidate_player(&mut state);
                                btn.set_icon_name("media-playback-start-symbolic");
                                meta_lbl_ctrl.set_text("(Playback unavailable)");
                                info_lbl_ctrl.set_text("");
                            }
                        }
                    }
                }
                None => {
                    btn.set_icon_name("media-playback-start-symbolic");
                }
            }
        } else {
            drop(state);
            match mode {
                ViewType::Stations
                | ViewType::Favorites
                | ViewType::Recent
                | ViewType::Playlists
                | ViewType::CustomTag(_) => {
                    if let Some(idx) = current {
                        if idx < max_len {
                            play_station(
                                &state_play_pause,
                                &ui_play_pause,
                                idx,
                                &btn_play_pause_clone,
                                &np_title_ctrl,
                                &np_sub_ctrl,
                                &meta_lbl_ctrl,
                                &info_lbl_ctrl,
                                &fav_toggle_ctrl,
                                &flag_lbl_ctrl,
                                &save_mgr_play_pause,
                                &prefs_play_pause,
                            );
                        }
                    }
                }
                _ => {}
            }
        }
    });

    let state_prev = player_state.clone();
    let ui_prev = ui_state.clone();
    let list_box_prev = list_box.clone();
    let btn_prev_ui = play_pause_btn.clone();
    let np_title_prev = now_playing_title.clone();
    let np_sub_prev = now_playing_subtitle.clone();
    let meta_lbl_prev = stream_meta_label.clone();
    let info_lbl_prev = stream_info_label.clone();
    let fav_toggle_prev = fav_toggle_btn.clone();
    let flag_lbl_prev = now_playing_flag_lbl.clone();
    let save_mgr_prev = app_state_manager.clone();
    let prefs_prev = app_prefs.clone();
    prev_btn.connect_clicked(move |_| {
        let ui = ui_prev.lock().unwrap();
        let mode = ui.current_view.clone();
        let current = ui.current_index;
        drop(ui);
        match mode {
            ViewType::Stations
            | ViewType::Favorites
            | ViewType::Recent
            | ViewType::Playlists
            | ViewType::CustomTag(_) => {
                if let Some(idx) = current {
                    if idx > 0 {
                        let new_idx = idx - 1;
                        play_station(
                            &state_prev,
                            &ui_prev,
                            new_idx,
                            &btn_prev_ui,
                            &np_title_prev,
                            &np_sub_prev,
                            &meta_lbl_prev,
                            &info_lbl_prev,
                            &fav_toggle_prev,
                            &flag_lbl_prev,
                            &save_mgr_prev,
                            &prefs_prev,
                        );
                        if let Some(row) = list_box_prev.row_at_index(new_idx as i32) {
                            list_box_prev.select_row(Some(&row));
                        }
                    }
                }
            }
            _ => {}
        }
    });

    let state_next = player_state.clone();
    let ui_next = ui_state.clone();
    let list_box_next = list_box.clone();
    let btn_next_ui = play_pause_btn.clone();
    let np_title_next = now_playing_title.clone();
    let np_sub_next = now_playing_subtitle.clone();
    let meta_lbl_next = stream_meta_label.clone();
    let info_lbl_next = stream_info_label.clone();
    let fav_toggle_next = fav_toggle_btn.clone();
    let flag_lbl_next = now_playing_flag_lbl.clone();
    let save_mgr_next = app_state_manager.clone();
    let prefs_next = app_prefs.clone();
    next_btn.connect_clicked(move |_| {
        let ui = ui_next.lock().unwrap();
        let mode = ui.current_view.clone();
        let current = ui.current_index;
        let fb = ui.active_bitrate != BitrateFilterSetting::None;
        let stations_len = match mode {
            ViewType::Favorites => {
                if fb {
                    ui.stations.len()
                } else {
                    ui.favorites.len()
                }
            }
            ViewType::Recent => {
                if fb {
                    ui.stations.len()
                } else {
                    ui.recent_stations.len()
                }
            }
            ViewType::Playlists => {
                if fb {
                    ui.stations.len()
                } else {
                    ui.playlists.len()
                }
            }
            _ => ui.stations.len(),
        };
        drop(ui);
        match mode {
            ViewType::Stations
            | ViewType::Favorites
            | ViewType::Recent
            | ViewType::Playlists
            | ViewType::CustomTag(_) => {
                if let Some(idx) = current {
                    if idx + 1 < stations_len {
                        let new_idx = idx + 1;
                        play_station(
                            &state_next,
                            &ui_next,
                            new_idx,
                            &btn_next_ui,
                            &np_title_next,
                            &np_sub_next,
                            &meta_lbl_next,
                            &info_lbl_next,
                            &fav_toggle_next,
                            &flag_lbl_next,
                            &save_mgr_next,
                            &prefs_next,
                        );
                        if let Some(row) = list_box_next.row_at_index(new_idx as i32) {
                            list_box_next.select_row(Some(&row));
                        }
                    }
                }
            }
            _ => {}
        }
    });

    let state_fav_toggle = player_state.clone();
    let ui_fav_toggle = ui_state.clone();
    let state_fav_toggle_save = app_state_manager.clone();
    let tx_fav_toggle = tx.clone();
    fav_toggle_btn.connect_clicked(move |btn| {
        let current_station = state_fav_toggle
            .lock()
            .unwrap()
            .currently_playing_station
            .clone();
        if let Some(current_station) = current_station {
            let should_refresh = {
                let ui = ui_fav_toggle.lock().unwrap();
                matches!(
                    ui.current_view,
                    ViewType::Stations
                        | ViewType::Favorites
                        | ViewType::Recent
                        | ViewType::Playlists
                        | ViewType::CustomTag(_)
                )
            };
            if should_refresh {
                begin_request();
            }
            let mut ui = ui_fav_toggle.lock().unwrap();
            if let Some(pos) = ui
                .favorites
                .iter()
                .position(|f| station_eq(f, &current_station))
            {
                ui.favorites.remove(pos);
                btn.set_icon_name("non-starred-symbolic");
            } else {
                ui.favorites.push(current_station);
                btn.set_icon_name("starred-symbolic");
            }
            let current_view = ui.current_view.clone();
            let refresh = match current_view {
                ViewType::Favorites => {
                    let stations = if ui.active_bitrate == BitrateFilterSetting::None {
                        ui.favorites.clone()
                    } else {
                        let filtered =
                            filter_stations_local(ui.favorites.clone(), ui.active_bitrate);
                        ui.stations = filtered.clone();
                        filtered
                    };
                    Some((ViewType::Favorites, stations, INITIAL_STATION_LIMIT, false))
                }
                ViewType::Recent => {
                    let stations = if ui.active_bitrate == BitrateFilterSetting::None {
                        ui.recent_stations.clone()
                    } else {
                        let filtered =
                            filter_stations_local(ui.recent_stations.clone(), ui.active_bitrate);
                        ui.stations = filtered.clone();
                        filtered
                    };
                    Some((ViewType::Recent, stations, INITIAL_STATION_LIMIT, false))
                }
                ViewType::Playlists => {
                    let stations = if ui.active_bitrate == BitrateFilterSetting::None {
                        ui.playlists.clone()
                    } else {
                        let filtered =
                            filter_stations_local(ui.playlists.clone(), ui.active_bitrate);
                        ui.stations = filtered.clone();
                        filtered
                    };
                    Some((ViewType::Playlists, stations, INITIAL_STATION_LIMIT, false))
                }
                view @ (ViewType::Stations | ViewType::CustomTag(_)) => Some((
                    view,
                    ui.stations.clone(),
                    ui.station_limit,
                    ui.station_has_more_hint,
                )),
                ViewType::Tags | ViewType::Languages | ViewType::Countries => None,
            };
            drop(ui);
            state_fav_toggle_save.save_data();
            if let Some((view, stations, limit, has_more_hint)) = refresh {
                send_local_stations_with_limit_hint(
                    &tx_fav_toggle,
                    view,
                    stations,
                    String::new(),
                    limit,
                    has_more_hint,
                );
            }
        }
    });

    let player_vol = player_state.clone();
    let prefs_vol = app_prefs.clone();
    volume_scale.connect_value_changed(move |scale| {
        let vol = scale.value();
        let player = player_vol.lock().unwrap();
        if let Some(pipeline) = &player.pipeline {
            pipeline.set_property("volume", vol);
        }
        drop(player);
        prefs_vol.lock().unwrap().volume = vol;
        // Update icon based on level
        let icon = if vol == 0.0 {
            "audio-volume-muted-symbolic"
        } else if vol < 0.4 {
            "audio-volume-low-symbolic"
        } else if vol < 0.75 {
            "audio-volume-medium-symbolic"
        } else {
            "audio-volume-high-symbolic"
        };
        volume_icon.set_icon_name(Some(icon));
    });

    let list_box_clone = list_box.clone();
    let ui_rx = ui_state.clone();
    let player_rx = player_state.clone();
    let stations_count_label_clone = stations_count_label.clone();
    let now_browsing_label_clone = now_browsing_label.clone();
    let last_updated_label_clone = last_updated_label.clone();
    let app_prefs_categories = app_prefs.clone();
    let custom_tag_btns_categories = custom_tag_buttons_box.clone();
    let tx_categories = tx.clone();
    let state_rx_save = app_state_manager.clone();
    let import_btn_rx = import_btn_clone.clone();
    let clear_recents_rx = clear_recents_btn.clone();
    let br_area_rx = br_area.clone();
    let sw_restore = scrolled_window.clone();

    glib::MainContext::default().spawn_local(async move {
        while let Ok(data) = rx.recv().await {
            let request_id = data.request_id();
            if !is_active_request(request_id) {
                continue;
            }
            let is_load_more = data.is_load_more();
            let keep_scroll = data.keeps_scroll();
            // For load-more keep the list intact (spinner stays visible) — idle_add_local
            // will wipe it on its first tick, replacing the spinner with real rows.
            // For all other navigations clear immediately and reset scroll.
            // A background refresh is redrawn in place by the render callback
            // (clear + rebuild in one tick), so the list never flashes empty.
            if data.changes_list() && !is_load_more && !keep_scroll {
                while let Some(child) = list_box_clone.first_child() {
                    list_box_clone.remove(&child);
                }
                sw_restore.vadjustment().set_value(0.0);
            }

            match data {
                IncomingData::Error { message, .. } => {
                    if is_load_more {
                        if let Some(last_child) = list_box_clone.last_child() {
                            list_box_clone.remove(&last_child);
                        }
                        let can_retry = {
                            let ui = ui_rx.lock().unwrap();
                            matches!(ui.current_view, ViewType::Stations | ViewType::CustomTag(_))
                        };
                        if can_retry {
                            let retry_row = ActionRow::builder()
                                .title("Retry Loading More")
                                .activatable(true)
                                .build();
                            let retry_row_for_handler = retry_row.clone();
                            let tx_retry = tx_categories.clone();
                            let prefs_retry = app_prefs_categories.clone();
                            let ui_retry = ui_rx.clone();
                            let list_retry = list_box_clone.clone();
                            retry_row.connect_activated(move |_| {
                                request_more_stations(
                                    &list_retry,
                                    &retry_row_for_handler,
                                    &tx_retry,
                                    &prefs_retry,
                                    &ui_retry,
                                );
                            });
                            list_box_clone.append(&retry_row);
                        }
                    }
                    br_area_rx.set_visible(true);
                    import_btn_rx.set_visible(false);
                    clear_recents_rx.set_visible(false);
                    now_browsing_label_clone.set_text(&message);
                    stations_count_label_clone.set_text("");
                }
                IncomingData::Stats { sync_time, .. } => {
                    let mut ui = ui_rx.lock().unwrap();
                    ui.last_update_timestamp = sync_time.clone();
                    last_updated_label_clone.set_text(&format!("DB synced: {}", sync_time));
                    if let Some(ref lbl) = *stats_label_chan.borrow() {
                        lbl.set_text(&format!(
                            "Total Working Stations Loaded: {}\nLast Radio-Browser Sync: {}\nTotal Categories / Stations Fetched: {}",
                            ui.last_stations_updated_count,
                            ui.last_update_timestamp,
                            ui.total_elements_fetched,
                        ));
                    }
                    drop(ui);
                    state_rx_save.save_data_in_background();
                }
                IncomingData::Categories { view: view_mode, items, .. } => {
                    br_area_rx.set_visible(true);
                    import_btn_rx.set_visible(false);
                    clear_recents_rx.set_visible(false);
                    // For category views there is no meaningful DB sync time;
                    // show the local fetch time instead.
                    let time_string = local_time_string();
                    last_updated_label_clone.set_text(&format!("Last update: {}", time_string));
                    let mut ui = ui_rx.lock().unwrap();
                    let is_tags_view = view_mode == ViewType::Tags;
                    let is_countries_view = view_mode == ViewType::Countries;
                    ui.current_view = view_mode;
                    ui.categories = items.clone();
                    ui.stations.clear();
                    ui.current_index = None;

                    let browsing_text = match ui.current_view {
                        ViewType::Tags => "Browse by Tag",
                        ViewType::Languages => "Browse by Language",
                        ViewType::Countries => "Browse by Country",
                        _ => "",
                    };
                    now_browsing_label_clone.set_text(browsing_text);
                    ui.last_update_timestamp = time_string;
                    ui.total_elements_fetched = items.len();
                    if let Some(ref lbl) = *stats_label_chan.borrow() {
                        lbl.set_text(&format!(
                            "Total Working Stations Loaded: {}\nLast Radio-Browser Sync: {}\nTotal Categories / Stations Fetched: {}",
                            ui.last_stations_updated_count,
                            ui.last_update_timestamp,
                            ui.total_elements_fetched,
                        ));
                    }
                    match ui.current_view {
                        ViewType::Tags => ui.cached_tags = Some(items.clone()),
                        ViewType::Languages => ui.cached_languages = Some(items.clone()),
                        ViewType::Countries => ui.cached_countries = Some(items.clone()),
                        _ => {}
                    }
                    drop(ui);

                    stations_count_label_clone.set_text(&format!("{} categories displayed", items.len()));

                    for item in items {
                        let title_text = item.name.trim().to_string();
                        let row_title = if is_countries_view {
                            let flag = country_code_to_flag(&item.countrycode);
                            if flag.is_empty() {
                                title_text.clone()
                            } else {
                                format!("{} {}", flag, title_text)
                            }
                        } else {
                            title_text.clone()
                        };
                        let row = ActionRow::builder()
                            .title(markup_text(&row_title))
                            .subtitle(format!("{} active stations available", item.stationcount))
                            .activatable(true)
                            .build();

                        if is_tags_view {
                            let tag_name = title_text.to_lowercase();

                            let add_btn = Button::builder()
                                .icon_name("list-add-symbolic")
                                .css_classes(vec!["flat".to_string()])
                                .tooltip_text(format!("Add \"{}\" to custom tags", tag_name))
                                .valign(gtk::Align::Center)
                                .build();

                            let tag_for_add = tag_name.clone();
                            let prefs_for_add = app_prefs_categories.clone();
                            let btns_for_add = custom_tag_btns_categories.clone();
                            let tx_for_add = tx_categories.clone();
                            let ui_for_add = ui_rx.clone();
                            let save_mgr_cat_add = state_rx_save.clone();
                            let list_box_cat_add = list_box_clone.clone();
                            let browsing_cat_add = now_browsing_label_clone.clone();
                            add_btn.connect_clicked(move |btn| {
                                let already = prefs_for_add.lock().unwrap().custom_tags.contains(&tag_for_add);
                                if already {
                                    btn.set_sensitive(false);
                                    return;
                                }
                                prefs_for_add.lock().unwrap().custom_tags.push(tag_for_add.clone());
                                save_mgr_cat_add.save_data();
                                btn.set_sensitive(false);
                                btn.set_icon_name("object-select-symbolic");

                                let _tag_rb = tag_for_add.clone();
                                let hbox_rb = Box::builder().orientation(Orientation::Horizontal).spacing(6).margin_start(20).halign(gtk::Align::Start).build();
                                let img_rb = gtk::Image::from_icon_name(tag_to_icon(&_tag_rb));
                                img_rb.set_icon_size(gtk::IconSize::Normal);
                                let lbl_rb = Label::builder().label(&_tag_rb).css_classes(vec!["caption".to_string()]).build();
                                hbox_rb.append(&img_rb); hbox_rb.append(&lbl_rb);
                                let nav_btn_rb = Button::builder()
                                    .child(&hbox_rb)
                                    .css_classes(vec!["flat".to_string()])
                                    .hexpand(true)
                                    .tooltip_text(&_tag_rb)
                                    .margin_top(1).margin_bottom(1)
                                    .build();
                                let tc_rb = _tag_rb.clone();
                                let txc_rb = tx_for_add.clone();
                                let uic_rb = ui_for_add.clone();
                                let prefs_rb = prefs_for_add.clone();
                                let btns_for_add_c = btns_for_add.clone();
                                let list_box_custom = list_box_cat_add.clone();
                                let browsing_custom = browsing_cat_add.clone();
                                nav_btn_rb.connect_clicked(move |clicked_btn| {
                                    // Reset all custom tag button icons
                                    let n = btns_for_add_c.observe_children().n_items();
                                    for i in 0..n {
                                        if let Some(child) = btns_for_add_c.observe_children().item(i) {
                        if let Ok(row) = child.downcast::<Box>() {
                                                if let Some(cb) = row.first_child().and_then(|c| c.downcast::<Button>().ok()) {
                                                    let tag_name = cb.tooltip_text().unwrap_or_default();
                                                    if let Some(box_child) = cb.child() {
                                                        if let Some(hb) = box_child.downcast_ref::<Box>() {
                                                            if let Some(img) = hb.first_child().and_then(|i| i.downcast::<gtk::Image>().ok()) {
                                                                img.set_icon_name(Some(tag_to_icon(&tag_name)));
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    // Highlight the active button
                                    if let Some(bc) = clicked_btn.child() {
                                        if let Some(hb) = bc.downcast_ref::<Box>() {
                                            if let Some(im) = hb.first_child() {
                                                if let Some(ic) = im.downcast_ref::<gtk::Image>() {
                                                    ic.set_icon_name(Some("media-playback-start-symbolic"));
                                                }
                                            }
                                        }
                                    }
                                    let mut ui = uic_rb.lock().unwrap();
                                    ui.previous_view = Some(ui.current_view.clone());
                                    ui.previous_url = ui.last_fetched_url.clone();
                                    ui.active_category = CategoryFilter::Tag(tc_rb.clone());
                                    ui.current_view = ViewType::CustomTag(tc_rb.clone());
                                    let url = build_filtered_url(&ui.active_category, ui.active_bitrate);
                                    ui.last_fetched_url = url.clone();
                                    drop(ui);
                                    while let Some(child) = list_box_custom.first_child() {
                                        list_box_custom.remove(&child);
                                    }
                                    browsing_custom.set_text("Loading stations...");
                                    fetch_data_async(url, true, ViewType::CustomTag(tc_rb.clone()), prefs_rb.clone(), txc_rb.clone(), false);
                                });
                                let row_box_rb = Box::builder().orientation(Orientation::Horizontal).spacing(0).hexpand(true).build();
                                let rm_tag_rb = _tag_rb.clone();
                                let prefs_rm_rb = prefs_for_add.clone();
                                let container_rm_rb = btns_for_add.clone();
                                let row_box_rm_rb = row_box_rb.clone();
                                let save_mgr_inner_cat_rm = save_mgr_cat_add.clone();
                                let rm_btn_rb = Button::builder()
                                    .icon_name("window-close-symbolic")
                                    .css_classes(vec!["flat".to_string()])
                                    .margin_top(1).margin_bottom(1)
                                    .tooltip_text("Remove tag")
                                    .build();
                                rm_btn_rb.connect_clicked(move |_| {
                                    prefs_rm_rb.lock().unwrap().custom_tags.retain(|t| t != &rm_tag_rb);
                                    save_mgr_inner_cat_rm.save_data();
                                    container_rm_rb.remove(&row_box_rm_rb);
                                });
                                row_box_rb.append(&nav_btn_rb);
                                row_box_rb.append(&rm_btn_rb);
                                btns_for_add.append(&row_box_rb);
                                btns_for_add.set_visible(true);
                            });

                            let already = app_prefs_categories.lock().unwrap().custom_tags.contains(&tag_name);
                            if already {
                                add_btn.set_sensitive(false);
                                add_btn.set_icon_name("object-select-symbolic");
                            }

                            row.add_suffix(&add_btn);
                        }

                        list_box_clone.append(&row);
                    }
                }
                IncomingData::Stations {
                    view: response_view,
                    stations: stations_list,
                    sync_time: server_sync_time,
                    is_load_more,
                    limit,
                    preserve_index,
                    has_more_hint,
                    ..
                } => {
                    clear_recents_rx.set_visible(false);
                    stations_count_label_clone.set_text("Updating stations...");
                    let mut ui = ui_rx.lock().unwrap();
                    if !matches!(ui.current_view, ViewType::Playlists) {
                        import_btn_rx.set_visible(false);
                    }
                    let display_time = if server_sync_time == "Unknown" || server_sync_time.is_empty() {
                        local_time_string()
                    } else {
                        server_sync_time.clone()
                    };
                    last_updated_label_clone.set_text(&format!("DB synced: {}", display_time));
                    ui.last_update_timestamp = display_time;
                    let prev_view = ui.current_view.clone();
                    ui.current_view = response_view;
                    ui.station_limit = clamp_station_limit(limit);
                    ui.station_has_more_hint = has_more_hint;
                    // Load-more: append only the new page (skipping any station
                    // already listed, in case the server order shifted).
                    // Otherwise replace the list.
                    let (stations_list, row_offset) = if is_load_more {
                        let existing: HashSet<String> = ui
                            .stations
                            .iter()
                            .filter(|s| !s.stationuuid.is_empty())
                            .map(|s| s.stationuuid.clone())
                            .collect();
                        let new_stations: Vec<Station> = stations_list
                            .into_iter()
                            .filter(|s| s.stationuuid.is_empty() || !existing.contains(&s.stationuuid))
                            .collect();
                        let row_offset = ui.stations.len();
                        ui.stations.extend(new_stations.iter().cloned());
                        (new_stations, row_offset)
                    } else {
                        ui.stations = stations_list.clone();
                        (stations_list, 0)
                    };
                    ui.categories.clear();
                    if !is_load_more {
                        ui.current_index = if preserve_index {
                            player_rx
                                .lock()
                                .ok()
                                .and_then(|player| player.currently_playing_station.clone())
                                .and_then(|playing| ui.stations.iter().position(|station| station_eq(station, &playing)))
                        } else {
                            None
                        };
                    }
                    let working_count = ui.stations.iter().filter(|s| s.lastcheckok == 1).count();
                    ui.total_elements_fetched = working_count;
                    ui.last_stations_updated_count = working_count;
                    if let Some(ref lbl) = *stats_label_chan.borrow() {
                        lbl.set_text(&format!(
                            "Total Working Stations Loaded: {}\nLast Radio-Browser Sync: {}\nTotal Categories / Stations Fetched: {}",
                            ui.last_stations_updated_count,
                            ui.last_update_timestamp,
                            ui.total_elements_fetched,
                        ));
                    }

                    let browsing_text = match &ui.current_view {
                        ViewType::CustomTag(t) => Some(format!("Now Browsing Custom Tags > {}", t)),
                        _ => match &ui.active_category {
                            CategoryFilter::Tag(t) => Some(format!("Now browsing: {}", t)),
                            CategoryFilter::Language(l) => Some(format!("Language: {}", l)),
                            CategoryFilter::Country(c) => Some(format!("Country: {}", c)),
                            CategoryFilter::None => match prev_view {
                                ViewType::Favorites => Some("Favorite Stations".to_string()),
                                ViewType::Recent => Some("Recently Played".to_string()),
                                ViewType::Playlists => Some("Imported Playlist".to_string()),
                                _ => None,
                            },
                        },
                    };
                    br_area_rx.set_visible(!matches!(ui.current_view, ViewType::Playlists) && !matches!(prev_view, ViewType::Playlists));
                    let is_playlists_view = matches!(ui.current_view, ViewType::Playlists);
                    let fav_api_uuids: HashSet<String> = ui.favorites.iter().filter(|f| !f.stationuuid.is_empty()).map(|f| f.stationuuid.clone()).collect();
                    let fav_imported: HashSet<(String, String)> = ui.favorites.iter().filter(|f| f.stationuuid.is_empty()).map(|f| (f.url_resolved.clone(), f.name.clone())).collect();
                    let total_stations = ui.stations.len();
                    let current_limit = ui.station_limit;
                    let can_paginate = matches!(ui.current_view, ViewType::Stations | ViewType::CustomTag(_));
                    drop(ui);
                    // Persist the sync timestamp/count off the UI thread so the
                    // fsync doesn't delay the first rows.
                    state_rx_save.save_data_in_background();

                    let display_list: Vec<StationDisplay> = stations_list.into_iter().map(|station| {
                        let title_text = station.name.trim().to_string();
                        let title_escaped = markup_text(&title_text);
                        let country_escaped = station.country.clone();
                        let tags_escaped = station.tags.clone();
                        let flag_str = country_code_to_flag(&station.countrycode);
                        let mut subtitle = if flag_str.is_empty() {
                            format!("{} | {}", country_escaped, tags_escaped)
                        } else {
                            format!("{} {} | {}", flag_str, country_escaped, tags_escaped)
                        };
                        if let Some(br) = station.bitrate {
                            if br > 0 {
                                subtitle = format!("{} | {} kbps", subtitle, br);
                            }
                        }
                        let is_fav = if station.stationuuid.is_empty() {
                            fav_imported.contains(&(station.url_resolved.clone(), station.name.clone()))
                        } else {
                            fav_api_uuids.contains(&station.stationuuid)
                        };
                        let tooltip = format!("{}\n{}", title_text, subtitle);
                        let subtitle = markup_text(&subtitle);
                        StationDisplay { title_escaped, subtitle, tooltip, station, is_fav }
                    }).collect();

                    let display_rc = std::rc::Rc::new(display_list);
                    let current_idx = std::cell::Cell::new(0);
                    let list_box_batch = list_box_clone.clone();
                    let ui_batch = ui_rx.clone();
                    let state_batch = state_rx_save.clone();
                    let browsing_label = now_browsing_label_clone.clone();
                    let browsing_final = browsing_text;
                    let count_label = stations_count_label_clone.clone();
                    let count_text = format!("{} stations displayed", total_stations);
                    let flat_css = vec!["flat".to_string()];
                    let tx_load_more = tx_categories.clone();
                    let prefs_load_more = app_prefs_categories.clone();
                    let ui_load_more = ui_rx.clone();
                    let has_more = can_paginate
                        && has_more_hint
                        && current_limit < MAX_STATION_LIMIT;
                    // New generation so an older render of the same request (e.g.
                    // the cached list still being drawn when its refresh arrives)
                    // stops appending rows.
                    let expected_gen = RENDER_GEN.fetch_add(1, Ordering::AcqRel) + 1;
                    let sw_batch = sw_restore.clone();
                    // HIGH_IDLE runs ahead of GTK's layout/redraw (but after input
                    // events and timers), so all batches are built before the next
                    // frame: one layout pass instead of one per batch, and the
                    // list can't be starved by other redraws (e.g. the analyzer).
                    glib::idle_add_local_full(glib::Priority::HIGH_IDLE, move || {
                        if !is_active_request(request_id)
                            || RENDER_GEN.load(Ordering::Acquire) != expected_gen
                        {
                            return glib::ControlFlow::Break;
                        }
                        // On load-more: remove the spinner row (everything after the
                        // existing station rows) on the first tick, right before
                        // appending the new rows. Existing rows stay untouched.
                        if is_load_more && current_idx.get() == 0 {
                            while let Some(last) = list_box_batch.last_child() {
                                let index = last
                                    .downcast_ref::<gtk::ListBoxRow>()
                                    .map(|row| row.index())
                                    .unwrap_or(-1);
                                if index >= 0 && (index as usize) < row_offset {
                                    break;
                                }
                                list_box_batch.remove(&last);
                            }
                        }
                        let max = display_rc.len();
                        // Background refresh: swap the whole list in one tick so
                        // the scroll position survives.
                        let saved_scroll = if keep_scroll && current_idx.get() == 0 {
                            let value = sw_batch.vadjustment().value();
                            while let Some(child) = list_box_batch.first_child() {
                                list_box_batch.remove(&child);
                            }
                            Some(value)
                        } else {
                            None
                        };
                        // Fill the visible window on the first tick, then render
                        // the rest in larger batches.
                        let batch = if keep_scroll {
                            usize::MAX
                        } else if current_idx.get() == 0 {
                            FIRST_RENDER_BATCH
                        } else {
                            RENDER_BATCH
                        };
                        for _ in 0..batch {
                            let idx = current_idx.get();
                            if idx >= max {
                                if let Some(text) = browsing_final.as_ref() {
                                    browsing_label.set_text(text);
                                }
                                count_label.set_text(&count_text);
                                if has_more {
                                    let load_btn = ActionRow::builder()
                                        .title("Load More Stations")
                                        .subtitle("Load 50 more stations")
                                        .activatable(true)
                                        .build();
                                    let icon = gtk::Image::from_icon_name("go-down-symbolic");
                                    icon.set_valign(gtk::Align::Center);
                                    load_btn.add_prefix(&icon);
                                    let tx_lm = tx_load_more.clone();
                                    let prefs_lm = prefs_load_more.clone();
                                    let ui_lm = ui_load_more.clone();
                                    let lb_lm = list_box_batch.clone();
                                    let load_btn_for_handler = load_btn.clone();
                                    load_btn.connect_activated(move |_| {
                                        request_more_stations(
                                            &lb_lm,
                                            &load_btn_for_handler,
                                            &tx_lm,
                                            &prefs_lm,
                                            &ui_lm,
                                        );
                                    });
                                    list_box_batch.append(&load_btn);
                                }
                                if let Some(value) = saved_scroll {
                                    let adjustment = sw_batch.vadjustment();
                                    adjustment.set_value(value);
                                    // Re-apply after layout in case the new list
                                    // height wasn't known yet.
                                    glib::timeout_add_local_once(Duration::from_millis(30), move || {
                                        if (adjustment.value() - value).abs() > 0.5 {
                                            adjustment.set_value(value);
                                        }
                                    });
                                }
                                return glib::ControlFlow::Break;
                            }
                            let data = &display_rc[idx];
                            // Single-line title/subtitle (ellipsized): wrapping labels
                            // need height-for-width measuring, which made laying out
                            // a 250-row list ~10x slower. Full text is in the tooltip.
                            let row = ActionRow::builder()
                                .title(&data.title_escaped)
                                .subtitle(&data.subtitle)
                                .title_lines(1)
                                .subtitle_lines(1)
                                .tooltip_text(&data.tooltip)
                                .activatable(true)
                                .build();
                            let fav_icon = if data.is_fav { "starred-symbolic" } else { "non-starred-symbolic" };
                            let row_fav_btn = Button::builder()
                                .icon_name(fav_icon)
                                .css_classes(flat_css.clone())
                                .valign(gtk::Align::Center)
                                .build();
                            let station_idx = idx;
                            let display_rc_clone = display_rc.clone();
                            let ui_for_row_fav = ui_batch.clone();
                            let save_mgr_row_fav = state_batch.clone();
                            row_fav_btn.connect_clicked(move |btn| {
                                let mut ui = ui_for_row_fav.lock().unwrap();
                                if let Some(pos) = ui.favorites.iter().position(|f| station_eq(f, &display_rc_clone[station_idx].station)) {
                                    ui.favorites.remove(pos);
                                    btn.set_icon_name("non-starred-symbolic");
                                } else {
                                    ui.favorites.push(display_rc_clone[station_idx].station.clone());
                                    btn.set_icon_name("starred-symbolic");
                                }
                                drop(ui);
                                save_mgr_row_fav.save_data();
                            });
                            row.add_suffix(&row_fav_btn);
                            if is_playlists_view {
                                let rm_btn = Button::builder()
                                    .icon_name("edit-delete-symbolic")
                                    .css_classes(flat_css.clone())
                                    .valign(gtk::Align::Center)
                                    .tooltip_text("Remove from playlist")
                                    .build();
                                let ui_for_rm = ui_batch.clone();
                                let save_rm = state_batch.clone();
                                let list_box_rm = list_box_batch.clone();
                                let count_rm = count_label.clone();
                                let row_rm = row.clone();
                                let station_uuid = data.station.stationuuid.clone();
                                let station_url = data.station.url_resolved.clone();
                                let station_name = data.station.name.clone();
                                rm_btn.connect_clicked(move |_| {
                                    begin_request();
                                    let mut v = ui_for_rm.lock().unwrap();
                                    let pos = if station_uuid.is_empty() {
                                        v.playlists.iter().position(|p| p.url_resolved == station_url && p.name == station_name)
                                    } else {
                                        v.playlists.iter().position(|p| p.stationuuid == station_uuid)
                                    };
                                    if let Some(idx) = pos {
                                        v.playlists.remove(idx);
                                        v.current_index = None;
                                        if v.active_bitrate != BitrateFilterSetting::None {
                                            v.stations = filter_stations_local(
                                                v.playlists.clone(),
                                                v.active_bitrate,
                                            );
                                        }
                                        drop(v);
                                        save_rm.save_data();
                                        list_box_rm.remove(&row_rm);
                                        let remaining = ui_for_rm.lock().unwrap().playlists.len();
                                        count_rm.set_text(&format!("{} stations in playlist", remaining));
                                    }
                                });
                                row.add_suffix(&rm_btn);
                            }
                            list_box_batch.append(&row);
                            current_idx.set(idx + 1);
                        }
                        glib::ControlFlow::Continue
                    });
                }
            }
        }
    });

    let tx_init = tx.clone();
    let prefs_init = app_prefs.clone();
    let target_url = "https://de1.api.radio-browser.info/json/tags?order=stationcount&reverse=true&hidebroken=true".to_string();
    fetch_data_async(
        target_url,
        false,
        ViewType::Tags,
        prefs_init,
        tx_init,
        false,
    );

    let ui_cron_tracker = ui_state.clone();
    let prefs_cron_tracker = app_prefs.clone();
    let tx_cron_tracker = tx.clone();
    std::thread::spawn(move || {
        while !SHUTTING_DOWN.load(Ordering::Acquire) {
            let wants_update = prefs_cron_tracker.lock().unwrap().auto_update;
            if wants_update {
                let (current_url, view, limit) = {
                    let ui = ui_cron_tracker.lock().unwrap();
                    (
                        ui.last_fetched_url.clone(),
                        ui.current_view.clone(),
                        ui.station_limit,
                    )
                };
                // Only auto-refresh live station lists, never local views.
                if matches!(view, ViewType::Stations | ViewType::CustomTag(_)) {
                    let current_url = if limit > INITIAL_STATION_LIMIT {
                        set_url_limit(current_url, limit)
                    } else {
                        current_url
                    };
                    fetch_data_async_preserve_index(
                        current_url,
                        true,
                        view,
                        prefs_cron_tracker.clone(),
                        tx_cron_tracker.clone(),
                        false,
                    );
                }
            }
            for _ in 0..21600 {
                if SHUTTING_DOWN.load(Ordering::Acquire) {
                    return;
                }
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    });

    let state_timer = player_state.clone();
    let time_label_hook = time_label.clone();
    let meta_label_hook = stream_meta_label.clone();
    let info_label_hook = stream_info_label.clone();
    let drawing_analyzer_clone = drawing_analyzer.clone();

    glib::timeout_add_local(Duration::from_millis(70), move || {
        let state = state_timer.lock().unwrap();
        let mut heights = wave_heights.lock().unwrap();
        let mut peaks = peak_heights.lock().unwrap();
        let mut decay_counters = peak_decay_counters.lock().unwrap();

        if let Some(pipeline) = &state.pipeline {
            if !state.is_paused {
                if let Some(position_ns) = pipeline.query_position::<gstreamer::ClockTime>() {
                    time_label_hook
                        .set_text(&format_time(Duration::from_nanos(position_ns.nseconds())));
                }

                for (i, h) in heights.iter_mut().enumerate() {
                    let next_val = glib::random_int_range(4, 48) as f64;
                    *h = next_val;

                    if next_val >= peaks[i] {
                        peaks[i] = next_val;
                        decay_counters[i] = 12;
                    } else if decay_counters[i] > 0 {
                        decay_counters[i] -= 1;
                    } else {
                        peaks[i] = (peaks[i] - 1.8).max(4.0);
                    }
                }
                drop(heights);
                drop(peaks);
                drop(decay_counters);
                drawing_analyzer_clone.queue_draw();

                if let Some(sink) = pipeline.property::<Option<gstreamer::Element>>("audio-sink") {
                    if let Some(pad) = sink.static_pad("sink") {
                        if let Some(caps) = pad.current_caps() {
                            if let Some(structure) = caps.structure(0) {
                                let mut meta = state.current_metadata.lock().unwrap();

                                // Sample rate
                                if let Ok(rate) = structure.get::<i32>("rate") {
                                    meta.sample_rate = Some(rate);
                                }

                                // Bit depth (integer PCM streams expose "width" or "depth")
                                if let Ok(depth) = structure.get::<i32>("depth") {
                                    meta.bit_depth = Some(depth);
                                } else if let Ok(width) = structure.get::<i32>("width") {
                                    meta.bit_depth = Some(width);
                                }

                                // Codec from caps mime name
                                if meta.codec.is_none() {
                                    let mime = structure.name();
                                    meta.codec = match mime.as_str() {
                                        "audio/mpeg" => Some("MP3".to_string()),
                                        "audio/x-flac" => Some("FLAC".to_string()),
                                        "audio/x-vorbis" => Some("Vorbis".to_string()),
                                        "audio/x-opus" => Some("Opus".to_string()),
                                        "audio/x-aac" | "audio/aac" | "audio/mp4a-latm" => {
                                            Some("AAC".to_string())
                                        }
                                        "audio/x-wma" => Some("WMA".to_string()),
                                        "audio/x-alac" => Some("ALAC".to_string()),
                                        "audio/x-speex" => Some("Speex".to_string()),
                                        "audio/x-raw" => Some("PCM".to_string()),
                                        other if other.starts_with("audio/") => Some(
                                            other
                                                .trim_start_matches("audio/x-")
                                                .trim_start_matches("audio/")
                                                .to_uppercase(),
                                        ),
                                        _ => None,
                                    };
                                }
                            }
                        }
                    }
                }

                // Update labels with current metadata (including buffering status)
                {
                    let meta = state.current_metadata.lock().unwrap();

                    // Stream title line
                    let title_str = meta
                        .stream_title
                        .clone()
                        .unwrap_or_else(|| "Live Stream".to_string());
                    meta_label_hook.set_text(&format!("♫ {}", title_str));

                    // Stream info line: codec · bitrate · sample rate · bit depth · buffering
                    let mut parts: Vec<String> = Vec::new();
                    if let Some(ref codec) = meta.codec {
                        parts.push(codec.clone());
                    }
                    if let Some(br) = meta.bitrate {
                        parts.push(format!("{} kbps", br / 1000));
                    }
                    if let Some(sr) = meta.sample_rate {
                        if sr >= 1000 {
                            parts.push(format!("{:.1} kHz", sr as f64 / 1000.0));
                        } else {
                            parts.push(format!("{} Hz", sr));
                        }
                    }
                    if let Some(bd) = meta.bit_depth {
                        parts.push(format!("{}-bit", bd));
                    }
                    if let Some(pct) = meta.buffering_percent {
                        parts.push(format!("⏳ {}%", pct));
                    }
                    if parts.is_empty() {
                        info_label_hook.set_text("");
                    } else {
                        info_label_hook.set_text(&parts.join(" · "));
                    }
                }
            } else {
                for (i, h) in heights.iter_mut().enumerate() {
                    *h = 4.0;
                    peaks[i] = (peaks[i] - 1.5).max(4.0);
                }
                drop(heights);
                drop(peaks);
                drop(decay_counters);
                drawing_analyzer_clone.queue_draw();
            }
        } else {
            time_label_hook.set_text("00:00");
            for (i, h) in heights.iter_mut().enumerate() {
                *h = 4.0;
                peaks[i] = (peaks[i] - 1.5).max(4.0);
            }
            drop(heights);
            drop(peaks);
            drop(decay_counters);
            drawing_analyzer_clone.queue_draw();
        }
        glib::ControlFlow::Continue
    });

    let window = ApplicationWindow::builder()
        .application(app)
        .title("Rust Radio")
        .default_width(1280)
        .default_height(800)
        .resizable(true)
        .content(&content_box)
        .build();
    let save_on_close = app_state_manager.clone();
    let player_on_close = player_state.clone();
    window.connect_close_request(move |_| {
        SHUTTING_DOWN.store(true, Ordering::Release);
        begin_request();
        if let Ok(mut player) = player_on_close.lock() {
            invalidate_player(&mut player);
        }
        save_on_close.save_data();
        gtk::glib::Propagation::Proceed
    });
    window.present();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_urls_are_limited_to_http_schemes() {
        assert!(is_allowed_stream_url("https://radio.example/stream"));
        assert!(is_allowed_stream_url("HTTP://radio.example/stream"));
        assert!(!is_allowed_stream_url("file:///tmp/music.mp3"));
        assert!(!is_allowed_stream_url("smb://server/share"));
        assert!(!is_allowed_stream_url("/tmp/music.mp3"));
        assert!(!is_allowed_stream_url(
            "http://user:password@example.test/stream"
        ));
        assert!(!is_allowed_stream_url("http://127.0.0.1/stream"));
        assert!(!is_allowed_stream_url("http://[fc00::1]/stream"));
        assert!(!is_allowed_stream_url(
            "http://example.test:not-a-port/stream"
        ));
    }

    #[test]
    fn bitrates_in_bps_are_converted() {
        let json = r#"[
            {"stationuuid":"a","name":"A","url_resolved":"https://a.example/s","tags":"","country":"","bitrate":192000},
            {"stationuuid":"b","name":"B","url_resolved":"https://b.example/s","tags":"","country":"","bitrate":320},
            {"stationuuid":"c","name":"C","url_resolved":"https://c.example/s","tags":"","country":"","bitrate":128000},
            {"stationuuid":"d","name":"D","url_resolved":"https://d.example/s","tags":"","country":"","bitrate":null},
            {"stationuuid":"e","name":"E","url_resolved":"https://e.example/s","tags":"","country":""}
        ]"#;
        let stations: Vec<Station> = serde_json::from_str(json).unwrap();
        let rates: Vec<Option<u32>> = stations.iter().map(|s| s.bitrate).collect();
        assert_eq!(rates, [Some(192), Some(320), Some(128), None, None]);
        assert_eq!(normalize_bitrate_kbps(10_000), 10_000);
        assert_eq!(normalize_bitrate_kbps(127_999), 128);

        // "High" filter + bitrate order: the 128000-bps station is dropped and
        // the 192000-bps one sorts below 320 kbps.
        let url = "https://h/json/stations/search?bitrateMin=192&order=bitrate&reverse=true&limit=250";
        let ids: Vec<String> = apply_bitrate_query_locally(url, stations.clone())
            .into_iter()
            .map(|s| s.stationuuid)
            .collect();
        assert_eq!(ids, ["b", "a", "d", "e"]);
        // No filter / order params: untouched.
        let same = apply_bitrate_query_locally("https://h/json/stations/byname/x", stations.clone());
        assert_eq!(same, stations);
    }

    #[test]
    fn page_params_are_set_and_replaced() {
        let base = "https://h/json/stations/search?tag=rock&order=bitrate&reverse=true".to_string();
        let url = set_url_offset(set_url_limit(base, 50), 250);
        assert_eq!(parse_url_limit(&url), Some(50));
        assert_eq!(parse_url_offset(&url), Some(250));
        let url = set_url_offset(url, 300);
        assert_eq!(parse_url_offset(&url), Some(300));
        assert_eq!(url.matches("offset=").count(), 1);
        // A search-term value containing "limit=" must not be mistaken for the param.
        let url = "https://h/s?name=xlimit=9&limit=20";
        assert_eq!(parse_url_limit(url), Some(20));
        // Trailing '&' (as produced by build_filtered_url bases) gets no extra separator.
        assert_eq!(
            set_url_limit("https://h/s?tag=a&".to_string(), 5),
            "https://h/s?tag=a&limit=5"
        );
    }

    #[test]
    fn station_limits_are_parsed_and_bounded() {
        assert_eq!(
            parse_url_limit("https://example.test/search?limit=300&order=bitrate"),
            Some(300)
        );
        assert_eq!(
            parse_url_limit("https://example.test/search?limit=abc"),
            None
        );
        assert_eq!(clamp_station_limit(1), INITIAL_STATION_LIMIT);
        assert_eq!(
            clamp_station_limit(MAX_STATION_LIMIT + 1),
            MAX_STATION_LIMIT
        );
    }
}
