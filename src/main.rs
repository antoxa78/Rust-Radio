use adw::prelude::*;
use gtk::prelude::Cast;
use gstreamer::prelude::*;
use gstreamer::prelude::ObjectExt as GstObjectExt;
use adw::{ActionRow, ApplicationWindow, HeaderBar, PreferencesGroup, PreferencesPage, ComboRow};
use gtk::{Application, Box, Button, DrawingArea, Label, ListBox, MenuButton, Orientation, Paned, Popover, Scale, ScrolledWindow, SearchEntry, Switch, StringList};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use std::collections::HashSet;
use std::time::Duration;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
struct Station {
    stationuuid: String,
    name: String,
    url_resolved: String,
    tags: String,
    country: String,
    #[serde(default)]
    countrycode: String,
    lastcheckok: i32,
    bitrate: Option<u32>,
}

struct StationDisplay {
    title_escaped: String,
    subtitle: String,
    station: Station,
    is_fav: bool,
}

#[derive(Deserialize, Debug, Clone)]
struct BrowseItem {
    name: String,
    stationcount: u32,
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
    cached_tags: Option<Vec<BrowseItem>>,
    cached_languages: Option<Vec<BrowseItem>>,
    cached_countries: Option<Vec<BrowseItem>>,
}

struct PlayerState {
    pipeline: Option<gstreamer::Element>,
    bus_guard: Option<gstreamer::bus::BusWatchGuard>,
    is_paused: bool,
    current_metadata: Arc<Mutex<AudioMetadata>>,
    currently_playing_station: Option<Station>,
}

// --- PERSISTENCE ---
fn default_bitrate_str() -> String { "None".to_string() }

fn default_audio_sink() -> String { "autoaudiosink".to_string() }

#[derive(Serialize, Deserialize, Default)]
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
}

struct AppState {
    ui: Arc<Mutex<UIState>>,
    prefs: Arc<Mutex<AppPreferences>>,
}

impl AppState {
    fn save_data(&self) {
        let mut path = gtk::glib::user_config_dir();
        path.push("RustRadioGTK");
        let _ = std::fs::create_dir_all(&path);
        path.push("saved_data.json");

        let ui = self.ui.lock().unwrap();
        let favorites = ui.favorites.clone();
        let recent_stations = ui.recent_stations.clone();
        let last_sync_timestamp = ui.last_update_timestamp.clone();
        let playlists = ui.playlists.clone();
        let active_bitrate = match ui.active_bitrate {
            BitrateFilterSetting::None => "None",
            BitrateFilterSetting::Low  => "Low",
            BitrateFilterSetting::High => "High",
            BitrateFilterSetting::Flac => "Flac",
        }.to_string();
        drop(ui);
        let prefs = self.prefs.lock().unwrap();
        let custom_tags = prefs.custom_tags.clone();
        let audio_sink = prefs.audio_sink.clone();
        drop(prefs);

        let data = SavedData {
            favorites,
            custom_tags,
            active_bitrate,
            recent_stations,
            last_sync_timestamp,
            playlists,
            audio_sink,
        };

        if let Ok(json) = serde_json::to_string(&data) {
            let _ = std::fs::write(path, json);
        }
    }
    
    fn load_data() -> SavedData {
        let mut path = gtk::glib::user_config_dir();
        path.push("RustRadioGTK");
        path.push("saved_data.json");

        std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }
}

fn main() {
    gstreamer::init().expect("Failed to initialize GStreamer.");
    
    let application = Application::builder()
        .application_id("com.example.RustRadioGtk")
        .build();

    application.connect_activate(build_ui);
    application.run();
}

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
    let mut ui = ui_lock.lock().unwrap();
    let station = match ui.current_view {
        ViewType::Favorites => {
            if index >= ui.favorites.len() { return; }
            ui.current_index = Some(index);
            ui.favorites[index].clone()
        }
        ViewType::Recent => {
            if index >= ui.recent_stations.len() { return; }
            ui.current_index = Some(index);
            ui.recent_stations[index].clone()
        }
        ViewType::Playlists => {
            if index >= ui.playlists.len() { return; }
            ui.current_index = Some(index);
            ui.playlists[index].clone()
        }
        _ => {
            if index >= ui.stations.len() { return; }
            ui.current_index = Some(index);
            ui.stations[index].clone()
        }
    };
    
    // Track recently played (keep last 50, no duplicates)
    ui.recent_stations.retain(|s| s.stationuuid != station.stationuuid);
    ui.recent_stations.insert(0, station.clone());
    ui.recent_stations.truncate(50);
    
    if ui.favorites.iter().any(|f| f.stationuuid == station.stationuuid) {
        fav_toggle_btn.set_icon_name("starred-symbolic");
    } else {
        fav_toggle_btn.set_icon_name("non-starred-symbolic");
    }
    drop(ui);
    save_mgr.save_data();

    let mut player = player_lock.lock().unwrap();
    player.is_paused = false;
    player.currently_playing_station = Some(station.clone());
    play_pause_btn.set_icon_name("media-playback-pause-symbolic");

    now_playing_title.set_text(&glib::markup_escape_text(&station.name));
    let br_info = station.bitrate.map_or(String::new(), |b| format!(" ({} kbps)", b));
    now_playing_subtitle.set_text(&format!("{} | {}{}", glib::markup_escape_text(&station.country), glib::markup_escape_text(&station.tags), br_info));
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
    }
    stream_meta_label.set_text("(Connecting...)");
    stream_info_label.set_text("");

    let url = station.url_resolved.clone();
    let sink_name = prefs.lock().unwrap().audio_sink.clone();
    player.bus_guard = None;
    if let Some(old_pipeline) = player.pipeline.take() {
        let _ = old_pipeline.set_state(gstreamer::State::Null);
    }

    let audio_sink = gstreamer::ElementFactory::make(&sink_name).build()
        .or_else(|_| gstreamer::ElementFactory::make("autoaudiosink").build())
        .ok();

    let mut playbin_builder = gstreamer::ElementFactory::make("playbin")
        .property("uri", &url);
    if let Some(ref sink) = audio_sink {
        playbin_builder = playbin_builder.property("audio-sink", sink);
    }
    if let Ok(pipeline) = playbin_builder.build()
    {
        let _ = pipeline.set_state(gstreamer::State::Playing);
        let bus = pipeline.bus().unwrap();
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

        let guard = bus.add_watch_local(move |_, msg| {
            match msg.view() {
                gstreamer::MessageView::Tag(tags_msg) => {
                    let tags = tags_msg.tags();
                    let mut meta = meta_rc.lock().unwrap();
                    
                    if let Some(title) = tags.index::<gstreamer::tags::Title>(0) {
                        meta.stream_title = Some(title.get().to_string());
                    } else if let Some(artist) = tags.index::<gstreamer::tags::Artist>(0) {
                        meta.stream_title = Some(artist.get().to_string());
                    }
                    if let Some(bitrate) = tags.index::<gstreamer::tags::Bitrate>(0) {
                        meta.bitrate = Some(bitrate.get());
                    }
                    if meta.codec.is_none() {
                        if let Some(codec) = tags.index::<gstreamer::tags::AudioCodec>(0) {
                            meta.codec = Some(codec.get().to_string());
                        }
                    }

                    let title_str = meta.stream_title.clone().unwrap_or_else(|| "Live Stream".to_string());
                    label_rc.set_text(&format!("♫ {}", title_str));
                }
                gstreamer::MessageView::Error(err) => {
                    eprintln!("Playback Error encountered: {}. Attempting fallback track...", err.error());
                    
                    let next_index = {
                        let ui = ui_fallback.lock().unwrap();
                        ui.current_index.map(|idx| idx + 1)
                    };

                    if let Some(idx) = next_index {
                        let total = {
                            let ui = ui_fallback.lock().unwrap();
                            match ui.current_view {
                                ViewType::Favorites => ui.favorites.len(),
                                ViewType::Recent => ui.recent_stations.len(),
                                ViewType::Playlists => ui.playlists.len(),
                                _ => ui.stations.len(),
                            }
                        };
                        
                        if idx < total {
                            let p_rc = player_fallback.clone();
                            let u_rc = ui_fallback.clone();
                            let b_rc = btn_fallback.clone();
                            let t_rc = title_fallback.clone();
                            let s_rc = sub_fallback.clone();
                            let m_rc = meta_lbl_fallback.clone();
                            let i_rc = info_lbl_fallback.clone();
                            let f_rc = fav_fallback.clone();
                            let flag_lbl_rc = flag_lbl_fallback.clone();
                            let save_mgr_rc = save_mgr_fallback.clone();
                            let prefs_rc = prefs_fallback.clone();
                            
                            glib::idle_add_local_once(move || {
                                play_station(&p_rc, &u_rc, idx, &b_rc, &t_rc, &s_rc, &m_rc, &i_rc, &f_rc, &flag_lbl_rc, &save_mgr_rc, &prefs_rc);
                            });
                        }
                    }
                }
                _ => {}
            }
            glib::ControlFlow::Continue
        }).expect("Failed to hook message bus loop watch context.");

        player.bus_guard = Some(guard);
        player.pipeline = Some(pipeline);
    } else {
        eprintln!("Failed to create pipeline for {}, skipping...", url);
        drop(player);
        let next_index = index + 1;
        let total = {
            let ui = ui_lock.lock().unwrap();
            match ui.current_view {
                ViewType::Favorites => ui.favorites.len(),
                ViewType::Recent => ui.recent_stations.len(),
                ViewType::Playlists => ui.playlists.len(),
                _ => ui.stations.len(),
            }
        };
        if next_index < total {
            let p_rc = player_lock.clone();
            let u_rc = ui_lock.clone();
            let b_rc = play_pause_btn.clone();
            let t_rc = now_playing_title.clone();
            let s_rc = now_playing_subtitle.clone();
            let m_rc = stream_meta_label.clone();
            let i_rc = stream_info_label.clone();
            let f_rc = fav_toggle_btn.clone();
            let flag_lbl_rc = now_playing_flag_lbl.clone();
            let save_mgr_rc = save_mgr.clone();
            let prefs_rc = prefs.clone();
            glib::idle_add_local_once(move || {
                play_station(&p_rc, &u_rc, next_index, &b_rc, &t_rc, &s_rc, &m_rc, &i_rc, &f_rc, &flag_lbl_rc, &save_mgr_rc, &prefs_rc);
            });
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
            stations.push(Station {
                stationuuid: String::new(),
                name: if next_name.is_empty() { trimmed.to_string() } else { next_name.clone() },
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
                if !url.is_empty() {
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
    code.chars().filter_map(|c| {
        if c.is_ascii_alphabetic() {
            Some(char::from_u32(0x1F1E6 + (c.to_ascii_uppercase() as u32 - 'A' as u32)).unwrap())
        } else {
            None
        }
    }).collect()
}

fn format_time(duration: Duration) -> String {
    let total_secs = duration.as_secs();
    format!("{:02}:{:02}", total_secs / 60, total_secs % 60)
}



enum IncomingData {
    Categories(ViewType, Vec<BrowseItem>),
    Stations(Vec<Station>, String),   // String = server DB last-change time from radio-browser.info
}

fn fetch_data_async(
    mut url: String,
    is_stations: bool,
    target_view: ViewType,
    prefs: Arc<Mutex<AppPreferences>>,
    tx: async_channel::Sender<IncomingData>,
) {
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let config = prefs.lock().unwrap();

            if is_stations {
                let sep = |u: &str| if u.contains('?') { "&" } else { "?" };
                if config.hide_broken {
                    url.push_str(&format!("{}lastcheckok=1", sep(&url)));
                }
                if config.prefer_high_bitrate {
                    url.push_str(&format!("{}order=bitrate&reverse=true", sep(&url)));
                }
            }
            drop(config);

            let client = reqwest::Client::new();
            if is_stations {
                let (stations_fut, stats_fut) = tokio::join!(
                    client.get(&url).header("User-Agent", "RustRadioGTK/1.0").send(),
                    client.get("https://de1.api.radio-browser.info/json/stats")
                        .header("User-Agent", "RustRadioGTK/1.0")
                        .send(),
                );
                if let Ok(response) = stations_fut {
                    if let Ok(stations) = response.json::<Vec<Station>>().await {
                        let sync_time = match stats_fut {
                            Ok(resp) => resp
                                .json::<RadioBrowserStats>()
                                .await
                                .map(|s| s.lastchangetime)
                                .unwrap_or_else(|_| "Unknown".to_string()),
                            Err(_) => "Unknown".to_string(),
                        };
                        let _ = tx.send_blocking(IncomingData::Stations(
                            stations.into_iter().take(250).collect(),
                            sync_time,
                        ));
                    }
                }
            } else {
                if let Ok(response) = client.get(&url).header("User-Agent", "RustRadioGTK/1.0").send().await {
                    if let Ok(cats) = response.json::<Vec<BrowseItem>>().await {
                        let filtered = cats.into_iter().filter(|c| !c.name.trim().is_empty()).take(100).collect();
                        let _ = tx.send_blocking(IncomingData::Categories(target_view, filtered));
                    }
                }
            }
        });
    });
}

fn build_filtered_url(category: &CategoryFilter, bitrate: BitrateFilterSetting) -> String {
    let base_endpoint = match category {
        CategoryFilter::None => "https://de1.api.radio-browser.info/json/stations/search?".to_string(),
        CategoryFilter::Tag(name) => format!("https://de1.api.radio-browser.info/json/stations/search?tag={}&", urlencoding::encode(name)),
        CategoryFilter::Language(name) => format!("https://de1.api.radio-browser.info/json/stations/search?language={}&", urlencoding::encode(name)),
        CategoryFilter::Country(name) => format!("https://de1.api.radio-browser.info/json/stations/search?country={}&", urlencoding::encode(name)),
    };

    match bitrate {
        BitrateFilterSetting::None => format!("{}order=bitrate&reverse=true", base_endpoint),
        BitrateFilterSetting::Low => format!("{}bitrateMax=160&order=bitrate&reverse=true", base_endpoint),
        BitrateFilterSetting::High => format!("{}bitrateMin=192&order=bitrate&reverse=true", base_endpoint),
        BitrateFilterSetting::Flac => {
            if let CategoryFilter::Tag(_) = category {
                format!("{}bitrateMin=500&order=bitrate&reverse=true", base_endpoint)
            } else {
                format!("{}tag=flac&order=bitrate&reverse=true", base_endpoint)
            }
        }
    }
}

fn tag_to_icon(tag: &str) -> &'static str {
    let t = tag.to_lowercase();

    // --- Audio quality / format ---
    if t.contains("flac") || t.contains("lossless") || t.contains("hifi") || t.contains("hi-fi") || t.contains("audiophile") || t.contains("320") || t.contains("high quality") { return "audio-headphones-symbolic"; }

    // --- Genres: electronic / club ---
    if t.contains("electron") || t.contains("techno") || t.contains("house") || t.contains("trance") || t.contains("edm") || t.contains("dance") || t.contains("dubstep") || t.contains("drum and bass") || t.contains("drum & bass") || t.contains("dnb") || t.contains("d&b") || t.contains("jungle") || t.contains("breakbeat") || t.contains("hardcore") || t.contains("rave") { return "utilities-terminal-symbolic"; }

    // --- Genres: hip-hop / urban ---
    if t.contains("hip-hop") || t.contains("hiphop") || t.contains("hip hop") || t.contains("rap") || t.contains("trap") || t.contains("grime") || t.contains("drill") { return "microphone-sensitivity-high-symbolic"; }

    // --- Genres: rock / metal ---
    if t.contains("metal") || t.contains("punk") || t.contains("grunge") || t.contains("hardcore") || t.contains("thrash") || t.contains("doom") || t.contains("black metal") || t.contains("heavy") { return "media-optical-cd-audio-symbolic"; }
    if t.contains("rock") { return "media-optical-cd-audio-symbolic"; }

    // --- Genres: classical / orchestral ---
    if t.contains("classical") || t.contains("orchestra") || t.contains("symphony") || t.contains("opera") || t.contains("chamber") || t.contains("baroque") || t.contains("choral") || t.contains("philharmonic") { return "emblem-music-symbolic"; }

    // --- Genres: jazz / swing / big band ---
    if t.contains("jazz") || t.contains("swing") || t.contains("big band") || t.contains("bebop") || t.contains("dixieland") || t.contains("blues") || t.contains("boogie") { return "audio-x-generic-symbolic"; }

    // --- Genres: soul / R&B / funk ---
    if t.contains("soul") || t.contains("funk") || t.contains("rnb") || t.contains("r&b") || t.contains("motown") || t.contains("gospel") || t.contains("rhythm") { return "emblem-favorite-symbolic"; }

    // --- Genres: ambient / chill / new age ---
    if t.contains("ambient") || t.contains("chill") || t.contains("lofi") || t.contains("lo-fi") || t.contains("relax") || t.contains("meditation") || t.contains("sleep") || t.contains("new age") || t.contains("newage") || t.contains("nature") || t.contains("spa") || t.contains("healing") { return "weather-clear-night-symbolic"; }

    // --- Genres: pop ---
    if t.contains("pop") { return "starred-symbolic"; }

    // --- Genres: country / folk ---
    if t.contains("country") || t.contains("folk") || t.contains("bluegrass") || t.contains("acoustic") || t.contains("singer-songwriter") || t.contains("americana") { return "find-location-symbolic"; }

    // --- Genres: Latin / tropical ---
    if t.contains("latin") || t.contains("salsa") || t.contains("reggaeton") || t.contains("cumbia") || t.contains("bachata") || t.contains("bossa") || t.contains("samba") || t.contains("merengue") || t.contains("tango") || t.contains("flamenco") || t.contains("fado") { return "emoji-activities-symbolic"; }

    // --- Genres: reggae / ska ---
    if t.contains("reggae") || t.contains("ska") || t.contains("dub") || t.contains("dancehall") { return "weather-clear-symbolic"; }

    // --- Genres: world / cultural ---
    if t.contains("world") || t.contains("international") || t.contains("global") || t.contains("multicultural") { return "applications-internet-symbolic"; }

    // --- Genres: instrumental ---
    if t.contains("instrumental") || t.contains("piano") || t.contains("guitar") || t.contains("violin") || t.contains("cello") || t.contains("trumpet") || t.contains("saxophone") || t.contains("orchestra") { return "emblem-music-symbolic"; }

    // --- Talk / news / speech ---
    if t.contains("news") || t.contains("talk") || t.contains("speech") || t.contains("podcast") || t.contains("interview") || t.contains("debate") || t.contains("current affairs") { return "dialog-information-symbolic"; }

    // --- Sports ---
    if t.contains("sport") || t.contains("football") || t.contains("soccer") || t.contains("basketball") || t.contains("baseball") || t.contains("cricket") || t.contains("rugby") || t.contains("tennis") { return "applications-games-symbolic"; }

    // --- Religious ---
    if t.contains("christian") || t.contains("gospel") || t.contains("spiritual") || t.contains("religion") || t.contains("worship") || t.contains("church") || t.contains("catholic") || t.contains("orthodox") || t.contains("islamic") || t.contains("quran") || t.contains("jewish") || t.contains("buddhist") { return "emblem-default-symbolic"; }

    // --- Children / education ---
    if t.contains("child") || t.contains("kids") || t.contains("school") || t.contains("education") || t.contains("learn") || t.contains("student") || t.contains("college") || t.contains("university") || t.contains("campus") { return "face-smile-symbolic"; }

    // --- Comedy / entertainment ---
    if t.contains("comedy") || t.contains("humor") || t.contains("humour") || t.contains("satire") { return "face-laugh-symbolic"; }

    // --- Decades / era ---
    if t.contains("50s") || t.contains("60s") || t.contains("70s") || t.contains("80s") || t.contains("90s") || t.contains("2000s") || t.contains("oldies") || t.contains("vintage") || t.contains("retro") || t.contains("golden") || t.contains("classic hits") || t.contains("throwback") { return "document-open-recent-symbolic"; }

    // --- Indie / alternative ---
    if t.contains("indie") || t.contains("alternative") { return "media-record-symbolic"; }

    // --- Live / events ---
    if t.contains("live") || t.contains("concert") || t.contains("festival") { return "media-record-symbolic"; }

    // --- Charts / variety ---
    if t.contains("hits") || t.contains("chart") || t.contains("top 40") || t.contains("variety") || t.contains("mix") { return "view-list-symbolic"; }

    // --- Seasonal ---
    if t.contains("christmas") || t.contains("holiday") || t.contains("seasonal") || t.contains("xmas") { return "starred-symbolic"; }

    // --- Regional: East Asian ---
    if t.contains("japanese") || t.contains("j-pop") || t.contains("j-rock") || t.contains("anime") { return "applications-internet-symbolic"; }
    if t.contains("korean") || t.contains("k-pop") || t.contains("k-drama") { return "applications-internet-symbolic"; }
    if t.contains("chinese") || t.contains("mandarin") || t.contains("cantonese") || t.contains("taiwanese") { return "applications-internet-symbolic"; }

    // --- Regional: South / Southeast Asian ---
    if t.contains("indian") || t.contains("hindi") || t.contains("bollywood") || t.contains("bhangra") || t.contains("carnatic") || t.contains("filmi") { return "emoji-activities-symbolic"; }
    if t.contains("bengali") || t.contains("tamil") || t.contains("telugu") || t.contains("malayalam") || t.contains("punjabi") { return "emoji-activities-symbolic"; }
    if t.contains("thai") || t.contains("vietnamese") || t.contains("indonesian") || t.contains("philippine") || t.contains("malay") { return "emoji-activities-symbolic"; }

    // --- Regional: Middle Eastern ---
    if t.contains("arabic") || t.contains("arab") || t.contains("khaleeji") || t.contains("middle east") || t.contains("levant") { return "weather-clear-symbolic"; }
    if t.contains("persian") || t.contains("farsi") || t.contains("iranian") { return "weather-clear-symbolic"; }
    if t.contains("turkish") || t.contains("turk") { return "weather-clear-symbolic"; }
    if t.contains("kurdish") { return "weather-clear-symbolic"; }

    // --- Regional: African ---
    if t.contains("african") || t.contains("afro") || t.contains("afrobeat") || t.contains("afropop") || t.contains("highlife") || t.contains("juju") || t.contains("mbalax") { return "find-location-symbolic"; }

    // --- Regional: Eastern European / Slavic ---
    if t.contains("russian") || t.contains("ukraine") || t.contains("ukrainian") || t.contains("polish") || t.contains("czech") || t.contains("slovak") || t.contains("balkan") || t.contains("serbian") || t.contains("croatian") || t.contains("bulgarian") || t.contains("romanian") { return "emblem-music-symbolic"; }

    // --- Regional: Western / Northern European ---
    if t.contains("french") || t.contains("chanson") { return "emblem-favorite-symbolic"; }
    if t.contains("german") || t.contains("deutsch") || t.contains("schlager") || t.contains("volksmusik") { return "emblem-music-symbolic"; }
    if t.contains("spanish") || t.contains("espanol") { return "emoji-activities-symbolic"; }
    if t.contains("italian") || t.contains("italiano") || t.contains("cantautore") { return "emblem-favorite-symbolic"; }
    if t.contains("greek") || t.contains("laika") || t.contains("rebetiko") { return "weather-clear-symbolic"; }
    if t.contains("celtic") || t.contains("irish") || t.contains("scottish") || t.contains("welsh") || t.contains("breton") { return "find-location-symbolic"; }
    if t.contains("nordic") || t.contains("scandinavian") || t.contains("swedish") || t.contains("norwegian") || t.contains("danish") || t.contains("finnish") { return "weather-clear-night-symbolic"; }

    // --- Missing genre gaps ---
    if t.contains("soundtrack") || t.contains("film") || t.contains("movie") || t.contains("cinema") || t.contains("score") { return "video-display-symbolic"; }
    if t.contains("industrial") || t.contains("noise") { return "utilities-terminal-symbolic"; }
    if t.contains("new wave") || t.contains("post-punk") || t.contains("prog") || t.contains("progressive") { return "media-record-symbolic"; }
    if t.contains("lounge") || t.contains("easy listening") || t.contains("beautiful music") || t.contains("adult contemporary") { return "starred-symbolic"; }
    if t.contains("roots") { return "find-location-symbolic"; }
    if t.contains("jazz fusion") { return "audio-x-generic-symbolic"; }
    if t.contains("trip-hop") || t.contains("triphop") || t.contains("downtempo") || t.contains("down tempo") { return "weather-clear-night-symbolic"; }
    if t.contains("a cappella") || t.contains("acappella") || t.contains("vocal") || t.contains("choir") { return "audio-input-microphone-symbolic"; }
    if t.contains("karaoke") || t.contains("instrumentals") { return "media-record-symbolic"; }
    if t.contains("audiobook") || t.contains("audio book") || t.contains("audio drama") { return "dialog-information-symbolic"; }
    if t.contains("public") || t.contains("npr") || t.contains("community") || t.contains("traffic") || t.contains("weather") || t.contains("emergency") { return "dialog-information-symbolic"; }
    if t.contains("top") || t.contains("greatest") || t.contains("request") || t.contains("today") { return "starred-symbolic"; }
    if t.contains("urban") || t.contains("contemporary") { return "emblem-favorite-symbolic"; }
    if t.contains("political") || t.contains("politics") || t.contains("government") { return "dialog-information-symbolic"; }

    // --- General music / radio (last resort before fallback) ---
    if t.contains("music") || t.contains("musica") || t.contains("musik") { return "audio-x-generic-symbolic"; }
    if t.contains("radio") || t.contains("broadcast") || t.contains("stream") { return "audio-input-microphone-symbolic"; }

    "audio-x-generic-symbolic"
}

fn build_ui(app: &Application) {
    adw::init().unwrap();

    let provider = gtk::CssProvider::new();
    provider.load_from_data("
        list row:selected { background-color: alpha(currentColor, 0.12); }
    ");
    gtk::style_context_add_provider_for_display(
        &gtk::gdk::Display::default().expect("Could not connect to system display context tracker."),
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );

    // --- REHYDRATE FROM DISK ---
    let saved_data = AppState::load_data();

    let current_metadata = Arc::new(Mutex::new(AudioMetadata { bitrate: None, sample_rate: None, stream_title: None, codec: None, bit_depth: None }));
    let player_state = Arc::new(Mutex::new(PlayerState { pipeline: None, bus_guard: None, is_paused: false, current_metadata, currently_playing_station: None }));
    
    let app_prefs = Arc::new(Mutex::new(AppPreferences { 
        prefer_high_bitrate: true, 
        hide_broken: true, 
        auto_update: true,
        analyzer_type: AnalyzerType::BlocksWithHold,
        custom_tags: saved_data.custom_tags,
        audio_sink: saved_data.audio_sink,
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
        total_elements_fetched: 0,
        last_stations_updated_count: 0,
        cached_tags: None,
        cached_languages: None,
        cached_countries: None,
    }));

    let app_state_manager = Arc::new(AppState { ui: ui_state.clone(), prefs: app_prefs.clone() });

    let content_box = Box::new(Orientation::Vertical, 0);

    // --- Header Bar & Context Menus ---
    let header_bar = HeaderBar::new();
    let menu_btn = MenuButton::builder().icon_name("open-menu-symbolic").build();
    let menu_popover = Popover::new();
    let menu_box = Box::new(Orientation::Vertical, 6);
    
    let settings_btn = Button::builder().label("Preferences").has_frame(false).build();
    menu_box.append(&settings_btn);
    menu_popover.set_child(Some(&menu_box));
    menu_btn.set_popover(Some(&menu_popover));
    
    // Search entry removed from headerbar and built for the main view
    let search_entry = SearchEntry::builder()
        .placeholder_text("Search stations…")
        .width_request(300)
        .margin_start(16).margin_end(16).margin_top(8).margin_bottom(4)
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
        .orientation(Orientation::Vertical).spacing(0)
        .margin_bottom(6).margin_top(6).margin_start(12).margin_end(12)
        .build();

    let row2_category_box = Box::builder()
        .orientation(Orientation::Horizontal).spacing(12)
        .halign(gtk::Align::Fill)
        .build();

    let sort_area = Box::builder()
        .orientation(Orientation::Vertical).spacing(0)
        .halign(gtk::Align::Start)
        .build();
    let sort_row = Box::builder()
        .orientation(Orientation::Horizontal).spacing(4)
        .halign(gtk::Align::Start)
        .build();
    let sort_desc = Label::builder()
        .label("Sort:")
        .css_classes(vec!["caption".to_string(), "dim-label".to_string()])
        .build();
    sort_row.append(&sort_desc);

    let make_sort_btn = |icon: &str, label: &str| -> Button {
        let hbox = Box::builder().orientation(Orientation::Horizontal).spacing(4).build();
        let img = gtk::Image::from_icon_name(icon);
        img.set_icon_size(gtk::IconSize::Inherit);
        let lbl = Label::builder().label(label).css_classes(vec!["caption".to_string()]).build();
        hbox.append(&img); hbox.append(&lbl);
        Button::builder().child(&hbox).css_classes(vec!["flat".to_string()]).build()
    };
    let sort_abc_btn   = make_sort_btn("format-justify-left-symbolic", "A–Z");
    sort_row.append(&sort_abc_btn);

    let br_area = Box::builder()
        .orientation(Orientation::Horizontal).spacing(4)
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
            BitrateFilterSetting::Low  => "⊟ Low ≤160",
            BitrateFilterSetting::High => "⊟ High ≥192",
            BitrateFilterSetting::Flac => "⊟ FLAC",
        })
        .css_classes(vec!["flat".to_string()])
        .build();
    br_area.append(&bitrate_menu_btn);

    let bitrate_popover = Popover::new();
    let bitrate_options_box = Box::new(Orientation::Vertical, 4);
    let any_br_btn  = Button::builder().label("Any Bitrate").has_frame(false).build();
    let low_br_btn  = Button::builder().label("Low (≤160 kbps)").has_frame(false).build();
    let high_br_btn = Button::builder().label("High (≥192 kbps)").has_frame(false).build();
    let flac_br_btn = Button::builder().label("Flac (Lossless)").has_frame(false).build();
    bitrate_options_box.append(&any_br_btn);
    bitrate_options_box.append(&low_br_btn);
    bitrate_options_box.append(&high_br_btn);
    bitrate_options_box.append(&flac_br_btn);
    bitrate_popover.set_child(Some(&bitrate_options_box));
    bitrate_menu_btn.set_popover(Some(&bitrate_popover));

    let combined_row = Box::builder()
        .orientation(Orientation::Horizontal).spacing(12)
        .halign(gtk::Align::Start)
        .build();
    combined_row.append(&sort_row);
    combined_row.append(&br_area);

    let back_btn = make_sort_btn("go-previous-symbolic", "Back");
    sort_area.append(&back_btn);

    row2_category_box.append(&sort_area);

    let header_search = Box::builder()
        .orientation(Orientation::Horizontal).spacing(8)
        .margin_start(12).margin_end(12).margin_top(6).margin_bottom(4)
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
        .orientation(Orientation::Horizontal).spacing(12)
        .halign(gtk::Align::End).margin_end(10)
        .build();
    
    let stations_count_label = Label::builder().label("0 entries displayed").css_classes(vec!["caption".to_string(), "dim-label".to_string()]).build();
    let last_updated_label = Label::builder().label("Last update: Never").css_classes(vec!["caption".to_string(), "dim-label".to_string()]).build();
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
        .orientation(Orientation::Horizontal).spacing(12)
        .margin_top(4).margin_bottom(2)
        .build();
    browsing_row.append(&now_browsing_label);
    browsing_row.append(&counter_panel);
    right_content.append(&browsing_row);

    let list_box = ListBox::builder()
        .margin_top(6).margin_bottom(12).margin_start(12).margin_end(12)
        .css_classes(vec!["boxed-list".to_string()])
        .build();

    let scrolled_window = ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never).vscrollbar_policy(gtk::PolicyType::Automatic)
        .vexpand(true).child(&list_box)
        .build();

    right_content.append(&scrolled_window);

    let prev_btn = Button::builder().icon_name("media-skip-backward-symbolic").css_classes(vec!["circular".to_string()]).build();
    let play_pause_btn = Button::builder().icon_name("media-playback-start-symbolic").css_classes(vec!["circular".to_string(), "suggested-action".to_string()]).build();
    let next_btn = Button::builder().icon_name("media-skip-forward-symbolic").css_classes(vec!["circular".to_string()]).build();

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
        .css_classes(vec!["title-2".to_string(), "numeric".to_string(), "dim-label".to_string()])
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
        let accent = ctx.lookup_color("accent_bg_color")
            .or_else(|| ctx.lookup_color("theme_selected_bg_color"))
            .unwrap_or(gtk::gdk::RGBA::new(0.2, 0.44, 0.85, 0.95));
        let (ar, ag, ab) = (accent.red() as f64, accent.green() as f64, accent.blue() as f64);
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
                    cr.arc(x + bar_width / 2.0, y, bar_width / 2.0, std::f64::consts::PI, 0.0);
                    cr.arc(x + bar_width / 2.0, y + bar_h, bar_width / 2.0, 0.0, std::f64::consts::PI);
                    cr.close_path();
                    let _ = cr.fill();

                    let p_h = (peaks[i] / 50.0) * height as f64;
                    let p_y = (height as f64 - p_h) / 2.0;
                    cr.rectangle(x, p_y - 1.0, bar_width, 2.0);
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

                for (i, &val) in bars.iter().enumerate() {
                    let x = i as f64 * (bar_width + spacing);
                    let active_dots = (((val / 50.0) * height as f64) / (dot_height + dot_gap)) as usize;
                    let active_dots = active_dots.min(max_dots).max(1);

                    for j in 0..active_dots {
                        let y = height as f64 - (j as f64 * (dot_height + dot_gap)) - dot_height;
                        cr.rectangle(x, y, bar_width, dot_height);
                        let _ = cr.fill();
                    }

                    let peak_dot_idx = (((peaks[i] / 50.0) * height as f64) / (dot_height + dot_gap)) as usize;
                    let peak_dot_idx = peak_dot_idx.min(max_dots - 1);
                    let p_y = height as f64 - (peak_dot_idx as f64 * (dot_height + dot_gap)) - dot_height;
                    
                    let (adr, adg, adb) = (accent_dim.red() as f64, accent_dim.green() as f64, accent_dim.blue() as f64);
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

                for (i, &val) in bars.iter().enumerate() {
                    let x = i as f64 * (bar_width + spacing);
                    let active_dots = (((val / 50.0) * height as f64) / (dot_height + dot_gap)) as usize;
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
        .margin_top(0).margin_bottom(0)
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
    volume_scale.set_value(1.0);

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

    let (tx, rx) = async_channel::unbounded::<IncomingData>();

    let window_parent_hook = content_box.clone();
    let prefs_window_ref = app_prefs.clone();
    let ui_telemetry_ref = ui_state.clone();
    let tx_sync_prefs = tx.clone();
    let prefs_sync_prefs = app_prefs.clone();
    let ui_sync_prefs = ui_state.clone();
    let sync_label_prefs = last_updated_label.clone();

    settings_btn.connect_clicked(move |_| {
        menu_popover.popdown();
        let prefs_window = adw::PreferencesWindow::builder()
            .title("Preferences")
            .transient_for(&window_parent_hook.root().unwrap().downcast::<gtk::Window>().unwrap())
            .modal(true).build();

        let page = PreferencesPage::new();
        let group = PreferencesGroup::builder().title("System Processing & UI Aesthetics").build();
        let current_state = prefs_window_ref.lock().unwrap();
        let current_ui = ui_telemetry_ref.lock().unwrap();

        let stats_group = PreferencesGroup::builder()
            .title("Database Statistics & Cache Lifecycle Telemetry")
            .description(&format!(
                "Total Stations Loaded: {}\nLast Radio-Browser Sync: {}\nActive Buffer Payload Allocation: {} index maps stored.",
                current_ui.last_stations_updated_count,
                current_ui.last_update_timestamp,
                current_ui.total_elements_fetched
            ))
            .build();

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
            let url = build_filtered_url(&ui.active_category, ui.active_bitrate);
            ui.last_fetched_url = url.clone();
            let view = ui.current_view.clone();
            let is_stations = !matches!(view, ViewType::Tags | ViewType::Languages | ViewType::Countries);
            drop(ui);
            fetch_data_async(url, is_stations, view, sync_prefs.clone(), sync_tx.clone());
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

        group.add(&sync_row);
        group.add(&high_quality_row);
        group.add(&broken_row);
        group.add(&auto_update_row);
        group.add(&analyzer_row);

        page.add(&stats_group);
        page.add(&audio_group);

        let about_group = PreferencesGroup::builder().title("About").build();
        let about_row = ActionRow::builder()
            .title(format!("Rust Radio GTK v{}", env!("CARGO_PKG_VERSION")))
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
                .default_height(280)
                .resizable(false)
                .build();
            let content = Box::builder()
                .orientation(Orientation::Vertical)
                .spacing(12)
                .margin_top(24).margin_bottom(24).margin_start(24).margin_end(24)
                .build();
            let app_label = Label::builder()
                .label(&format!("Rust Radio GTK v{}", env!("CARGO_PKG_VERSION")))
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
                .label("<a href=\"https://github.com/antoxa78/rust-radio-gtk\">github.com/antoxa78/rust-radio-gtk</a>")
                .use_markup(true)
                .css_classes(vec!["caption".to_string()])
                .halign(gtk::Align::Center)
                .build();
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

        page.add(&about_group);
        prefs_window.add(&page);
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
    let br_btn_for_any  = bitrate_menu_btn.clone();
    let br_btn_for_low  = bitrate_menu_btn.clone();
    let br_btn_for_high = bitrate_menu_btn.clone();
    let br_btn_for_flac = bitrate_menu_btn.clone();
    let save_mgr_br_any  = app_state_manager.clone();
    let save_mgr_br_low  = app_state_manager.clone();
    let save_mgr_br_high = app_state_manager.clone();
    let save_mgr_br_flac = app_state_manager.clone();
    let import_btn_tags = import_btn_clone.clone();
    let import_btn_tag_inner = import_btn_clone.clone();
    let clear_recents_tags = clear_recents_btn.clone();
    let clear_recents_tag_inner = clear_recents_btn.clone();
    custom_tags_btn.connect_clicked(move |_| {
        let currently_visible = custom_tag_btns_toggle.is_visible();
        if currently_visible {
            custom_tag_btns_toggle.set_visible(false);
            return;
        }
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
            let hbox = Box::builder().orientation(Orientation::Horizontal).spacing(6).margin_start(20).halign(gtk::Align::Start).build();
            let icon_img = gtk::Image::from_icon_name(tag_to_icon(&tag));
            icon_img.set_icon_size(gtk::IconSize::Normal);
            let lbl = Label::builder().label(&tag).css_classes(vec!["caption".to_string()]).build();
            hbox.append(&icon_img); hbox.append(&lbl);
            let btn = Button::builder()
                .child(&hbox)
                .css_classes(vec!["flat".to_string()])
                .hexpand(true)
                .tooltip_text(&tag)
                .margin_top(1).margin_bottom(1)
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
            btn.connect_clicked(move |clicked_btn| {
                import_btn_tag_btn.set_visible(false);
                clear_recents_tag_btn.set_visible(false);
                // Reset all custom tag button icons to their original tag icon
                let n = btns_container.observe_children().n_items();
                for i in 0..n {
                    if let Some(child) = btns_container.observe_children().item(i) {
                        if let Some(row) = child.downcast::<Box>().ok() {
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
                    BitrateFilterSetting::Low  => "⊟ Low ≤160",
                    BitrateFilterSetting::High => "⊟ High ≥192",
                    BitrateFilterSetting::Flac => "⊟ FLAC",
                });
                drop(ui);
                while let Some(child) = list_box_tag_btn.first_child() {
                    list_box_tag_btn.remove(&child);
                }
                browsing_tag_btn.set_text("Loading stations...");
                fetch_data_async(url, true, ViewType::CustomTag(tag_click.clone()), prefs_c.clone(), tx_c.clone());
            });
            let rm_btn = Button::builder()
                .icon_name("window-close-symbolic")
                .css_classes(vec!["flat".to_string()])
                .margin_top(1).margin_bottom(1)
                .tooltip_text("Remove tag")
                .build();
            let tag_rm = tag.clone();
            let prefs_rm_toggle = prefs_custom_tags_toggle.clone();
            let row_box_rm = row_box.clone();
            let container_rm = custom_tag_btns_toggle.clone();
            let save_mgr_toggle_rm = state_toggle_save.clone();
            rm_btn.connect_clicked(move |_| {
                prefs_rm_toggle.lock().unwrap().custom_tags.retain(|t| t != &tag_rm);
                save_mgr_toggle_rm.save_data();
                container_rm.remove(&row_box_rm);
            });
            row_box.append(&btn);
            row_box.append(&rm_btn);
            custom_tag_btns_toggle.append(&row_box);
        }
        custom_tag_btns_toggle.set_visible(true);
    });

    let tx_tag = tx.clone(); let prefs_tag = app_prefs.clone(); let ui_tag = ui_state.clone();
    tag_btn.connect_clicked(move |_| {
        let mut ui = ui_tag.lock().unwrap();
        if let Some(cached) = ui.cached_tags.clone() {
            ui.last_fetched_url = "https://de1.api.radio-browser.info/json/tags?order=stationcount&reverse=true&hidebroken=true".to_string();
            ui.active_category = CategoryFilter::None;
            drop(ui);
            let _ = tx_tag.send_blocking(IncomingData::Categories(ViewType::Tags, cached));
            return;
        }
        let target_url = "https://de1.api.radio-browser.info/json/tags?order=stationcount&reverse=true&hidebroken=true".to_string();
        ui.last_fetched_url = target_url.clone();
        ui.active_category = CategoryFilter::None;
        drop(ui);
        fetch_data_async(target_url, false, ViewType::Tags, prefs_tag.clone(), tx_tag.clone());
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
                    ui.last_fetched_url = url.clone();
                    let cached = match &prev {
                        ViewType::Tags => ui.cached_tags.clone(),
                        ViewType::Languages => ui.cached_languages.clone(),
                        ViewType::Countries => ui.cached_countries.clone(),
                        _ => None,
                    };
                    if let Some(items) = cached {
                        drop(ui);
                        let _ = back_tx.send_blocking(IncomingData::Categories(prev, items));
                    } else {
                        drop(ui);
                        fetch_data_async(url, false, prev, back_prefs.clone(), back_tx.clone());
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
                    fetch_data_async(cat_url, true, ViewType::CustomTag(tag), back_prefs.clone(), back_tx.clone());
                }
                ViewType::Stations => {
                    let cat_url = build_filtered_url(&ui.active_category, ui.active_bitrate);
                    ui.current_view = ViewType::Stations;
                    ui.last_fetched_url = cat_url.clone();
                    drop(ui);
                    fetch_data_async(cat_url, true, ViewType::Stations, back_prefs.clone(), back_tx.clone());
                }
                ViewType::Favorites => {
                    let stations = ui.favorites.clone();
                    ui.current_view = ViewType::Favorites;
                    ui.active_category = CategoryFilter::None;
                    ui.last_fetched_url = url.clone();
                    drop(ui);
                    back_browsing.set_text("Favorite Stations");
                    let _ = back_tx.send_blocking(IncomingData::Stations(stations, String::new()));
                }
                ViewType::Recent => {
                    let stations = ui.recent_stations.clone();
                    ui.current_view = ViewType::Recent;
                    ui.active_category = CategoryFilter::None;
                    ui.last_fetched_url = url.clone();
                    drop(ui);
                    back_browsing.set_text("Recently Played");
                    let _ = back_tx.send_blocking(IncomingData::Stations(stations, String::new()));
                }
                ViewType::Playlists => {
                    let stations = ui.playlists.clone();
                    ui.current_view = ViewType::Playlists;
                    ui.active_category = CategoryFilter::None;
                    ui.last_fetched_url = url.clone();
                    drop(ui);
                    back_browsing.set_text("Imported Playlist");
                    let _ = back_tx.send_blocking(IncomingData::Stations(stations, String::new()));
                }
            }
        }
    });

    let tx_lang = tx.clone(); let prefs_lang = app_prefs.clone(); let ui_lang = ui_state.clone();
    lang_btn.connect_clicked(move |_| {
        let mut ui = ui_lang.lock().unwrap();
        if let Some(cached) = ui.cached_languages.clone() {
            ui.last_fetched_url = "https://de1.api.radio-browser.info/json/languages?order=stationcount&reverse=true".to_string();
            ui.active_category = CategoryFilter::None;
            drop(ui);
            let _ = tx_lang.send_blocking(IncomingData::Categories(ViewType::Languages, cached));
            return;
        }
        let target_url = "https://de1.api.radio-browser.info/json/languages?order=stationcount&reverse=true".to_string();
        ui.last_fetched_url = target_url.clone();
        ui.active_category = CategoryFilter::None;
        drop(ui);
        fetch_data_async(target_url, false, ViewType::Languages, prefs_lang.clone(), tx_lang.clone());
    });

    let tx_country = tx.clone(); let prefs_country = app_prefs.clone(); let ui_country = ui_state.clone();
    country_btn.connect_clicked(move |_| {
        let mut ui = ui_country.lock().unwrap();
        if let Some(cached) = ui.cached_countries.clone() {
            ui.last_fetched_url = "https://de1.api.radio-browser.info/json/countries?order=stationcount&reverse=true".to_string();
            ui.active_category = CategoryFilter::None;
            drop(ui);
            let _ = tx_country.send_blocking(IncomingData::Categories(ViewType::Countries, cached));
            return;
        }
        let target_url = "https://de1.api.radio-browser.info/json/countries?order=stationcount&reverse=true".to_string();
        ui.last_fetched_url = target_url.clone();
        ui.active_category = CategoryFilter::None;
        drop(ui);
        fetch_data_async(target_url, false, ViewType::Countries, prefs_country.clone(), tx_country.clone());
    });

    let list_box_search = list_box.clone();
    let browsing_search = now_browsing_label.clone();
    let tx_search = tx.clone(); let prefs_search = app_prefs.clone(); let ui_search = ui_state.clone();
    search_entry.connect_activate(move |entry| {
        let query = entry.text().to_string();
        if !query.is_empty() {
            let target_url = format!("https://de1.api.radio-browser.info/json/stations/byname/{}", query);
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
            fetch_data_async(target_url, true, ViewType::Stations, prefs_search.clone(), tx_search.clone());
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
                    ui.categories.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
                } else {
                    ui.categories.sort_by(|a, b| b.name.to_lowercase().cmp(&a.name.to_lowercase()));
                }
                let _ = tx_abc_sort.send_blocking(IncomingData::Categories(ui.current_view.clone(), ui.categories.clone()));
            }
            ViewType::Stations | ViewType::CustomTag(_) => {
                if asc {
                    ui.stations.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
                } else {
                    ui.stations.sort_by(|a, b| b.name.to_lowercase().cmp(&a.name.to_lowercase()));
                }
                let _ = tx_abc_sort.send_blocking(IncomingData::Stations(ui.stations.clone(), ui.last_update_timestamp.clone()));
            }
            ViewType::Playlists => {
                if asc {
                    ui.playlists.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
                } else {
                    ui.playlists.sort_by(|a, b| b.name.to_lowercase().cmp(&a.name.to_lowercase()));
                }
                let _ = tx_abc_sort.send_blocking(IncomingData::Stations(ui.playlists.clone(), ui.last_update_timestamp.clone()));
            }
            ViewType::Favorites => {
                if asc {
                    ui.favorites.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
                } else {
                    ui.favorites.sort_by(|a, b| b.name.to_lowercase().cmp(&a.name.to_lowercase()));
                }
                let _ = tx_abc_sort.send_blocking(IncomingData::Stations(ui.favorites.clone(), ui.last_update_timestamp.clone()));
            }
            ViewType::Recent => {
                if asc {
                    ui.recent_stations.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
                } else {
                    ui.recent_stations.sort_by(|a, b| b.name.to_lowercase().cmp(&a.name.to_lowercase()));
                }
                let _ = tx_abc_sort.send_blocking(IncomingData::Stations(ui.recent_stations.clone(), ui.last_update_timestamp.clone()));
            }
        }
        sort_ascending.set(!asc);
    });

    let list_box_votes = list_box.clone();
    let tx_votes = tx.clone(); let prefs_votes = app_prefs.clone(); let ui_votes = ui_state.clone(); let browsing_votes = now_browsing_label.clone();
    votes_sidebar_btn.connect_clicked(move |_| {
        let target_url = "https://de1.api.radio-browser.info/json/stations/topvote/150".to_string();
        let mut ui = ui_votes.lock().unwrap();
        ui.last_fetched_url = target_url.clone();
        ui.active_category = CategoryFilter::None;
        drop(ui);
        browsing_votes.set_text("Top Voted");
        while let Some(child) = list_box_votes.first_child() {
            list_box_votes.remove(&child);
        }
        browsing_votes.set_text("Loading stations...");
        fetch_data_async(target_url, true, ViewType::Stations, prefs_votes.clone(), tx_votes.clone());
    });

    let ui_fav_menu = ui_state.clone();
    let list_box_fav = list_box.clone();
    let count_lbl_fav = stations_count_label.clone();
    let browsing_fav = now_browsing_label.clone();
    let state_fav_rm_save = app_state_manager.clone();
    let import_btn_fav = import_btn_clone.clone();
    let clear_recents_fav = clear_recents_btn.clone();
    fav_menu_btn.connect_clicked(move |_| {
        import_btn_fav.set_visible(false);
        clear_recents_fav.set_visible(false);
        while let Some(child) = list_box_fav.first_child() {
            list_box_fav.remove(&child);
        }
        let mut ui = ui_fav_menu.lock().unwrap();
        ui.current_view = ViewType::Favorites;
        let count = ui.favorites.len();
        browsing_fav.set_text("Favorite Stations");
        count_lbl_fav.set_text(&format!("{} favorite stations displayed", count));
        
        for station in &ui.favorites {
            let br_badge = station.bitrate.map_or(String::new(), |b| format!(" [{} kbps]", b));
            let row = ActionRow::builder()
                .title(glib::markup_escape_text(station.name.trim()).to_string())
                .subtitle(format!("{} | {}{}", glib::markup_escape_text(&station.country), glib::markup_escape_text(&station.tags), br_badge))
                .activatable(true)
                .build();
            let remove_fav_btn = Button::builder()
                .icon_name("edit-delete-symbolic")
                .css_classes(vec!["flat".to_string()])
                .valign(gtk::Align::Center)
                .tooltip_text("Remove from favourites")
                .build();
            let uuid_to_remove = station.stationuuid.clone();
            let ui_rm_fav = ui_fav_menu.clone();
            let list_rm_fav = list_box_fav.clone();
            let count_rm_fav = count_lbl_fav.clone();
            let row_clone = row.clone();
            let save_mgr_fav_rm = state_fav_rm_save.clone();
            remove_fav_btn.connect_clicked(move |_| {
                ui_rm_fav.lock().unwrap().favorites.retain(|f| f.stationuuid != uuid_to_remove);
                save_mgr_fav_rm.save_data();
                list_rm_fav.remove(&row_clone);
                let remaining = ui_rm_fav.lock().unwrap().favorites.len();
                count_rm_fav.set_text(&format!("{} favorite stations displayed", remaining));
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
    recent_nav_btn.connect_clicked(move |_| {
        import_btn_rec.set_visible(false);
        clear_recents_rec.set_visible(true);
        while let Some(child) = list_box_recent.first_child() {
            list_box_recent.remove(&child);
        }
        let mut ui = ui_recent_menu.lock().unwrap();
        ui.current_view = ViewType::Recent;
        browsing_recent.set_text("Recently Played");
        let count = ui.recent_stations.len();
        count_lbl_recent.set_text(&format!("{} recently played stations", count));
        for station in &ui.recent_stations {
            let br_badge = station.bitrate.map_or(String::new(), |b| format!(" [{} kbps]", b));
            let row = ActionRow::builder()
                .title(glib::markup_escape_text(station.name.trim()).to_string())
                .subtitle(format!("{} | {}{}", glib::markup_escape_text(&station.country), glib::markup_escape_text(&station.tags), br_badge))
                .activatable(true)
                .build();
            list_box_recent.append(&row);
        }
    });

    let ui_playlists_view = ui_state.clone();
    let list_box_playlists = list_box.clone();
    let count_lbl_playlists = stations_count_label.clone();
    let browsing_playlists = now_browsing_label.clone();
    let save_mgr_pl = app_state_manager.clone();
    let import_btn_pl = import_playlist_btn.clone();
    let clear_recents_pl = clear_recents_btn.clone();
    playlists_nav_btn.connect_clicked(move |_| {
        import_btn_pl.set_visible(true);
        clear_recents_pl.set_visible(false);
        browsing_playlists.set_text("Imported Playlist");
        while let Some(child) = list_box_playlists.first_child() {
            list_box_playlists.remove(&child);
        }
        let mut view = ui_playlists_view.lock().unwrap();
        view.current_view = ViewType::Playlists;
        if view.playlists.is_empty() {
            count_lbl_playlists.set_text("No playlist imported");
        } else {
            count_lbl_playlists.set_text(&format!("{} stations in playlist", view.playlists.len()));
            let stations = view.playlists.clone();
            drop(view);
            for (_, s) in stations.iter().enumerate() {
                let row = ActionRow::builder()
                    .title(glib::markup_escape_text(s.name.trim()).to_string())
                    .subtitle(glib::markup_escape_text(&s.url_resolved).to_string())
                    .activatable(true)
                    .build();
                let rm_btn = Button::builder()
                    .icon_name("edit-delete-symbolic")
                    .css_classes(vec!["flat".to_string()])
                    .valign(gtk::Align::Center)
                    .tooltip_text("Remove from playlist")
                    .build();
                let view_rm = ui_playlists_view.clone();
                let list_rm = list_box_playlists.clone();
                let count_rm = count_lbl_playlists.clone();
                let row_clone = row.clone();
                let save_rm = save_mgr_pl.clone();
                rm_btn.connect_clicked(move |_| {
                    let idx = row_clone.index() as usize;
                    let mut v = view_rm.lock().unwrap();
                    if idx < v.playlists.len() {
                        v.playlists.remove(idx);
                        v.current_view = ViewType::Playlists;
                        drop(v);
                        save_rm.save_data();
                        list_rm.remove(&row_clone);
                        let remaining = view_rm.lock().unwrap().playlists.len();
                        count_rm.set_text(&format!("{} stations in playlist", remaining));
                    }
                });
                row.add_suffix(&rm_btn);
                list_box_playlists.append(&row);
            }
        }
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
                        let content = std::fs::read_to_string(&path).unwrap_or_default();
                        let parsed = if path.extension().map(|e| e == "m3u" || e == "M3U").unwrap_or(false) {
                            parse_m3u(&content)
                        } else {
                            parse_pls(&content)
                        };
                        if !parsed.is_empty() {
                            let mut v = vf.lock().unwrap();
                            v.playlists.extend(parsed);
                            v.current_view = ViewType::Playlists;
                            drop(v);
                            sm.save_data();
                            while let Some(child) = lf.first_child() {
                                lf.remove(&child);
                            }
                            let vv = vf.lock().unwrap();
                            cf.set_text(&format!("{} stations in playlist", vv.playlists.len()));
                            let stations = vv.playlists.clone();
                            drop(vv);
                            for (_, s) in stations.iter().enumerate() {
                                let row = ActionRow::builder()
                                    .title(glib::markup_escape_text(s.name.trim()).to_string())
                                    .subtitle(glib::markup_escape_text(&s.url_resolved).to_string())
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
                                rm_btn.connect_clicked(move |_| {
                                    let idx = row_clone.index() as usize;
                                    let mut v = view_rm.lock().unwrap();
                                    if idx < v.playlists.len() {
                                        v.playlists.remove(idx);
                                        v.current_view = ViewType::Playlists;
                                        drop(v);
                                        save_rm.save_data();
                                        list_rm.remove(&row_clone);
                                        let remaining = view_rm.lock().unwrap().playlists.len();
                                        count_rm.set_text(&format!("{} stations in playlist", remaining));
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
        clear_recents_ui.lock().unwrap().recent_stations.clear();
        clear_recents_save.save_data();
        while let Some(child) = clear_recents_list.first_child() {
            clear_recents_list.remove(&child);
        }
        clear_recents_count.set_text("0 recently played stations");
    });

    let list_box_br = list_box.clone();
    let browsing_br = now_browsing_label.clone();
    let tx_any = tx.clone(); let prefs_any = app_prefs.clone(); let pop_any = bitrate_popover.clone(); let ui_any = ui_state.clone();
    any_br_btn.connect_clicked(move |_| {
        pop_any.popdown();
        let mut ui = ui_any.lock().unwrap();
        ui.active_bitrate = BitrateFilterSetting::None;
        let url = build_filtered_url(&ui.active_category, ui.active_bitrate);
        ui.last_fetched_url = url.clone();
        drop(ui);
        br_btn_for_any.set_label("⊟ Bitrate");
        save_mgr_br_any.save_data();
        while let Some(child) = list_box_br.first_child() {
            list_box_br.remove(&child);
        }
        browsing_br.set_text("Loading stations...");
        fetch_data_async(url, true, ViewType::Stations, prefs_any.clone(), tx_any.clone());
    });

    let list_box_low = list_box.clone();
    let browsing_low = now_browsing_label.clone();
    let tx_low = tx.clone(); let prefs_low = app_prefs.clone(); let pop_low = bitrate_popover.clone(); let ui_low = ui_state.clone();
    low_br_btn.connect_clicked(move |_| {
        pop_low.popdown();
        let mut ui = ui_low.lock().unwrap();
        ui.active_bitrate = BitrateFilterSetting::Low;
        let url = build_filtered_url(&ui.active_category, ui.active_bitrate);
        ui.last_fetched_url = url.clone();
        drop(ui);
        br_btn_for_low.set_label("⊟ Low ≤160");
        save_mgr_br_low.save_data();
        while let Some(child) = list_box_low.first_child() {
            list_box_low.remove(&child);
        }
        browsing_low.set_text("Loading stations...");
        fetch_data_async(url, true, ViewType::Stations, prefs_low.clone(), tx_low.clone());
    });

    let list_box_high = list_box.clone();
    let browsing_high = now_browsing_label.clone();
    let tx_high = tx.clone(); let prefs_high = app_prefs.clone(); let pop_high = bitrate_popover.clone(); let ui_high = ui_state.clone();
    high_br_btn.connect_clicked(move |_| {
        pop_high.popdown();
        let mut ui = ui_high.lock().unwrap();
        ui.active_bitrate = BitrateFilterSetting::High;
        let url = build_filtered_url(&ui.active_category, ui.active_bitrate);
        ui.last_fetched_url = url.clone();
        drop(ui);
        br_btn_for_high.set_label("⊟ High ≥192");
        save_mgr_br_high.save_data();
        while let Some(child) = list_box_high.first_child() {
            list_box_high.remove(&child);
        }
        browsing_high.set_text("Loading stations...");
        fetch_data_async(url, true, ViewType::Stations, prefs_high.clone(), tx_high.clone());
    });

    let list_box_flac = list_box.clone();
    let browsing_flac = now_browsing_label.clone();
    let tx_flac = tx.clone(); let prefs_flac = app_prefs.clone(); let pop_flac = bitrate_popover.clone(); let ui_flac = ui_state.clone();
    flac_br_btn.connect_clicked(move |_| {
        pop_flac.popdown();
        let mut ui = ui_flac.lock().unwrap();
        ui.active_bitrate = BitrateFilterSetting::Flac;
        let url = build_filtered_url(&ui.active_category, ui.active_bitrate);
        ui.last_fetched_url = url.clone();
        drop(ui);
        br_btn_for_flac.set_label("⊟ FLAC");
        save_mgr_br_flac.save_data();
        while let Some(child) = list_box_flac.first_child() {
            list_box_flac.remove(&child);
        }
        browsing_flac.set_text("Loading stations...");
        fetch_data_async(url, true, ViewType::Stations, prefs_flac.clone(), tx_flac.clone());
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
        let index = row.index() as usize;
        let mut ui = ui_row_click.lock().unwrap();
        let mode = ui.current_view.clone();

        match mode {
            ViewType::Stations | ViewType::Favorites | ViewType::Recent | ViewType::Playlists | ViewType::CustomTag(_) => {
                drop(ui);
                play_station(&state_row_click, &ui_row_click, index, &ui_play_row_click, &title_row_click, &sub_row_click, &meta_row_click, &info_row_click, &fav_toggle_click, &flag_lbl_row_click, &save_mgr_row_click, &prefs_play_row_click);
            }
            ViewType::Tags => {
                let name = ui.categories[index].name.clone();
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
                fetch_data_async(url, true, ViewType::Stations, prefs_row_click.clone(), tx_row_click.clone());
            }
            ViewType::Languages => {
                let name = ui.categories[index].name.clone();
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
                fetch_data_async(url, true, ViewType::Stations, prefs_row_click.clone(), tx_row_click.clone());
            }
            ViewType::Countries => {
                let name = ui.categories[index].name.clone();
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
                fetch_data_async(url, true, ViewType::Stations, prefs_row_click.clone(), tx_row_click.clone());
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
        let ui = ui_play_pause.lock().unwrap();
        let current = ui.current_index;
        let state = state_play_pause.lock().unwrap();
        if state.currently_playing_station.is_some() {
            if !state.is_paused {
                let _ = state.pipeline.as_ref().unwrap().set_state(gstreamer::State::Paused);
                let mut s = state_play_pause.lock().unwrap();
                s.is_paused = true;
                btn.set_icon_name("media-playback-start-symbolic");
            } else {
                let _ = state.pipeline.as_ref().unwrap().set_state(gstreamer::State::Playing);
                let mut s = state_play_pause.lock().unwrap();
                s.is_paused = false;
                btn.set_icon_name("media-playback-pause-symbolic");
            }
        } else {
            let mode = ui.current_view.clone();
            match mode {
                ViewType::Stations | ViewType::Favorites | ViewType::Recent | ViewType::Playlists | ViewType::CustomTag(_) => {
                    if let Some(idx) = current {
                        if idx < ui.stations.len() {
                            drop(ui);
                            play_station(&state_play_pause, &ui_play_pause, idx, &btn_play_pause_clone, &np_title_ctrl, &np_sub_ctrl, &meta_lbl_ctrl, &info_lbl_ctrl, &fav_toggle_ctrl, &flag_lbl_ctrl, &save_mgr_play_pause, &prefs_play_pause);
                        }
                    }
                }
                _ => {}
            }
        }
    });

    let state_prev = player_state.clone(); let ui_prev = ui_state.clone();
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
            ViewType::Stations | ViewType::Favorites | ViewType::Recent | ViewType::Playlists | ViewType::CustomTag(_) => {
                if let Some(idx) = current {
                    if idx > 0 {
                        let new_idx = idx - 1;
                        play_station(&state_prev, &ui_prev, new_idx, &btn_prev_ui, &np_title_prev, &np_sub_prev, &meta_lbl_prev, &info_lbl_prev, &fav_toggle_prev, &flag_lbl_prev, &save_mgr_prev, &prefs_prev);
                        if let Some(row) = list_box_prev.row_at_index(new_idx as i32) {
                            list_box_prev.select_row(Some(&row));
                        }
                    }
                }
            }
            _ => {}
        }
    });

    let state_next = player_state.clone(); let ui_next = ui_state.clone();
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
        let stations_len = ui.stations.len();
        drop(ui);
        match mode {
            ViewType::Stations | ViewType::Favorites | ViewType::Recent | ViewType::Playlists | ViewType::CustomTag(_) => {
                if let Some(idx) = current {
                    if idx + 1 < stations_len {
                        let new_idx = idx + 1;
                        play_station(&state_next, &ui_next, new_idx, &btn_next_ui, &np_title_next, &np_sub_next, &meta_lbl_next, &info_lbl_next, &fav_toggle_next, &flag_lbl_next, &save_mgr_next, &prefs_next);
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
    fav_toggle_btn.connect_clicked(move |btn| {
        let player = state_fav_toggle.lock().unwrap();
        if let Some(current_station) = &player.currently_playing_station {
            let mut ui = ui_fav_toggle.lock().unwrap();
            if let Some(pos) = ui.favorites.iter().position(|f| f.stationuuid == current_station.stationuuid) {
                ui.favorites.remove(pos);
                btn.set_icon_name("non-starred-symbolic");
            } else {
                ui.favorites.push(current_station.clone());
                btn.set_icon_name("starred-symbolic");
            }
            drop(ui);
            state_fav_toggle_save.save_data();
        }
    });

    let player_vol = player_state.clone();
    volume_scale.connect_value_changed(move |scale| {
        let vol = scale.value();
        let player = player_vol.lock().unwrap();
        if let Some(pipeline) = &player.pipeline {
            pipeline.set_property("volume", vol);
        }
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
    let stations_count_label_clone = stations_count_label.clone();
    let now_browsing_label_clone = now_browsing_label.clone();
    let last_updated_label_clone = last_updated_label.clone();
    let app_prefs_categories = app_prefs.clone();
    let custom_tag_btns_categories = custom_tag_buttons_box.clone();
    let tx_categories = tx.clone();
    let state_rx_save = app_state_manager.clone();
    let import_btn_rx = import_btn_clone.clone();
    let clear_recents_rx = clear_recents_btn.clone();

    glib::MainContext::default().spawn_local(async move {
        while let Ok(data) = rx.recv().await {
            while let Some(child) = list_box_clone.first_child() {
                list_box_clone.remove(&child);
            }

            match data {
                IncomingData::Categories(view_mode, items) => {
                    import_btn_rx.set_visible(false);
                    clear_recents_rx.set_visible(false);
                    // For category views there is no meaningful DB sync time;
                    // show the local fetch time instead.
                    let now = glib::DateTime::now_local().unwrap();
                    let time_string = now.format("%Y-%m-%d %H:%M:%S").unwrap().to_string();
                    last_updated_label_clone.set_text(&format!("Last update: {}", time_string));
                    let mut ui = ui_rx.lock().unwrap();
                    let is_tags_view = view_mode == ViewType::Tags;
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
                        let row = ActionRow::builder()
                            .title(glib::markup_escape_text(&title_text).to_string())
                            .subtitle(format!("{} active stations available", item.stationcount))
                            .activatable(true)
                            .build();

                        if is_tags_view {
                            let tag_name = title_text.to_lowercase();

                            let add_btn = Button::builder()
                                .icon_name("list-add-symbolic")
                                .css_classes(vec!["flat".to_string()])
                                .tooltip_text(&format!("Add \"{}\" to custom tags", tag_name))
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
                                            if let Some(row) = child.downcast::<Box>().ok() {
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
                                    fetch_data_async(url, true, ViewType::CustomTag(tc_rb.clone()), prefs_rb.clone(), txc_rb.clone());
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
                IncomingData::Stations(stations_list, server_sync_time) => {
                    import_btn_rx.set_visible(false);
                    clear_recents_rx.set_visible(false);
                    let mut ui = ui_rx.lock().unwrap();
                    let display_time = if server_sync_time == "Unknown" || server_sync_time.is_empty() {
                        let now = glib::DateTime::now_local().unwrap();
                        now.format("%Y-%m-%d %H:%M:%S").unwrap().to_string()
                    } else {
                        server_sync_time.clone()
                    };
                    last_updated_label_clone.set_text(&format!("DB synced: {}", display_time));
                    ui.last_update_timestamp = display_time;
                    let prev_view = ui.current_view.clone();
                    if matches!(ui.current_view, ViewType::Tags | ViewType::Languages | ViewType::Countries) {
                        ui.current_view = ViewType::Stations;
                    }
                    ui.stations = stations_list.clone();
                    ui.categories.clear();
                    ui.current_index = None;
                    let working_count = stations_list.iter().filter(|s| s.lastcheckok == 1).count();
                    ui.total_elements_fetched = working_count;
                    ui.last_stations_updated_count = working_count;

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
                    let fav_uuids: HashSet<String> = ui.favorites.iter().map(|f| f.stationuuid.clone()).collect();
                    let total_stations = stations_list.len();
                    drop(ui);
                    state_rx_save.save_data();

                    let display_list: Vec<StationDisplay> = stations_list.into_iter().map(|station| {
                        let title_text = station.name.trim().to_string();
                        let title_escaped = glib::markup_escape_text(&title_text).to_string();
                        let country_escaped = glib::markup_escape_text(&station.country).to_string();
                        let tags_escaped = glib::markup_escape_text(&station.tags).to_string();
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
                        let is_fav = fav_uuids.contains(&station.stationuuid);
                        StationDisplay { title_escaped, subtitle, station, is_fav }
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
                    glib::idle_add_local(move || {
                        let max = display_rc.len();
                        for _ in 0..5 {
                            let idx = current_idx.get();
                            if idx >= max {
                                if let Some(text) = browsing_final.as_ref() {
                                    browsing_label.set_text(text);
                                }
                                count_label.set_text(&count_text);
                                return glib::ControlFlow::Break;
                            }
                            let data = &display_rc[idx];
                            let row = ActionRow::builder()
                                .title(&data.title_escaped)
                                .subtitle(&data.subtitle)
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
                                if let Some(pos) = ui.favorites.iter().position(|f| f.stationuuid == display_rc_clone[station_idx].station.stationuuid) {
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
    fetch_data_async(target_url, false, ViewType::Tags, prefs_init, tx_init);

    let ui_cron_tracker = ui_state.clone();
    let prefs_cron_tracker = app_prefs.clone();
    let tx_cron_tracker = tx.clone();
    std::thread::spawn(move || {
        loop {
            let wants_update = prefs_cron_tracker.lock().unwrap().auto_update;
            if wants_update {
                let current_url = ui_cron_tracker.lock().unwrap().last_fetched_url.clone();
                let view = ui_cron_tracker.lock().unwrap().current_view.clone();
                let is_stations = !matches!(view,
                    ViewType::Tags | ViewType::Languages | ViewType::Countries
                );
                fetch_data_async(current_url, is_stations, view, prefs_cron_tracker.clone(), tx_cron_tracker.clone());
            }
            std::thread::sleep(Duration::from_secs(21600));
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
                    time_label_hook.set_text(&format_time(Duration::from_nanos(position_ns.nseconds())));
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
                drop(heights); drop(peaks); drop(decay_counters);
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
                                        "audio/mpeg"                                     => Some("MP3".to_string()),
                                        "audio/x-flac"                                   => Some("FLAC".to_string()),
                                        "audio/x-vorbis"                                 => Some("Vorbis".to_string()),
                                        "audio/x-opus"                                   => Some("Opus".to_string()),
                                        "audio/x-aac" | "audio/aac" | "audio/mp4a-latm" => Some("AAC".to_string()),
                                        "audio/x-wma"                                    => Some("WMA".to_string()),
                                        "audio/x-alac"                                   => Some("ALAC".to_string()),
                                        "audio/x-speex"                                  => Some("Speex".to_string()),
                                        "audio/x-raw"                                    => Some("PCM".to_string()),
                                        other if other.starts_with("audio/") => Some(
                                            other
                                                .trim_start_matches("audio/x-")
                                                .trim_start_matches("audio/")
                                                .to_uppercase()
                                        ),
                                        _ => None,
                                    };
                                }

                                // Stream title line
                                let title_str = meta.stream_title.clone()
                                    .unwrap_or_else(|| "Live Stream".to_string());
                                meta_label_hook.set_text(&format!("♫ {}", title_str));

                                // Stream info line: codec · bitrate · sample rate · bit depth
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
                                if parts.is_empty() {
                                    info_label_hook.set_text("");
                                } else {
                                    info_label_hook.set_text(&parts.join(" · "));
                                }
                            }
                        }
                    }
                }
            } else {
                for (i, h) in heights.iter_mut().enumerate() {
                    *h = 4.0;
                    peaks[i] = (peaks[i] - 1.5).max(4.0);
                }
                drop(heights); drop(peaks); drop(decay_counters);
                drawing_analyzer_clone.queue_draw();
            }
        } else {
            time_label_hook.set_text("00:00");
            for (i, h) in heights.iter_mut().enumerate() {
                *h = 4.0;
                peaks[i] = (peaks[i] - 1.5).max(4.0);
            }
            drop(heights); drop(peaks); drop(decay_counters);
            drawing_analyzer_clone.queue_draw();
        }
        glib::ControlFlow::Continue
    });

    let window = ApplicationWindow::builder()
        .application(app).title("Rust Radio GTK")
        .default_width(1280).default_height(800)
        .resizable(true)
        .content(&content_box)
        .build();
    window.present();
}
