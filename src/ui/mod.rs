//! Application window and behaviour.
//!
//! All UI state lives on the GTK main thread in `App` (`Rc` + `RefCell`);
//! network work runs on the API runtime and is awaited from the main loop.

mod analyzer;
mod prefs;
mod widgets;

use crate::api::{self, CategoryKind, INITIAL_STATION_LIMIT, MAX_STATION_LIMIT, STATION_PAGE_SIZE};
use crate::icons::tag_to_icon;
use crate::model::{
    country_code_to_flag, format_time, AnalyzerType, BitrateFilter, BrowseItem, Station,
    StationSource, View,
};
use crate::mpris::{Mpris, MprisCommand};
use crate::net_safety::{self, HostCheck};
use crate::persistence::{self, SavedData};
use crate::player::{PlayOptions, Player, PlayerEvent};
use crate::station_cache;
use adw::prelude::*;
use analyzer::Analyzer;
use gtk::{gio, glib};
use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::Rc;
use std::time::Duration;
use widgets::Widgets;

const FIRST_RENDER_BATCH: usize = 30;
const RENDER_BATCH: usize = 40;
const MAX_RECENT: usize = 50;
const MAX_HISTORY: usize = 50;
/// Consecutive stations skipped automatically when one fails to start.
const MAX_AUTO_SKIPS: u32 = 3;
/// Reconnect attempts when a stream that was playing drops.
const MAX_RECONNECTS: u32 = 2;
const AUTO_UPDATE_SECS: u32 = 6 * 60 * 60;
const MAX_BACKUP_BYTES: u64 = 10 * 1024 * 1024;
const MAX_PLAYLIST_BYTES: u64 = 5 * 1024 * 1024;

pub struct Prefs {
    pub prefer_high_bitrate: bool,
    pub hide_broken: bool,
    pub auto_update: bool,
    pub allow_local_streams: bool,
    pub analyzer_type: AnalyzerType,
    pub analyzer_enabled: bool,
    pub custom_tags: Vec<String>,
    pub audio_sink: String,
    pub disable_video_streams: bool,
    pub disable_audio_buffer: bool,
    pub volume: f64,
    pub bitrate: BitrateFilter,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PlayStatus {
    Stopped,
    Connecting,
    Playing,
    /// Stopped by the user; Play reconnects to the same station.
    Paused,
}

#[derive(Default, Clone)]
struct StreamMeta {
    title: Option<String>,
    bitrate_bps: Option<u32>,
    codec: Option<String>,
    sample_rate: Option<i32>,
    bit_depth: Option<i32>,
    buffering: Option<u32>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LoadMode {
    /// Show cached list if any, refresh in background when stale.
    Navigate,
    /// Background refresh of the list on screen (auto-update).
    Refresh,
    /// Skip the cache (Preferences → Sync now).
    Force,
}

struct State {
    view: View,
    history: Vec<View>,
    categories: Vec<BrowseItem>,
    /// Exactly what the station rows show, in row order.
    displayed: Vec<Station>,
    search_path: String,
    /// Stations requested from the server so far (next page offset).
    station_limit: usize,
    has_more: bool,
    loading: bool,
    sort_ascending_next: bool,
    favorites: Vec<Station>,
    recent: Vec<Station>,
    playlists: Vec<Station>,
    cached_tags: Option<Vec<BrowseItem>>,
    cached_languages: Option<Vec<BrowseItem>>,
    cached_countries: Option<Vec<BrowseItem>>,
    last_sync: String,
    last_count: usize,
    total_fetched: usize,
    // Playback
    current: Option<Station>,
    /// Snapshot of the list playback was started from; Next/Previous and
    /// automatic fallback walk this list, independent of later navigation.
    queue: Vec<Station>,
    queue_pos: usize,
    status: PlayStatus,
    started_current: bool,
    auto_skips: u32,
    reconnects: u32,
    meta: StreamMeta,
    track_number: u64,
}

pub struct App {
    w: Widgets,
    player: Rc<Player>,
    st: RefCell<State>,
    prefs: RefCell<Prefs>,
    analyzer: RefCell<Analyzer>,
    mpris: RefCell<Option<Rc<Mpris>>>,
    custom_tag_icons: RefCell<Vec<(String, gtk::Image)>>,
    nav_gen: Cell<u64>,
    render_gen: Cell<u64>,
    play_gen: Cell<u64>,
    nav_abort: RefCell<Option<tokio::task::AbortHandle>>,
    decay_timer: RefCell<Option<glib::SourceId>>,
}

pub fn build(app: &adw::Application) {
    api::prewarm();
    let saved = persistence::load();
    let prefs = Prefs {
        prefer_high_bitrate: saved.prefer_high_bitrate,
        hide_broken: saved.hide_broken,
        auto_update: saved.auto_update,
        allow_local_streams: saved.allow_local_streams,
        analyzer_type: AnalyzerType::parse(&saved.analyzer_type),
        analyzer_enabled: saved.analyzer_enabled,
        custom_tags: saved.custom_tags.clone(),
        audio_sink: saved.audio_sink.clone(),
        disable_video_streams: saved.disable_video_streams,
        disable_audio_buffer: saved.disable_audio_buffer,
        volume: saved.volume.clamp(0.0, 1.0),
        bitrate: BitrateFilter::parse(&saved.active_bitrate),
    };
    let w = Widgets::build(
        app,
        saved.window_width,
        saved.window_height,
        saved.window_maximized,
        prefs.bitrate,
    );
    let state = State {
        view: View::Tags,
        history: Vec::new(),
        categories: Vec::new(),
        displayed: Vec::new(),
        search_path: String::new(),
        station_limit: INITIAL_STATION_LIMIT,
        has_more: false,
        loading: false,
        sort_ascending_next: true,
        favorites: saved.favorites,
        recent: saved.recent_stations,
        playlists: saved.playlists,
        cached_tags: None,
        cached_languages: None,
        cached_countries: None,
        last_sync: saved.last_sync_timestamp,
        last_count: saved.last_stations_loaded,
        total_fetched: saved.last_stations_loaded,
        current: None,
        queue: Vec::new(),
        queue_pos: 0,
        status: PlayStatus::Stopped,
        started_current: false,
        auto_skips: 0,
        reconnects: 0,
        meta: StreamMeta::default(),
        track_number: 0,
    };
    let app_rc = Rc::new(App {
        w,
        player: Player::new(),
        st: RefCell::new(state),
        prefs: RefCell::new(prefs),
        analyzer: RefCell::new(Analyzer::default()),
        mpris: RefCell::new(None),
        custom_tag_icons: RefCell::new(Vec::new()),
        nav_gen: Cell::new(0),
        render_gen: Cell::new(0),
        play_gen: Cell::new(0),
        nav_abort: RefCell::new(None),
        decay_timer: RefCell::new(None),
    });
    app_rc.init(app);
}

fn markup(text: &str) -> String {
    glib::markup_escape_text(text).to_string()
}

/// Filled star in the theme's warning (gold) colour when favourited.
fn set_favorite_star(button: &gtk::Button, active: bool) {
    button.set_icon_name(if active {
        "starred-symbolic"
    } else {
        "non-starred-symbolic"
    });
    button.set_tooltip_text(Some(if active {
        "Remove from favourites"
    } else {
        "Add to favourites"
    }));
    if active {
        button.add_css_class("warning");
    } else {
        button.remove_css_class("warning");
    }
}

fn local_time_string() -> String {
    glib::DateTime::now_local()
        .ok()
        .and_then(|now| now.format("%Y-%m-%d %H:%M:%S").ok())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "Unknown".to_string())
}

fn unix_time_local_string(secs: u64) -> String {
    i64::try_from(secs)
        .ok()
        .and_then(|secs| glib::DateTime::from_unix_local(secs).ok())
        .and_then(|time| time.format("%Y-%m-%d %H:%M:%S").ok())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "Unknown".to_string())
}

fn station_subtitle(station: &Station) -> String {
    let flag = country_code_to_flag(&station.countrycode);
    let mut parts: Vec<String> = Vec::new();
    let place = format!("{} {}", flag, station.country).trim().to_string();
    if !place.is_empty() {
        parts.push(place);
    }
    if !station.tags.is_empty() {
        parts.push(station.tags.replace(',', ", "));
    }
    if let Some(br) = station.bitrate.filter(|&b| b > 0) {
        parts.push(format!("{} kbps", br));
    }
    if parts.is_empty() {
        station.url_resolved.clone()
    } else {
        parts.join(" | ")
    }
}

impl App {
    fn init(self: &Rc<Self>, app: &adw::Application) {
        let w = &self.w;
        w.analyzer.add_css_class("accent");
        w.analyzer.set_visible(self.prefs.borrow().analyzer_enabled);
        w.volume_scale.set_value(self.prefs.borrow().volume);
        self.update_volume_icon();
        if !self.st.borrow().last_sync.is_empty() {
            w.updated_label
                .set_text(&format!("DB synced: {}", self.st.borrow().last_sync));
        }

        self.connect_sidebar();
        self.connect_toolbar();
        self.connect_list();
        self.connect_player_bar();
        self.connect_player_events();
        self.connect_actions(app);
        self.connect_window();
        self.start_mpris();
        self.start_timers();
        self.rebuild_custom_tags();

        let me = self.clone();
        self.w
            .analyzer
            .set_draw_func(move |area, cr, width, height| {
                let kind = me.prefs.borrow().analyzer_type;
                me.analyzer.borrow().draw(kind, area, cr, width, height);
            });

        self.navigate(View::Tags, false);
        self.update_transport();
        self.w.window.present();
    }

    // -----------------------------------------------------------------
    // Persistence

    fn snapshot(&self) -> SavedData {
        let st = self.st.borrow();
        let p = self.prefs.borrow();
        let (width, height) = self.w.window.default_size();
        SavedData {
            favorites: st.favorites.clone(),
            custom_tags: p.custom_tags.clone(),
            active_bitrate: p.bitrate.as_str().to_string(),
            recent_stations: st.recent.clone(),
            last_sync_timestamp: st.last_sync.clone(),
            playlists: st.playlists.clone(),
            audio_sink: p.audio_sink.clone(),
            disable_video_streams: p.disable_video_streams,
            disable_audio_buffer: p.disable_audio_buffer,
            analyzer_type: p.analyzer_type.as_str().to_string(),
            analyzer_enabled: p.analyzer_enabled,
            volume: p.volume,
            last_stations_loaded: st.last_count,
            prefer_high_bitrate: p.prefer_high_bitrate,
            hide_broken: p.hide_broken,
            auto_update: p.auto_update,
            allow_local_streams: p.allow_local_streams,
            window_width: width,
            window_height: height,
            window_maximized: self.w.window.is_maximized(),
        }
    }

    fn save(&self) {
        persistence::save_in_background(self.snapshot());
    }

    // -----------------------------------------------------------------
    // Wiring

    fn connect_sidebar(self: &Rc<Self>) {
        let nav = &self.w.nav;
        let pairs: [(&gtk::Button, View); 7] = [
            (&nav.tags, View::Tags),
            (&nav.top_voted, View::Stations(StationSource::TopVoted)),
            (&nav.languages, View::Languages),
            (&nav.countries, View::Countries),
            (&nav.favorites, View::Favorites),
            (&nav.recent, View::Recent),
            (&nav.playlists, View::Playlists),
        ];
        for (button, view) in pairs {
            let me = self.clone();
            button.connect_clicked(move |_| me.navigate(view.clone(), true));
        }
        let me = self.clone();
        nav.custom_tags.connect_clicked(move |_| {
            let visible = !me.w.custom_tags_box.is_visible();
            me.w.custom_tags_box.set_visible(visible);
            if visible && me.prefs.borrow().custom_tags.is_empty() {
                me.w.toast("No custom tags yet — use + next to a tag in “Browse By Tag”");
            }
        });
    }

    fn connect_toolbar(self: &Rc<Self>) {
        let me = self.clone();
        self.w.search_entry.connect_activate(move |entry| {
            let query = entry.text().trim().to_string();
            if !query.is_empty() {
                me.navigate(View::Stations(StationSource::Search(query)), true);
            }
        });
        let me = self.clone();
        self.w.back_btn.connect_clicked(move |_| me.go_back());
        let me = self.clone();
        self.w.sort_btn.connect_clicked(move |_| me.toggle_sort());
        for (filter, button) in &self.w.bitrate_options {
            let me = self.clone();
            let filter = *filter;
            button.connect_clicked(move |_| {
                me.w.bitrate_popover.popdown();
                me.set_bitrate_filter(filter);
            });
        }
        let me = self.clone();
        self.w
            .import_btn
            .connect_clicked(move |_| me.import_playlist());
        let me = self.clone();
        self.w
            .clear_recents_btn
            .connect_clicked(move |_| me.clear_recents());
    }

    fn connect_list(self: &Rc<Self>) {
        let me = self.clone();
        self.w.list_box.connect_row_activated(move |_, row| {
            let Ok(index) = usize::try_from(row.index()) else {
                return;
            };
            let view = me.st.borrow().view.clone();
            if view.is_category() {
                let Some(name) = me.st.borrow().categories.get(index).map(|c| c.name.clone())
                else {
                    return;
                };
                let source = match view {
                    View::Tags => StationSource::Tag(name),
                    View::Languages => StationSource::Language(name),
                    _ => StationSource::Country(name),
                };
                me.navigate(View::Stations(source), true);
            } else if index < me.st.borrow().displayed.len() {
                me.play_from_list(index);
            }
        });
    }

    fn connect_player_bar(self: &Rc<Self>) {
        let me = self.clone();
        self.w.play_btn.connect_clicked(move |_| me.play_pause());
        let me = self.clone();
        self.w.next_btn.connect_clicked(move |_| me.step(1));
        let me = self.clone();
        self.w.prev_btn.connect_clicked(move |_| me.step(-1));
        let me = self.clone();
        self.w
            .fav_btn
            .connect_clicked(move |_| me.toggle_current_favorite());
        let me = self.clone();
        self.w
            .volume_scale
            .connect_value_changed(move |scale| me.set_volume(scale.value(), false));
        let release = gtk::GestureClick::new();
        let me = self.clone();
        release.connect_released(move |_, _, _, _| me.save());
        self.w.volume_scale.add_controller(release);
    }

    fn connect_window(self: &Rc<Self>) {
        let me = self.clone();
        self.w.window.connect_close_request(move |_| {
            me.play_gen.set(me.play_gen.get() + 1);
            me.player.stop();
            persistence::save_now(&me.snapshot());
            glib::Propagation::Proceed
        });
        // Space toggles playback unless the user is typing.
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let me = self.clone();
        keys.connect_key_pressed(move |_, key, _, modifiers| {
            let typing = gtk::prelude::GtkWindowExt::focus(&me.w.window)
                .is_some_and(|f| f.is::<gtk::Text>() || f.is::<gtk::Editable>());
            if key == gtk::gdk::Key::space && modifiers.is_empty() && !typing {
                me.play_pause();
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        self.w.window.add_controller(keys);
    }

    fn connect_actions(self: &Rc<Self>, app: &adw::Application) {
        let add = |name: &str, accels: &[&str], f: Box<dyn Fn()>| {
            let action = gio::SimpleAction::new(name, None);
            action.connect_activate(move |_, _| f());
            app.add_action(&action);
            if !accels.is_empty() {
                app.set_accels_for_action(&format!("app.{}", name), accels);
            }
        };
        let me = self.clone();
        add(
            "preferences",
            &["<Control>comma"],
            Box::new(move || prefs::show_preferences(&me)),
        );
        let me = self.clone();
        add("about", &[], Box::new(move || prefs::show_about(&me)));
        let me = self.clone();
        add(
            "shortcuts",
            &["<Control>question"],
            Box::new(move || prefs::show_shortcuts(&me)),
        );
        let me = self.clone();
        add(
            "quit",
            &["<Control>q"],
            Box::new(move || me.w.window.close()),
        );
        let me = self.clone();
        add(
            "search",
            &["<Control>f"],
            Box::new(move || {
                me.w.search_entry.grab_focus();
            }),
        );
        let me = self.clone();
        add("next", &["<Control>Right"], Box::new(move || me.step(1)));
        let me = self.clone();
        add(
            "previous",
            &["<Control>Left"],
            Box::new(move || me.step(-1)),
        );
        let me = self.clone();
        add(
            "play-pause",
            &["<Control>space"],
            Box::new(move || me.play_pause()),
        );
        let me = self.clone();
        add(
            "favorite",
            &["<Control>d"],
            Box::new(move || me.toggle_current_favorite()),
        );
        let me = self.clone();
        add("back", &["<Alt>Left"], Box::new(move || me.go_back()));
    }

    fn start_mpris(self: &Rc<Self>) {
        let me = self.clone();
        let me_volume = self.clone();
        let mpris = Mpris::start(
            move |command| match command {
                MprisCommand::Raise => me.w.window.present(),
                MprisCommand::Quit => me.w.window.close(),
                MprisCommand::PlayPause => me.play_pause(),
                MprisCommand::Play => {
                    if !matches!(
                        me.st.borrow().status,
                        PlayStatus::Playing | PlayStatus::Connecting
                    ) {
                        me.play_pause();
                    }
                }
                MprisCommand::Pause | MprisCommand::Stop => {
                    if matches!(
                        me.st.borrow().status,
                        PlayStatus::Playing | PlayStatus::Connecting
                    ) {
                        me.play_pause();
                    }
                }
                MprisCommand::Next => me.step(1),
                MprisCommand::Previous => me.step(-1),
            },
            move |volume| me_volume.set_volume(volume, true),
        );
        *self.mpris.borrow_mut() = Some(mpris);
    }

    fn start_timers(self: &Rc<Self>) {
        // Clock and stream format, once a second while playing.
        let me = self.clone();
        glib::timeout_add_seconds_local(1, move || {
            if me.st.borrow().status == PlayStatus::Playing {
                if let Some(position) = me.player.position() {
                    me.w.time_label.set_text(&format_time(position));
                }
                let (rate, depth) = me.player.audio_format();
                {
                    let mut st = me.st.borrow_mut();
                    st.meta.sample_rate = rate.or(st.meta.sample_rate);
                    st.meta.bit_depth = depth.or(st.meta.bit_depth);
                }
                me.update_stream_labels();
            }
            glib::ControlFlow::Continue
        });
        // Background refresh of the open station list.
        let me = self.clone();
        glib::timeout_add_seconds_local(AUTO_UPDATE_SECS, move || {
            let (wants, is_list, busy) = {
                let st = me.st.borrow();
                (
                    me.prefs.borrow().auto_update,
                    matches!(st.view, View::Stations(_)),
                    st.loading,
                )
            };
            if wants && is_list && !busy {
                me.load_stations(LoadMode::Refresh);
            }
            glib::ControlFlow::Continue
        });
    }

    // -----------------------------------------------------------------
    // Navigation

    fn begin_nav(&self) -> u64 {
        let gen = self.nav_gen.get() + 1;
        self.nav_gen.set(gen);
        self.render_gen.set(self.render_gen.get() + 1);
        if let Some(abort) = self.nav_abort.borrow_mut().take() {
            abort.abort();
        }
        self.st.borrow_mut().loading = false;
        gen
    }

    fn navigate(self: &Rc<Self>, view: View, push_history: bool) {
        {
            let mut st = self.st.borrow_mut();
            if push_history && st.view != view {
                let previous = st.view.clone();
                st.history.push(previous);
                if st.history.len() > MAX_HISTORY {
                    st.history.remove(0);
                }
            }
            st.view = view.clone();
            st.sort_ascending_next = true;
        }
        self.w.sort_label.set_text("A–Z");
        self.update_toolbar();
        self.update_custom_tag_icons();
        match view {
            View::Tags => self.show_categories(CategoryKind::Tags, false),
            View::Languages => self.show_categories(CategoryKind::Languages, false),
            View::Countries => self.show_categories(CategoryKind::Countries, false),
            View::Stations(source) => {
                let path = {
                    let p = self.prefs.borrow();
                    api::station_search_path(
                        &source,
                        &api::StationQueryOptions {
                            bitrate: p.bitrate,
                            prefer_high_bitrate: p.prefer_high_bitrate,
                            hide_broken: p.hide_broken,
                        },
                    )
                };
                self.st.borrow_mut().search_path = path;
                self.w.title_label.set_text(&source.title());
                self.load_stations(LoadMode::Navigate);
            }
            View::Favorites | View::Recent | View::Playlists => {
                self.begin_nav();
                self.show_local_list(false);
            }
        }
    }

    fn go_back(self: &Rc<Self>) {
        let previous = self.st.borrow_mut().history.pop();
        if let Some(view) = previous {
            self.navigate(view, false);
        }
    }

    /// Re-run the current view (after a filter or preference change).
    pub fn reload_view(self: &Rc<Self>) {
        let view = self.st.borrow().view.clone();
        self.navigate(view, false);
    }

    fn update_toolbar(&self) {
        let st = self.st.borrow();
        self.w.back_btn.set_visible(!st.history.is_empty());
        self.w.bitrate_area.set_visible(!st.view.is_category());
        self.w.import_btn.set_visible(st.view == View::Playlists);
        self.w
            .clear_recents_btn
            .set_visible(st.view == View::Recent);
    }

    fn show_loading(&self, text: &str) {
        self.st.borrow_mut().displayed.clear();
        self.w.clear_list();
        self.w.scrolled.vadjustment().set_value(0.0);
        let row = adw::ActionRow::builder()
            .title(text)
            .activatable(false)
            .build();
        let spinner = gtk::Spinner::builder()
            .spinning(true)
            .valign(gtk::Align::Center)
            .build();
        row.add_suffix(&spinner);
        self.w.list_box.append(&row);
        self.w.count_label.set_text("");
    }

    fn show_error(self: &Rc<Self>, what: &str, error: &str) {
        self.st.borrow_mut().displayed.clear();
        self.w.clear_list();
        self.w.count_label.set_text("");
        let row = adw::ActionRow::builder()
            .title(markup(what))
            .subtitle(markup(error))
            .activatable(false)
            .build();
        row.add_prefix(&gtk::Image::from_icon_name("network-error-symbolic"));
        let retry = gtk::Button::builder()
            .label("Retry")
            .valign(gtk::Align::Center)
            .css_classes(vec!["suggested-action".to_string()])
            .build();
        let me = self.clone();
        retry.connect_clicked(move |_| me.reload_view());
        row.add_suffix(&retry);
        self.w.list_box.append(&row);
    }

    // -----------------------------------------------------------------
    // Categories

    fn show_categories(self: &Rc<Self>, kind: CategoryKind, force: bool) {
        let gen = self.begin_nav();
        let title = match kind {
            CategoryKind::Tags => "Browse by Tag",
            CategoryKind::Languages => "Browse by Language",
            CategoryKind::Countries => "Browse by Country",
        };
        self.w.title_label.set_text(title);
        let cached = {
            let st = self.st.borrow();
            match kind {
                CategoryKind::Tags => st.cached_tags.clone(),
                CategoryKind::Languages => st.cached_languages.clone(),
                CategoryKind::Countries => st.cached_countries.clone(),
            }
        };
        if let (Some(items), false) = (cached, force) {
            self.render_categories(kind, items);
            return;
        }
        self.show_loading("Loading…");
        self.st.borrow_mut().loading = true;
        let (fut, abort) = api::run(api::fetch_categories(kind));
        *self.nav_abort.borrow_mut() = Some(abort);
        let me = self.clone();
        glib::spawn_future_local(async move {
            let result = fut.await;
            if me.nav_gen.get() != gen {
                return;
            }
            me.st.borrow_mut().loading = false;
            match result {
                Some(Ok(items)) => {
                    {
                        let mut st = me.st.borrow_mut();
                        match kind {
                            CategoryKind::Tags => st.cached_tags = Some(items.clone()),
                            CategoryKind::Languages => st.cached_languages = Some(items.clone()),
                            CategoryKind::Countries => st.cached_countries = Some(items.clone()),
                        }
                    }
                    me.mark_synced();
                    me.render_categories(kind, items);
                }
                Some(Err(err)) => me.show_error("Couldn't load categories", &err),
                None => {}
            }
        });
    }

    fn render_categories(self: &Rc<Self>, kind: CategoryKind, items: Vec<BrowseItem>) {
        self.w.clear_list();
        self.w.scrolled.vadjustment().set_value(0.0);
        let custom: HashSet<String> = self.prefs.borrow().custom_tags.iter().cloned().collect();
        for item in &items {
            let name = item.name.trim().to_string();
            let title = match kind {
                CategoryKind::Countries => {
                    format!("{} {}", country_code_to_flag(&item.countrycode), name)
                        .trim()
                        .to_string()
                }
                _ => name.clone(),
            };
            let row = adw::ActionRow::builder()
                .title(markup(&title))
                .subtitle(format!("{} active stations available", item.stationcount))
                .activatable(true)
                .build();
            if kind == CategoryKind::Tags {
                let tag = name.to_lowercase();
                let added = custom.contains(&tag);
                let add_btn = gtk::Button::builder()
                    .icon_name(if added {
                        "object-select-symbolic"
                    } else {
                        "list-add-symbolic"
                    })
                    .css_classes(vec!["flat".to_string()])
                    .tooltip_text(format!("Add “{}” to custom tags", tag))
                    .valign(gtk::Align::Center)
                    .sensitive(!added)
                    .build();
                let me = self.clone();
                add_btn.connect_clicked(move |btn| {
                    me.add_custom_tag(&tag);
                    btn.set_sensitive(false);
                    btn.set_icon_name("object-select-symbolic");
                });
                row.add_suffix(&add_btn);
            }
            self.w.list_box.append(&row);
        }
        self.w
            .count_label
            .set_text(&format!("{} categories displayed", items.len()));
        let mut st = self.st.borrow_mut();
        st.total_fetched = items.len();
        st.categories = items;
        st.displayed.clear();
    }

    // -----------------------------------------------------------------
    // Station lists

    fn load_stations(self: &Rc<Self>, mode: LoadMode) {
        let gen = self.begin_nav();
        let (path, limit, allow_local) = {
            let st = self.st.borrow();
            let limit = if mode == LoadMode::Refresh {
                st.station_limit
            } else {
                INITIAL_STATION_LIMIT
            };
            (
                st.search_path.clone(),
                limit,
                self.prefs.borrow().allow_local_streams,
            )
        };
        let key = station_cache::key_for(&path);
        if mode != LoadMode::Refresh {
            self.show_loading("Loading stations…");
        }
        self.st.borrow_mut().loading = true;
        let me = self.clone();
        glib::spawn_future_local(async move {
            let mut limit = limit;
            let mut cached_list: Option<Vec<Station>> = None;
            if mode == LoadMode::Navigate {
                let cache_key = key.clone();
                let entry = api::blocking(move || station_cache::get(&cache_key))
                    .await
                    .flatten();
                if me.nav_gen.get() != gen {
                    return;
                }
                if let Some(entry) = entry {
                    me.set_station_list(entry.stations.clone(), entry.limit, entry.has_more, false);
                    me.w.updated_label.set_text(&format!(
                        "DB synced: {} (cached)",
                        unix_time_local_string(entry.fetched_at)
                    ));
                    if entry.is_fresh() {
                        me.st.borrow_mut().loading = false;
                        return;
                    }
                    limit = api::clamp_station_limit(entry.limit);
                    cached_list = Some(entry.stations);
                }
            }
            let refreshing = mode == LoadMode::Refresh || cached_list.is_some();
            let (fut, abort) = api::run(api::fetch_station_page(path, 0, limit, allow_local));
            *me.nav_abort.borrow_mut() = Some(abort);
            let result = fut.await;
            if me.nav_gen.get() != gen {
                return;
            }
            me.st.borrow_mut().loading = false;
            match result {
                Some(Ok((stations, has_more))) => {
                    station_cache::put(&key, stations.clone(), limit, has_more);
                    let persist_key = key.clone();
                    api::spawn(async move {
                        let _ = tokio::task::spawn_blocking(move || {
                            station_cache::persist(&persist_key)
                        })
                        .await;
                    });
                    let unchanged = if mode == LoadMode::Refresh {
                        me.st.borrow().displayed == stations
                    } else {
                        cached_list.as_ref() == Some(&stations)
                    };
                    if !unchanged {
                        me.set_station_list(stations, limit, has_more, refreshing);
                    }
                    me.mark_synced();
                }
                Some(Err(err)) => {
                    if refreshing {
                        me.w.toast(&format!("Couldn't refresh the list: {}", err));
                    } else {
                        me.show_error("Couldn't load stations", &err);
                    }
                }
                None => {}
            }
        });
    }

    /// Record a completed list refresh. The timestamp is when *this app*
    /// fetched the data, not radio-browser's own last-change time (which can
    /// stay unchanged for days and made "Sync" look like it did nothing).
    fn mark_synced(self: &Rc<Self>) {
        let time = local_time_string();
        self.w
            .updated_label
            .set_text(&format!("DB synced: {}", time));
        self.st.borrow_mut().last_sync = time;
        self.save();
    }

    fn set_station_list(
        self: &Rc<Self>,
        stations: Vec<Station>,
        limit: usize,
        has_more: bool,
        keep_scroll: bool,
    ) {
        {
            let mut st = self.st.borrow_mut();
            st.station_limit = api::clamp_station_limit(limit);
            st.has_more = has_more && st.station_limit < MAX_STATION_LIMIT;
            let working = stations.iter().filter(|s| s.lastcheckok == 1).count();
            st.last_count = working;
            st.total_fetched = working;
            st.displayed = stations;
            st.categories.clear();
        }
        self.render_station_rows(0, keep_scroll);
    }

    fn load_more(self: &Rc<Self>, trigger_row: &adw::ActionRow) {
        let (path, offset, allow_local) = {
            let st = self.st.borrow();
            (
                st.search_path.clone(),
                st.station_limit,
                self.prefs.borrow().allow_local_streams,
            )
        };
        if offset >= MAX_STATION_LIMIT {
            return;
        }
        let page = STATION_PAGE_SIZE.min(MAX_STATION_LIMIT - offset);
        trigger_row.set_title("Loading more stations…");
        trigger_row.set_activatable(false);
        let spinner = gtk::Spinner::builder()
            .spinning(true)
            .valign(gtk::Align::Center)
            .build();
        trigger_row.add_suffix(&spinner);
        let gen = self.nav_gen.get();
        let key = station_cache::key_for(&path);
        let (fut, _) = api::run(api::fetch_station_page(path, offset, page, allow_local));
        let me = self.clone();
        let row = trigger_row.clone();
        glib::spawn_future_local(async move {
            let result = fut.await;
            if me.nav_gen.get() != gen {
                return;
            }
            me.w.list_box.remove(&row);
            match result {
                Some(Ok((stations, has_more))) => {
                    let start = {
                        let mut st = me.st.borrow_mut();
                        let known: HashSet<String> = st
                            .displayed
                            .iter()
                            .filter(|s| !s.stationuuid.is_empty())
                            .map(|s| s.stationuuid.clone())
                            .collect();
                        let fresh: Vec<Station> = stations
                            .into_iter()
                            .filter(|s| s.stationuuid.is_empty() || !known.contains(&s.stationuuid))
                            .collect();
                        let start = st.displayed.len();
                        st.station_limit = api::clamp_station_limit(offset + page);
                        st.has_more = has_more && st.station_limit < MAX_STATION_LIMIT;
                        if station_cache::append(&key, &fresh, st.station_limit, st.has_more) {
                            let persist_key = key.clone();
                            api::spawn(async move {
                                let _ = tokio::task::spawn_blocking(move || {
                                    station_cache::persist(&persist_key)
                                })
                                .await;
                            });
                        }
                        st.displayed.extend(fresh);
                        start
                    };
                    me.render_station_rows(start, false);
                }
                Some(Err(err)) => {
                    me.w.toast(&format!("Couldn't load more stations: {}", err));
                    me.append_load_more_row("Retry Loading More");
                }
                None => {}
            }
        });
    }

    fn append_load_more_row(self: &Rc<Self>, title: &str) {
        let row = adw::ActionRow::builder()
            .title(title)
            .subtitle(format!("Load {} more stations", STATION_PAGE_SIZE))
            .activatable(true)
            .build();
        row.add_prefix(&gtk::Image::from_icon_name("go-down-symbolic"));
        let me = self.clone();
        row.connect_activated(move |row| me.load_more(row));
        self.w.list_box.append(&row);
    }

    fn show_local_list(self: &Rc<Self>, keep_scroll: bool) {
        let (view, list) = {
            let st = self.st.borrow();
            let list = match st.view {
                View::Favorites => st.favorites.clone(),
                View::Recent => st.recent.clone(),
                _ => st.playlists.clone(),
            };
            (st.view.clone(), list)
        };
        let title = match view {
            View::Favorites => "Favorite Stations",
            View::Recent => "Recently Played",
            _ => "Imported Playlist",
        };
        self.w.title_label.set_text(title);
        let filtered = self.prefs.borrow().bitrate.apply(&list);
        {
            let mut st = self.st.borrow_mut();
            st.displayed = filtered;
            st.has_more = false;
            st.categories.clear();
        }
        self.render_station_rows(0, keep_scroll);
    }

    fn count_text(&self) -> String {
        let st = self.st.borrow();
        let n = st.displayed.len();
        match st.view {
            View::Favorites => format!("{} favorite stations", n),
            View::Recent => format!("{} recently played stations", n),
            View::Playlists if st.playlists.is_empty() => "No playlist imported".to_string(),
            View::Playlists => format!("{} stations in playlist", n),
            _ => format!("{} stations displayed", n),
        }
    }

    /// Render `displayed[start..]` as rows, in small idle batches so the
    /// first screenful appears immediately. `keep_scroll` swaps the whole
    /// list in one tick and keeps the scroll position (background refresh).
    fn render_station_rows(self: &Rc<Self>, start: usize, keep_scroll: bool) {
        let gen = self.render_gen.get() + 1;
        self.render_gen.set(gen);
        let rows: Rc<Vec<Station>> = Rc::new(self.st.borrow().displayed[start..].to_vec());
        if start == 0 && !keep_scroll {
            self.w.clear_list();
            self.w.scrolled.vadjustment().set_value(0.0);
        }
        let favorites: Rc<Vec<Station>> = Rc::new(self.st.borrow().favorites.clone());
        let next = Cell::new(0usize);
        let me = self.clone();
        glib::idle_add_local_full(glib::Priority::HIGH_IDLE, move || {
            if me.render_gen.get() != gen {
                return glib::ControlFlow::Break;
            }
            let saved_scroll = (keep_scroll && next.get() == 0).then(|| {
                let value = me.w.scrolled.vadjustment().value();
                me.w.clear_list();
                value
            });
            let batch = if keep_scroll {
                usize::MAX
            } else if next.get() == 0 {
                FIRST_RENDER_BATCH
            } else {
                RENDER_BATCH
            };
            for _ in 0..batch {
                let i = next.get();
                let Some(station) = rows.get(i) else {
                    me.finish_render(saved_scroll);
                    return glib::ControlFlow::Break;
                };
                let is_fav = favorites.iter().any(|f| f.same_as(station));
                let row = me.station_row(station, is_fav);
                me.w.list_box.append(&row);
                next.set(i + 1);
            }
            glib::ControlFlow::Continue
        });
    }

    fn finish_render(self: &Rc<Self>, saved_scroll: Option<f64>) {
        let has_more = {
            let st = self.st.borrow();
            st.has_more && matches!(st.view, View::Stations(_))
        };
        if has_more {
            self.append_load_more_row("Load More Stations");
        }
        self.w.count_label.set_text(&self.count_text());
        if let Some(value) = saved_scroll {
            let adjustment = self.w.scrolled.vadjustment();
            adjustment.set_value(value);
            glib::timeout_add_local_once(Duration::from_millis(30), move || {
                if (adjustment.value() - value).abs() > 0.5 {
                    adjustment.set_value(value);
                }
            });
        }
        self.highlight_current_row();
    }

    fn station_row(self: &Rc<Self>, station: &Station, is_fav: bool) -> adw::ActionRow {
        let subtitle = station_subtitle(station);
        let row = adw::ActionRow::builder()
            .title(markup(station.name.trim()))
            .subtitle(markup(&subtitle))
            .title_lines(1)
            .subtitle_lines(1)
            .tooltip_text(format!("{}\n{}", station.name.trim(), subtitle))
            .activatable(true)
            .build();
        let view = self.st.borrow().view.clone();
        if view != View::Favorites {
            let star = gtk::Button::builder()
                .css_classes(vec!["flat".to_string()])
                .valign(gtk::Align::Center)
                .build();
            set_favorite_star(&star, is_fav);
            let me = self.clone();
            let s = station.clone();
            star.connect_clicked(move |btn| {
                let now_fav = me.toggle_favorite(&s);
                set_favorite_star(btn, now_fav);
            });
            row.add_suffix(&star);
        }
        if matches!(view, View::Favorites | View::Playlists) {
            let remove = gtk::Button::builder()
                .icon_name("edit-delete-symbolic")
                .css_classes(vec!["flat".to_string()])
                .tooltip_text(if view == View::Favorites {
                    "Remove from favourites"
                } else {
                    "Remove from playlist"
                })
                .valign(gtk::Align::Center)
                .build();
            let me = self.clone();
            let s = station.clone();
            let row_ref = row.clone();
            remove.connect_clicked(move |_| me.remove_from_local_list(&s, &row_ref));
            row.add_suffix(&remove);
        }
        row
    }

    fn remove_from_local_list(self: &Rc<Self>, station: &Station, row: &adw::ActionRow) {
        {
            let mut st = self.st.borrow_mut();
            let view = st.view.clone();
            let list = match view {
                View::Favorites => &mut st.favorites,
                View::Playlists => &mut st.playlists,
                _ => return,
            };
            if let Some(pos) = list.iter().position(|s| s.same_as(station)) {
                list.remove(pos);
            }
            if let Some(pos) = st.displayed.iter().position(|s| s.same_as(station)) {
                st.displayed.remove(pos);
            }
        }
        self.w.list_box.remove(row);
        self.w.count_label.set_text(&self.count_text());
        self.update_fav_button();
        self.save();
    }

    fn highlight_current_row(&self) {
        let st = self.st.borrow();
        let index = st
            .current
            .as_ref()
            .and_then(|c| st.displayed.iter().position(|s| s.same_as(c)));
        match index.and_then(|i| self.w.list_box.row_at_index(i as i32)) {
            Some(row) => self.w.list_box.select_row(Some(&row)),
            None => self.w.list_box.unselect_all(),
        }
    }

    fn toggle_sort(self: &Rc<Self>) {
        let ascending = {
            let mut st = self.st.borrow_mut();
            let ascending = st.sort_ascending_next;
            st.sort_ascending_next = !ascending;
            ascending
        };
        self.w
            .sort_label
            .set_text(if ascending { "Z–A" } else { "A–Z" });
        let view = self.st.borrow().view.clone();
        // Sorting only changes what's on screen; stored lists (e.g. the
        // play history) keep their order.
        if view.is_category() {
            let mut items = self.st.borrow().categories.clone();
            items.sort_by_key(|c| c.name.to_lowercase());
            if !ascending {
                items.reverse();
            }
            let kind = match view {
                View::Tags => CategoryKind::Tags,
                View::Languages => CategoryKind::Languages,
                _ => CategoryKind::Countries,
            };
            self.render_categories(kind, items);
        } else {
            {
                let mut st = self.st.borrow_mut();
                st.displayed.sort_by_key(|s| s.name.trim().to_lowercase());
                if !ascending {
                    st.displayed.reverse();
                }
            }
            self.render_station_rows(0, false);
        }
    }

    fn set_bitrate_filter(self: &Rc<Self>, filter: BitrateFilter) {
        self.prefs.borrow_mut().bitrate = filter;
        self.w.bitrate_btn.set_label(filter.button_label());
        self.save();
        self.reload_view();
    }

    // -----------------------------------------------------------------
    // Favorites, recent, playlists, custom tags

    /// Toggle; returns whether the station is now a favorite.
    fn toggle_favorite(self: &Rc<Self>, station: &Station) -> bool {
        let now_fav = {
            let mut st = self.st.borrow_mut();
            match st.favorites.iter().position(|f| f.same_as(station)) {
                Some(pos) => {
                    st.favorites.remove(pos);
                    false
                }
                None => {
                    st.favorites.push(station.clone());
                    true
                }
            }
        };
        self.update_fav_button();
        self.save();
        now_fav
    }

    fn toggle_current_favorite(self: &Rc<Self>) {
        let Some(current) = self.st.borrow().current.clone() else {
            return;
        };
        let now_fav = self.toggle_favorite(&current);
        self.w.toast(&if now_fav {
            format!("Added “{}” to favourites", current.name.trim())
        } else {
            format!("Removed “{}” from favourites", current.name.trim())
        });
        let view = self.st.borrow().view.clone();
        if view == View::Favorites {
            self.show_local_list(true);
        } else if !view.is_category() {
            self.render_station_rows(0, true);
        }
    }

    fn update_fav_button(&self) {
        let st = self.st.borrow();
        let is_fav = st
            .current
            .as_ref()
            .is_some_and(|c| st.favorites.iter().any(|f| f.same_as(c)));
        set_favorite_star(&self.w.fav_btn, is_fav);
        self.w.fav_btn.set_sensitive(st.current.is_some());
    }

    fn add_to_recent(&self, station: &Station) {
        let mut st = self.st.borrow_mut();
        st.recent.retain(|s| !s.same_as(station));
        st.recent.insert(0, station.clone());
        st.recent.truncate(MAX_RECENT);
    }

    fn clear_recents(self: &Rc<Self>) {
        let previous = std::mem::take(&mut self.st.borrow_mut().recent);
        if previous.is_empty() {
            return;
        }
        self.save();
        self.show_local_list(false);
        let toast = adw::Toast::builder()
            .title("Recent list cleared")
            .button_label("Undo")
            .build();
        let me = self.clone();
        toast.connect_button_clicked(move |_| {
            me.st.borrow_mut().recent = previous.clone();
            me.save();
            if me.st.borrow().view == View::Recent {
                me.show_local_list(false);
            }
        });
        self.w.toasts.add_toast(toast);
    }

    fn import_playlist(self: &Rc<Self>) {
        let filter = gtk::FileFilter::new();
        filter.set_name(Some("Playlists (M3U, M3U8, PLS)"));
        for suffix in ["m3u", "m3u8", "pls"] {
            filter.add_suffix(suffix);
        }
        let filters = gio::ListStore::new::<gtk::FileFilter>();
        filters.append(&filter);
        let dialog = gtk::FileDialog::builder()
            .title("Import Playlist")
            .filters(&filters)
            .default_filter(&filter)
            .modal(true)
            .build();
        let me = self.clone();
        glib::spawn_future_local(async move {
            let Ok(file) = dialog.open_future(Some(&me.w.window)).await else {
                return;
            };
            let Some(path) = file.path() else {
                me.w.toast("Only local files can be imported");
                return;
            };
            let content = match read_limited(&path, MAX_PLAYLIST_BYTES) {
                Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
                Err(err) => {
                    me.w.toast(&format!("Couldn't read playlist: {}", err));
                    return;
                }
            };
            let allow_local = me.prefs.borrow().allow_local_streams;
            let parsed = crate::playlist::parse_playlist(&content, allow_local);
            let (added, duplicates) = {
                let mut st = me.st.borrow_mut();
                let mut added = 0;
                let mut duplicates = 0;
                for station in parsed.stations {
                    if st.playlists.len() >= crate::playlist::MAX_PLAYLIST_STATIONS {
                        break;
                    }
                    if st.playlists.iter().any(|s| s.same_as(&station)) {
                        duplicates += 1;
                    } else {
                        st.playlists.push(station);
                        added += 1;
                    }
                }
                (added, duplicates)
            };
            let mut message = format!(
                "Imported {} station{}",
                added,
                if added == 1 { "" } else { "s" }
            );
            if duplicates > 0 {
                message.push_str(&format!(", {} already in playlist", duplicates));
            }
            if parsed.skipped > 0 {
                message.push_str(&format!(
                    ", {} skipped (unsupported or local-network URL{})",
                    parsed.skipped,
                    if allow_local {
                        ""
                    } else {
                        " — see Preferences"
                    }
                ));
            }
            me.w.toast(&message);
            if added > 0 {
                me.save();
            }
            me.navigate(View::Playlists, me.st.borrow().view != View::Playlists);
        });
    }

    fn add_custom_tag(self: &Rc<Self>, tag: &str) {
        {
            let mut p = self.prefs.borrow_mut();
            if p.custom_tags.iter().any(|t| t == tag) {
                return;
            }
            p.custom_tags.push(tag.to_string());
        }
        self.save();
        self.rebuild_custom_tags();
        self.w.custom_tags_box.set_visible(true);
    }

    pub fn rebuild_custom_tags(self: &Rc<Self>) {
        let container = &self.w.custom_tags_box;
        while let Some(child) = container.first_child() {
            container.remove(&child);
        }
        let mut icons = Vec::new();
        let tags = self.prefs.borrow().custom_tags.clone();
        for tag in tags {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            let content = gtk::Box::builder()
                .orientation(gtk::Orientation::Horizontal)
                .spacing(6)
                .margin_start(20)
                .halign(gtk::Align::Start)
                .build();
            let icon = gtk::Image::from_icon_name(tag_to_icon(&tag));
            content.append(&icon);
            content.append(
                &gtk::Label::builder()
                    .label(&tag)
                    .css_classes(vec!["caption".to_string()])
                    .ellipsize(gtk::pango::EllipsizeMode::End)
                    .build(),
            );
            let open = gtk::Button::builder()
                .child(&content)
                .css_classes(vec!["flat".to_string()])
                .hexpand(true)
                .tooltip_text(&tag)
                .build();
            let me = self.clone();
            let t = tag.clone();
            open.connect_clicked(move |_| {
                me.navigate(View::Stations(StationSource::CustomTag(t.clone())), true)
            });
            let remove = gtk::Button::builder()
                .icon_name("window-close-symbolic")
                .css_classes(vec!["flat".to_string()])
                .tooltip_text("Remove tag")
                .build();
            let me = self.clone();
            let t = tag.clone();
            remove.connect_clicked(move |_| {
                me.prefs.borrow_mut().custom_tags.retain(|x| x != &t);
                me.save();
                me.rebuild_custom_tags();
            });
            row.append(&open);
            row.append(&remove);
            container.append(&row);
            icons.push((tag, icon));
        }
        *self.custom_tag_icons.borrow_mut() = icons;
        self.update_custom_tag_icons();
    }

    /// The custom tag being browsed shows a "playing" arrow.
    fn update_custom_tag_icons(&self) {
        let active = match &self.st.borrow().view {
            View::Stations(StationSource::CustomTag(t)) => Some(t.clone()),
            _ => None,
        };
        for (tag, icon) in self.custom_tag_icons.borrow().iter() {
            let name = if active.as_deref() == Some(tag.as_str()) {
                "media-playback-start-symbolic"
            } else {
                tag_to_icon(tag)
            };
            icon.set_icon_name(Some(name));
        }
    }

    // -----------------------------------------------------------------
    // Playback

    fn play_from_list(self: &Rc<Self>, index: usize) {
        {
            let mut st = self.st.borrow_mut();
            st.queue = st.displayed.clone();
            st.queue_pos = index;
            st.auto_skips = 0;
            st.reconnects = 0;
        }
        self.play_queue_item();
    }

    fn play_queue_item(self: &Rc<Self>) {
        let station = {
            let st = self.st.borrow();
            st.queue.get(st.queue_pos).cloned()
        };
        if let Some(station) = station {
            self.start(station);
        }
    }

    fn step(self: &Rc<Self>, delta: isize) {
        let target = {
            let st = self.st.borrow();
            let pos = st.queue_pos as isize + delta;
            (pos >= 0 && (pos as usize) < st.queue.len()).then_some(pos as usize)
        };
        if let Some(pos) = target {
            {
                let mut st = self.st.borrow_mut();
                st.queue_pos = pos;
                st.auto_skips = 0;
                st.reconnects = 0;
            }
            self.play_queue_item();
        }
    }

    fn play_pause(self: &Rc<Self>) {
        let (status, current) = {
            let st = self.st.borrow();
            (st.status, st.current.clone())
        };
        match (status, current) {
            (PlayStatus::Playing | PlayStatus::Connecting, _) => {
                // Live radio can't resume where it left off; stop now and
                // reconnect on Play instead of letting the server drop us.
                self.play_gen.set(self.play_gen.get() + 1);
                self.player.stop();
                self.st.borrow_mut().status = PlayStatus::Paused;
                self.w
                    .np_meta
                    .set_text("(Stopped — press Play to reconnect)");
                self.w.np_info.set_text("");
                self.w.time_label.set_text("00:00");
                self.after_playback_stopped();
            }
            (_, Some(station)) => {
                {
                    let mut st = self.st.borrow_mut();
                    st.auto_skips = 0;
                    st.reconnects = 0;
                }
                self.start(station);
            }
            (_, None) => {
                let index = {
                    let st = self.st.borrow();
                    if st.displayed.is_empty() {
                        None
                    } else {
                        Some(
                            self.w
                                .list_box
                                .selected_row()
                                .and_then(|r| usize::try_from(r.index()).ok())
                                .filter(|&i| i < st.displayed.len())
                                .unwrap_or(0),
                        )
                    }
                };
                if let Some(index) = index {
                    self.play_from_list(index);
                }
            }
        }
    }

    fn set_volume(self: &Rc<Self>, volume: f64, from_mpris: bool) {
        let volume = volume.clamp(0.0, 1.0);
        self.prefs.borrow_mut().volume = volume;
        self.player.set_volume(volume);
        if from_mpris && (self.w.volume_scale.value() - volume).abs() > f64::EPSILON {
            self.w.volume_scale.set_value(volume);
        }
        self.update_volume_icon();
        if let Some(m) = self.mpris.borrow().as_ref() {
            m.update(|s| s.volume = volume);
        }
    }

    fn update_volume_icon(&self) {
        let v = self.prefs.borrow().volume;
        let icon = if v <= 0.0 {
            "audio-volume-muted-symbolic"
        } else if v < 0.4 {
            "audio-volume-low-symbolic"
        } else if v < 0.75 {
            "audio-volume-medium-symbolic"
        } else {
            "audio-volume-high-symbolic"
        };
        self.w.volume_icon.set_icon_name(Some(icon));
    }

    /// Start playing `station`: validate the URL, check the host isn't
    /// local (unless allowed), then hand it to GStreamer.
    fn start(self: &Rc<Self>, station: Station) {
        let pg = self.play_gen.get() + 1;
        self.play_gen.set(pg);
        self.player.stop();
        {
            let mut st = self.st.borrow_mut();
            st.current = Some(station.clone());
            st.status = PlayStatus::Connecting;
            st.started_current = false;
            st.meta = StreamMeta::default();
            st.track_number += 1;
        }
        self.show_now_playing(&station);
        self.w.np_meta.set_text("(Connecting…)");
        self.w.np_info.set_text("");
        self.w.time_label.set_text("00:00");
        self.update_transport();
        self.highlight_current_row();

        let allow_local = self.prefs.borrow().allow_local_streams;
        if !net_safety::is_allowed_stream_url(&station.url_resolved, allow_local) {
            let reason = if net_safety::is_allowed_stream_url(&station.url_resolved, true) {
                "local-network address; allow it in Preferences › Audio"
            } else {
                "unsupported stream URL"
            };
            self.on_failure(reason.to_string());
            return;
        }
        let me = self.clone();
        if allow_local {
            me.begin_playback(&station);
            return;
        }
        let Some((_, host, port)) = net_safety::split_stream_url(&station.url_resolved) else {
            self.on_failure("unsupported stream URL".into());
            return;
        };
        let (fut, _) = api::run(net_safety::check_host(host, port));
        glib::spawn_future_local(async move {
            let check = fut.await.unwrap_or(HostCheck::Unresolvable);
            // Only a newer play request cancels this one — browsing doesn't.
            if me.play_gen.get() != pg {
                return;
            }
            match check {
                HostCheck::Public => me.begin_playback(&station),
                HostCheck::NonPublic => me.on_failure(
                    "stream is on the local network; allow it in Preferences › Audio".into(),
                ),
                HostCheck::Unresolvable => {
                    me.on_failure("couldn't resolve the stream's server — are you online?".into())
                }
            }
        });
    }

    fn begin_playback(self: &Rc<Self>, station: &Station) {
        let opts = {
            let p = self.prefs.borrow();
            PlayOptions {
                sink: p.audio_sink.clone(),
                disable_video: p.disable_video_streams,
                prebuffer: !p.disable_audio_buffer,
                bitrate_kbps: station.bitrate,
                volume: p.volume,
                spectrum: p.analyzer_enabled,
            }
        };
        if let Err(err) = self.player.play(&station.url_resolved, &opts) {
            self.on_failure(err);
        }
    }

    fn connect_player_events(self: &Rc<Self>) {
        let me = self.clone();
        self.player
            .connect_event(move |event| me.on_player_event(event));
    }

    fn on_player_event(self: &Rc<Self>, event: PlayerEvent) {
        match event {
            PlayerEvent::Buffering(percent) => {
                self.st.borrow_mut().meta.buffering = (percent < 100).then_some(percent);
                if !self.st.borrow().started_current {
                    self.w
                        .np_meta
                        .set_text(&format!("(Buffering {}%…)", percent));
                }
                self.update_stream_labels();
            }
            PlayerEvent::Started => {
                let station = {
                    let mut st = self.st.borrow_mut();
                    st.status = PlayStatus::Playing;
                    st.started_current = true;
                    st.auto_skips = 0;
                    st.meta.buffering = None;
                    st.current.clone()
                };
                if let Some(station) = station {
                    // Only stations that actually played go into Recent.
                    self.add_to_recent(&station);
                    api::count_click(&station.stationuuid);
                    self.save();
                }
                self.update_stream_labels();
                self.update_transport();
                self.stop_decay();
            }
            PlayerEvent::Tags {
                title,
                bitrate_bps,
                codec,
            } => {
                {
                    let mut st = self.st.borrow_mut();
                    if title.is_some() {
                        st.meta.title = title;
                    }
                    if bitrate_bps.is_some() {
                        st.meta.bitrate_bps = bitrate_bps;
                    }
                    if st.meta.codec.is_none() {
                        st.meta.codec = codec;
                    }
                }
                self.update_stream_labels();
                self.update_mpris();
            }
            PlayerEvent::Spectrum(magnitudes) => {
                if !self.prefs.borrow().analyzer_enabled {
                    return;
                }
                let rate = self.st.borrow().meta.sample_rate;
                self.analyzer.borrow_mut().push(&magnitudes, rate);
                self.w.analyzer.queue_draw();
            }
            PlayerEvent::Failed(message) => self.on_failure(message),
            PlayerEvent::Ended => self.on_failure("the stream ended".into()),
        }
    }

    /// A stream failed or ended. Before it ever played: skip to the next
    /// station (at most `MAX_AUTO_SKIPS` in a row). After it played:
    /// reconnect (at most `MAX_RECONNECTS`). Otherwise stop and say why.
    fn on_failure(self: &Rc<Self>, reason: String) {
        self.player.stop();
        let pg = self.play_gen.get();
        enum Next {
            Reconnect,
            Skip(String),
            GiveUp,
        }
        let decision = {
            let mut st = self.st.borrow_mut();
            let name = st
                .current
                .as_ref()
                .map(|s| s.name.trim().to_string())
                .unwrap_or_default();
            if st.started_current && st.reconnects < MAX_RECONNECTS {
                st.reconnects += 1;
                Next::Reconnect
            } else if !st.started_current
                && st.auto_skips < MAX_AUTO_SKIPS
                && st.queue_pos + 1 < st.queue.len()
            {
                st.auto_skips += 1;
                st.queue_pos += 1;
                Next::Skip(name)
            } else {
                st.status = PlayStatus::Stopped;
                Next::GiveUp
            }
        };
        match decision {
            Next::Reconnect => {
                self.w.np_meta.set_text("(Connection lost — reconnecting…)");
                let me = self.clone();
                glib::timeout_add_local_once(Duration::from_secs(2), move || {
                    if me.play_gen.get() != pg {
                        return;
                    }
                    let station = me.st.borrow().current.clone();
                    if let Some(station) = station {
                        // `start` keeps the reconnect counter for this station.
                        me.start(station);
                    }
                });
            }
            Next::Skip(name) => {
                self.w.toast(&format!(
                    "“{}” is unavailable: {}. Trying the next station…",
                    name, reason
                ));
                let me = self.clone();
                glib::idle_add_local_once(move || {
                    if me.play_gen.get() == pg {
                        me.play_queue_item();
                    }
                });
            }
            Next::GiveUp => {
                self.w
                    .np_meta
                    .set_text(&format!("(Playback failed: {})", reason));
                self.w.np_info.set_text("");
                self.after_playback_stopped();
            }
        }
    }

    fn after_playback_stopped(self: &Rc<Self>) {
        self.update_transport();
        self.start_decay();
    }

    fn show_now_playing(&self, station: &Station) {
        self.w.np_title.set_text(station.name.trim());
        self.w
            .np_title
            .set_tooltip_text(Some(&station_subtitle(station)));
        let flag = country_code_to_flag(&station.countrycode);
        self.w.np_flag.set_text(&flag);
        self.w.np_flag.set_visible(!flag.is_empty());
        self.update_fav_button();
    }

    fn update_stream_labels(&self) {
        let st = self.st.borrow();
        if st.status != PlayStatus::Playing {
            return;
        }
        let title = st
            .meta
            .title
            .clone()
            .unwrap_or_else(|| "Live Stream".to_string());
        self.w.np_meta.set_text(&format!("♫ {}", title));
        let mut parts: Vec<String> = Vec::new();
        if let Some(codec) = &st.meta.codec {
            parts.push(codec.clone());
        }
        if let Some(bps) = st.meta.bitrate_bps.filter(|&b| b > 0) {
            parts.push(format!("{} kbps", (bps + 500) / 1000));
        } else if let Some(kbps) = st
            .current
            .as_ref()
            .and_then(|s| s.bitrate)
            .filter(|&b| b > 0)
        {
            parts.push(format!("{} kbps", kbps));
        }
        if let Some(rate) = st.meta.sample_rate {
            parts.push(format!("{:.1} kHz", rate as f64 / 1000.0));
        }
        if let Some(depth) = st.meta.bit_depth {
            parts.push(format!("{}-bit", depth));
        }
        if let Some(pct) = st.meta.buffering {
            parts.push(format!("⏳ {}%", pct));
        }
        self.w.np_info.set_text(&parts.join(" · "));
    }

    /// Play/next/previous buttons and MPRIS state.
    fn update_transport(&self) {
        let (status, has_current, can_prev, can_next) = {
            let st = self.st.borrow();
            (
                st.status,
                st.current.is_some(),
                !st.queue.is_empty() && st.queue_pos > 0,
                st.queue_pos + 1 < st.queue.len(),
            )
        };
        let active = matches!(status, PlayStatus::Playing | PlayStatus::Connecting);
        self.w.play_btn.set_icon_name(if active {
            "media-playback-stop-symbolic"
        } else {
            "media-playback-start-symbolic"
        });
        self.w.prev_btn.set_sensitive(can_prev);
        self.w.next_btn.set_sensitive(can_next);
        self.w.fav_btn.set_sensitive(has_current);
        self.update_mpris();
    }

    fn update_mpris(&self) {
        let Some(mpris) = self.mpris.borrow().clone() else {
            return;
        };
        let st = self.st.borrow();
        let status = match st.status {
            PlayStatus::Playing | PlayStatus::Connecting => "Playing",
            PlayStatus::Paused => "Paused",
            PlayStatus::Stopped => "Stopped",
        };
        let station = st.current.as_ref().map(|s| s.name.trim().to_string());
        let title = st.meta.title.clone();
        let can_next = st.queue_pos + 1 < st.queue.len();
        let can_previous = !st.queue.is_empty() && st.queue_pos > 0;
        let can_play = st.current.is_some() || !st.displayed.is_empty();
        let track = st.track_number;
        drop(st);
        mpris.update(|s| {
            s.status = status;
            s.station = station;
            s.title = title;
            s.can_next = can_next;
            s.can_previous = can_previous;
            s.can_play = can_play;
            s.track_number = track;
        });
    }

    // Analyzer fall-off when playback stops.
    fn start_decay(self: &Rc<Self>) {
        if !self.prefs.borrow().analyzer_enabled || self.decay_timer.borrow().is_some() {
            return;
        }
        let me = self.clone();
        let id = glib::timeout_add_local(Duration::from_millis(50), move || {
            let moving = me.analyzer.borrow_mut().decay();
            me.w.analyzer.queue_draw();
            if moving {
                glib::ControlFlow::Continue
            } else {
                me.decay_timer.borrow_mut().take();
                glib::ControlFlow::Break
            }
        });
        *self.decay_timer.borrow_mut() = Some(id);
    }

    fn stop_decay(&self) {
        if let Some(id) = self.decay_timer.borrow_mut().take() {
            id.remove();
        }
    }

    // -----------------------------------------------------------------
    // Accessors for the preferences module

    pub fn stats_text(&self) -> String {
        let st = self.st.borrow();
        format!(
            "Working stations in current list: {}\nLast Radio-Browser sync: {}\nItems in current view: {}",
            st.last_count,
            if st.last_sync.is_empty() { "never" } else { &st.last_sync },
            st.total_fetched
        )
    }

    /// Preferences → "Sync now".
    pub fn sync_now(self: &Rc<Self>) {
        let view = self.st.borrow().view.clone();
        match view {
            View::Stations(_) => self.load_stations(LoadMode::Force),
            View::Tags => self.show_categories(CategoryKind::Tags, true),
            View::Languages => self.show_categories(CategoryKind::Languages, true),
            View::Countries => self.show_categories(CategoryKind::Countries, true),
            _ => self.show_local_list(true),
        }
    }
}

fn read_limited(path: &std::path::Path, max_bytes: u64) -> Result<Vec<u8>, String> {
    let metadata = std::fs::metadata(path).map_err(|e| e.to_string())?;
    if metadata.len() > max_bytes {
        return Err(format!(
            "file is larger than {} MB",
            max_bytes / (1024 * 1024)
        ));
    }
    std::fs::read(path).map_err(|e| e.to_string())
}
