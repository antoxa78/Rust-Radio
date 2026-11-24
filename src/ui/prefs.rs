//! Preferences dialog, backup/restore, About and keyboard shortcuts.

use super::{read_limited, App, MAX_BACKUP_BYTES};
use crate::model::{AnalyzerType, BrowseItem, Station, View};
use crate::net_safety::is_allowed_stream_url;
use crate::persistence::BackupPayload;
use crate::playlist::MAX_PLAYLIST_STATIONS;
use adw::prelude::*;
use gtk::{gio, glib};
use std::rc::Rc;

const SINKS: [(&str, &str); 6] = [
    ("Auto (default)", "autoaudiosink"),
    ("PulseAudio", "pulsesink"),
    ("PipeWire", "pipewiresink"),
    ("ALSA", "alsasink"),
    ("JACK", "jackaudiosink"),
    ("OSS", "osssink"),
];

fn switch_row(title: &str, subtitle: Option<&str>, active: bool) -> adw::SwitchRow {
    let row = adw::SwitchRow::builder()
        .title(title)
        .active(active)
        .build();
    if let Some(subtitle) = subtitle {
        row.set_subtitle(subtitle);
    }
    row
}

pub fn show_preferences(app: &Rc<App>) {
    let dialog = adw::PreferencesDialog::builder()
        .title("Preferences")
        .build();
    let page = adw::PreferencesPage::new();
    let p = app.prefs.borrow();

    // --- Stations
    let stations = adw::PreferencesGroup::builder().title("Stations").build();
    let sync_row = adw::ActionRow::builder()
        .title("Update Station List Now")
        .subtitle("Reload the current view from radio-browser.info")
        .build();
    let sync_btn = gtk::Button::builder()
        .label("Sync")
        .valign(gtk::Align::Center)
        .build();
    let me = app.clone();
    sync_btn.connect_clicked(move |_| me.sync_now());
    sync_row.add_suffix(&sync_btn);
    stations.add(&sync_row);

    let high = switch_row(
        "Prefer High Bitrate Streams",
        Some("Sort lists by bitrate instead of popularity"),
        p.prefer_high_bitrate,
    );
    let me = app.clone();
    high.connect_active_notify(move |row| {
        me.prefs.borrow_mut().prefer_high_bitrate = row.is_active();
        me.save();
        if matches!(me.st.borrow().view, View::Stations(_)) {
            me.reload_view();
        }
    });
    stations.add(&high);

    let broken = switch_row(
        "Hide Broken Stations",
        Some("Skip stations that failed radio-browser's last check"),
        p.hide_broken,
    );
    let me = app.clone();
    broken.connect_active_notify(move |row| {
        me.prefs.borrow_mut().hide_broken = row.is_active();
        me.save();
        if matches!(me.st.borrow().view, View::Stations(_)) {
            me.reload_view();
        }
    });
    stations.add(&broken);

    let auto = switch_row(
        "Update Automatically",
        Some("Refresh the open station list every 6 hours"),
        p.auto_update,
    );
    let me = app.clone();
    auto.connect_active_notify(move |row| {
        me.prefs.borrow_mut().auto_update = row.is_active();
        me.save();
    });
    stations.add(&auto);

    let analyzer_enabled = switch_row(
        "Show Spectrum Analyzer",
        Some("Live audio visualization in the player bar"),
        p.analyzer_enabled,
    );
    let analyzer_model = gtk::StringList::new(&AnalyzerType::ALL.map(|a| a.label()));
    let analyzer = adw::ComboRow::builder()
        .title("Spectrum Analyzer Style")
        .model(&analyzer_model)
        .selected(
            AnalyzerType::ALL
                .iter()
                .position(|a| *a == p.analyzer_type)
                .unwrap_or(2) as u32,
        )
        .sensitive(p.analyzer_enabled)
        .build();
    let me = app.clone();
    let style_row = analyzer.clone();
    analyzer_enabled.connect_active_notify(move |row| {
        let enabled = row.is_active();
        me.prefs.borrow_mut().analyzer_enabled = enabled;
        me.w.analyzer.set_visible(enabled);
        me.player.set_spectrum_enabled(enabled);
        style_row.set_sensitive(enabled);
        if enabled {
            me.analyzer.borrow_mut().reset();
            me.w.analyzer.queue_draw();
        } else {
            me.stop_decay();
        }
        me.save();
    });
    stations.add(&analyzer_enabled);
    let me = app.clone();
    analyzer.connect_selected_notify(move |row| {
        if let Some(kind) = AnalyzerType::ALL.get(row.selected() as usize) {
            me.prefs.borrow_mut().analyzer_type = *kind;
            me.save();
            me.w.analyzer.queue_draw();
        }
    });
    stations.add(&analyzer);
    page.add(&stations);

    // --- Audio
    let audio = adw::PreferencesGroup::builder().title("Audio").build();
    let sink_model = gtk::StringList::new(&SINKS.map(|(label, _)| label));
    let sink = adw::ComboRow::builder()
        .title("Audio Output")
        .subtitle("Takes effect on the next station play")
        .model(&sink_model)
        .selected(
            SINKS
                .iter()
                .position(|(_, id)| *id == p.audio_sink)
                .unwrap_or(0) as u32,
        )
        .build();
    let me = app.clone();
    sink.connect_selected_notify(move |row| {
        if let Some((_, id)) = SINKS.get(row.selected() as usize) {
            me.prefs.borrow_mut().audio_sink = id.to_string();
            me.save();
        }
    });
    audio.add(&sink);

    let video = switch_row(
        "Disable Video Streams",
        Some("Play only the audio of video streams"),
        p.disable_video_streams,
    );
    let me = app.clone();
    video.connect_active_notify(move |row| {
        me.prefs.borrow_mut().disable_video_streams = row.is_active();
        me.save();
    });
    audio.add(&video);

    let buffer = switch_row(
        "Disable Pre-Buffering",
        Some("Start faster, but more prone to dropouts"),
        p.disable_audio_buffer,
    );
    let me = app.clone();
    buffer.connect_active_notify(move |row| {
        me.prefs.borrow_mut().disable_audio_buffer = row.is_active();
        me.save();
    });
    audio.add(&buffer);

    let local = switch_row(
        "Allow Local Network Streams",
        Some("Play stations on your own network (e.g. a home Icecast server). Off by default for safety."),
        p.allow_local_streams,
    );
    let me = app.clone();
    local.connect_active_notify(move |row| {
        me.prefs.borrow_mut().allow_local_streams = row.is_active();
        me.save();
    });
    audio.add(&local);
    page.add(&audio);
    drop(p);

    // --- Data
    let data = adw::PreferencesGroup::builder()
        .title("Data Management")
        .description("Back up or restore favourites, playlists and tags as a JSON file.")
        .build();
    let backup_row = adw::ActionRow::builder()
        .title("Backup Application Data")
        .build();
    let backup_btn = gtk::Button::builder()
        .label("Backup…")
        .valign(gtk::Align::Center)
        .build();
    let me = app.clone();
    let dlg = dialog.clone();
    backup_btn.connect_clicked(move |_| backup(&me, &dlg));
    backup_row.add_suffix(&backup_btn);
    data.add(&backup_row);
    let restore_row = adw::ActionRow::builder()
        .title("Restore Application Data")
        .build();
    let restore_btn = gtk::Button::builder()
        .label("Restore…")
        .valign(gtk::Align::Center)
        .build();
    let me = app.clone();
    let dlg = dialog.clone();
    restore_btn.connect_clicked(move |_| restore(&me, &dlg));
    restore_row.add_suffix(&restore_btn);
    data.add(&restore_row);
    page.add(&data);

    // --- Statistics
    let stats = adw::PreferencesGroup::builder().title("Statistics").build();
    let stats_label = gtk::Label::builder()
        .label(app.stats_text())
        .css_classes(vec!["caption".to_string()])
        .halign(gtk::Align::Start)
        .wrap(true)
        .build();
    stats.add(&stats_label);
    page.add(&stats);

    // The label above is only a snapshot; keep it live while the dialog is
    // open so "Sync" (and background refreshes) show up immediately.
    let weak_app = Rc::downgrade(app);
    let label = stats_label.clone();
    let open_dialog = dialog.clone();
    let mut last_text = stats_label.text().to_string();
    glib::timeout_add_seconds_local(1, move || {
        let Some(app) = weak_app.upgrade() else {
            return glib::ControlFlow::Break;
        };
        if !open_dialog.is_visible() {
            return glib::ControlFlow::Break;
        }
        let text = app.stats_text();
        if text != last_text {
            label.set_text(&text);
            last_text = text;
        }
        glib::ControlFlow::Continue
    });

    dialog.add(&page);
    dialog.present(Some(&app.w.window));
}

fn check(label: &str, active: bool, sensitive: bool) -> gtk::CheckButton {
    let c = gtk::CheckButton::with_label(label);
    c.set_active(active);
    c.set_sensitive(sensitive);
    c
}

fn backup(app: &Rc<App>, parent: &adw::PreferencesDialog) {
    let boxed = gtk::Box::new(gtk::Orientation::Vertical, 6);
    let tags = check("Home Tags", true, true);
    let favs = check("Favourites", true, true);
    let pls = check("Playlists", true, true);
    let custom = check("Custom Tags", true, true);
    for c in [&tags, &favs, &pls, &custom] {
        boxed.append(c);
    }
    let alert = adw::AlertDialog::builder()
        .heading("Backup Application Data")
        .body("Select what to include:")
        .extra_child(&boxed)
        .build();
    alert.add_responses(&[("cancel", "Cancel"), ("save", "Save File…")]);
    alert.set_response_appearance("save", adw::ResponseAppearance::Suggested);
    alert.set_default_response(Some("save"));
    let me = app.clone();
    let parent = parent.clone();
    glib::spawn_future_local(async move {
        if alert.choose_future(&parent).await != "save" {
            return;
        }
        let payload = {
            let st = me.st.borrow();
            let p = me.prefs.borrow();
            BackupPayload {
                home_tags: tags.is_active().then(|| {
                    st.cached_tags
                        .clone()
                        .unwrap_or_default()
                        .into_iter()
                        .map(|c| c.name)
                        .collect()
                }),
                favourites: favs.is_active().then(|| st.favorites.clone()),
                playlists: pls.is_active().then(|| st.playlists.clone()),
                custom_tags: custom.is_active().then(|| p.custom_tags.clone()),
            }
        };
        let json = match serde_json::to_string_pretty(&payload) {
            Ok(json) => json,
            Err(err) => {
                parent.add_toast(adw::Toast::new(&format!("Backup failed: {}", err)));
                return;
            }
        };
        let chooser = gtk::FileDialog::builder()
            .title("Save Backup")
            .initial_name("rust_radio_backup.json")
            .modal(true)
            .build();
        let Ok(file) = chooser.save_future(Some(&me.w.window)).await else {
            return;
        };
        let result = file
            .replace_contents_future(
                json.into_bytes(),
                None,
                false,
                gio::FileCreateFlags::REPLACE_DESTINATION,
            )
            .await;
        let message = match result {
            Ok(_) => "Backup saved".to_string(),
            Err((_, err)) => format!("Couldn't save backup: {}", err),
        };
        parent.add_toast(
            adw::Toast::builder()
                .title(message)
                .use_markup(false)
                .build(),
        );
    });
}

fn restore(app: &Rc<App>, parent: &adw::PreferencesDialog) {
    let me = app.clone();
    let parent = parent.clone();
    glib::spawn_future_local(async move {
        let toast = |text: String| {
            parent.add_toast(adw::Toast::builder().title(text).use_markup(false).build())
        };
        let filter = gtk::FileFilter::new();
        filter.set_name(Some("JSON files"));
        filter.add_suffix("json");
        let filters = gio::ListStore::new::<gtk::FileFilter>();
        filters.append(&filter);
        let chooser = gtk::FileDialog::builder()
            .title("Open Backup File")
            .filters(&filters)
            .modal(true)
            .build();
        let Ok(file) = chooser.open_future(Some(&me.w.window)).await else {
            return;
        };
        let Some(path) = file.path() else {
            toast("Only local files can be restored".into());
            return;
        };
        let bytes = match read_limited(&path, MAX_BACKUP_BYTES) {
            Ok(bytes) => bytes,
            Err(err) => {
                toast(format!("Couldn't read backup: {}", err));
                return;
            }
        };
        let mut payload: BackupPayload = match serde_json::from_slice(&bytes) {
            Ok(p) => p,
            Err(err) => {
                toast(format!("Not a Rust Radio backup: {}", err));
                return;
            }
        };
        for list in [&mut payload.favourites, &mut payload.playlists]
            .into_iter()
            .flatten()
        {
            list.truncate(MAX_PLAYLIST_STATIONS);
        }
        if let Some(tags) = payload.custom_tags.as_mut() {
            tags.truncate(500);
        }
        if let Some(tags) = payload.home_tags.as_mut() {
            tags.truncate(500);
        }
        if payload.home_tags.is_none()
            && payload.favourites.is_none()
            && payload.playlists.is_none()
            && payload.custom_tags.is_none()
        {
            toast("The backup file contains no data".into());
            return;
        }

        let boxed = gtk::Box::new(gtk::Orientation::Vertical, 6);
        let tags = check(
            "Home Tags",
            payload.home_tags.is_some(),
            payload.home_tags.is_some(),
        );
        let favs = check(
            "Favourites",
            payload.favourites.is_some(),
            payload.favourites.is_some(),
        );
        let pls = check(
            "Playlists",
            payload.playlists.is_some(),
            payload.playlists.is_some(),
        );
        let custom = check(
            "Custom Tags",
            payload.custom_tags.is_some(),
            payload.custom_tags.is_some(),
        );
        for c in [&tags, &favs, &pls, &custom] {
            boxed.append(c);
        }
        let alert = adw::AlertDialog::builder()
            .heading("Restore Application Data")
            .body("Restoring overwrites the selected categories.")
            .extra_child(&boxed)
            .build();
        alert.add_responses(&[("cancel", "Cancel"), ("restore", "Restore")]);
        alert.set_response_appearance("restore", adw::ResponseAppearance::Destructive);
        if alert.choose_future(&parent).await != "restore" {
            return;
        }

        let allow_local = me.prefs.borrow().allow_local_streams;
        let keep = |list: Vec<Station>| -> Vec<Station> {
            list.into_iter()
                .filter(|s| is_allowed_stream_url(&s.url_resolved, allow_local))
                .collect()
        };
        let mut restored = Vec::new();
        {
            let mut st = me.st.borrow_mut();
            if let (true, Some(list)) = (tags.is_active(), payload.home_tags.take()) {
                st.cached_tags = Some(
                    list.into_iter()
                        .map(|name| BrowseItem {
                            name,
                            stationcount: 0,
                            countrycode: String::new(),
                        })
                        .collect(),
                );
                restored.push("home tags");
            }
            if let (true, Some(list)) = (favs.is_active(), payload.favourites.take()) {
                st.favorites = keep(list);
                restored.push("favourites");
            }
            if let (true, Some(list)) = (pls.is_active(), payload.playlists.take()) {
                st.playlists = keep(list);
                restored.push("playlists");
            }
        }
        if let (true, Some(list)) = (custom.is_active(), payload.custom_tags.take()) {
            me.prefs.borrow_mut().custom_tags = list;
            restored.push("custom tags");
        }
        if restored.is_empty() {
            return;
        }
        me.save();
        me.rebuild_custom_tags();
        me.update_fav_button();
        me.reload_view();
        toast(format!("Restored {}", restored.join(", ")));
    });
}

pub fn show_about(app: &Rc<App>) {
    let about = adw::AboutDialog::builder()
        .application_name("Rust Radio")
        .application_icon("rust-radio")
        .version(env!("CARGO_PKG_VERSION"))
        .developer_name("Antoxa")
        .copyright("© 2026 Antoxa")
        .license_type(gtk::License::Gpl30)
        .website("https://github.com/antoxa78/Rust-Radio")
        .issue_url("https://github.com/antoxa78/Rust-Radio/issues")
        .comments("A GTK4 internet radio player that browses and plays stations from the radio-browser.info database.")
        .debug_info(format!("Built: {}", option_env!("BUILD_DATETIME").unwrap_or("unknown")))
        .build();
    about.present(Some(&app.w.window));
}

const SHORTCUTS_UI: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<interface>
  <object class="GtkShortcutsWindow" id="shortcuts">
    <property name="modal">1</property>
    <child>
      <object class="GtkShortcutsSection">
        <property name="section-name">main</property>
        <child>
          <object class="GtkShortcutsGroup">
            <property name="title">Playback</property>
            <child><object class="GtkShortcutsShortcut"><property name="title">Play / Stop</property><property name="accelerator">space</property></object></child>
            <child><object class="GtkShortcutsShortcut"><property name="title">Next station</property><property name="accelerator">&lt;Control&gt;Right</property></object></child>
            <child><object class="GtkShortcutsShortcut"><property name="title">Previous station</property><property name="accelerator">&lt;Control&gt;Left</property></object></child>
            <child><object class="GtkShortcutsShortcut"><property name="title">Toggle favourite</property><property name="accelerator">&lt;Control&gt;d</property></object></child>
          </object>
        </child>
        <child>
          <object class="GtkShortcutsGroup">
            <property name="title">General</property>
            <child><object class="GtkShortcutsShortcut"><property name="title">Search stations</property><property name="accelerator">&lt;Control&gt;f</property></object></child>
            <child><object class="GtkShortcutsShortcut"><property name="title">Back</property><property name="accelerator">&lt;Alt&gt;Left</property></object></child>
            <child><object class="GtkShortcutsShortcut"><property name="title">Preferences</property><property name="accelerator">&lt;Control&gt;comma</property></object></child>
            <child><object class="GtkShortcutsShortcut"><property name="title">Keyboard shortcuts</property><property name="accelerator">&lt;Control&gt;question</property></object></child>
            <child><object class="GtkShortcutsShortcut"><property name="title">Quit</property><property name="accelerator">&lt;Control&gt;q</property></object></child>
          </object>
        </child>
      </object>
    </child>
  </object>
</interface>
"#;

pub fn show_shortcuts(app: &Rc<App>) {
    let builder = gtk::Builder::from_string(SHORTCUTS_UI);
    if let Some(window) = builder.object::<gtk::ShortcutsWindow>("shortcuts") {
        window.set_transient_for(Some(&app.w.window));
        window.present();
    }
}
