//! Widget tree. Builds the layout only; behaviour is wired up in `ui::mod`.

use crate::model::BitrateFilter;
use adw::prelude::*;
use gtk::{gio, Align, Box, Button, Label, ListBox, Orientation, ScrolledWindow};

pub struct NavButtons {
    pub tags: Button,
    pub top_voted: Button,
    pub languages: Button,
    pub countries: Button,
    pub favorites: Button,
    pub recent: Button,
    pub playlists: Button,
    pub custom_tags: Button,
}

pub struct Widgets {
    pub window: adw::ApplicationWindow,
    pub toasts: adw::ToastOverlay,
    pub nav: NavButtons,
    pub custom_tags_box: Box,
    pub search_entry: gtk::SearchEntry,
    pub sort_btn: Button,
    pub sort_label: Label,
    pub bitrate_area: Box,
    pub bitrate_btn: gtk::MenuButton,
    pub bitrate_popover: gtk::Popover,
    pub bitrate_options: Vec<(BitrateFilter, Button)>,
    pub back_btn: Button,
    pub import_btn: Button,
    pub clear_recents_btn: Button,
    pub title_label: Label,
    pub count_label: Label,
    pub updated_label: Label,
    pub list_box: ListBox,
    pub scrolled: ScrolledWindow,
    pub np_flag: Label,
    pub np_title: Label,
    pub np_meta: Label,
    pub np_info: Label,
    pub time_label: Label,
    pub prev_btn: Button,
    pub play_btn: Button,
    pub next_btn: Button,
    pub fav_btn: Button,
    pub volume_icon: gtk::Image,
    pub volume_scale: gtk::Scale,
    pub analyzer: gtk::DrawingArea,
}

fn css(classes: &[&str]) -> Vec<String> {
    classes.iter().map(|c| c.to_string()).collect()
}

fn nav_button(icon: &str, text: &str) -> Button {
    let hbox = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(8)
        .margin_start(10)
        .halign(Align::Start)
        .build();
    hbox.append(&gtk::Image::from_icon_name(icon));
    hbox.append(&Label::new(Some(text)));
    Button::builder()
        .child(&hbox)
        .css_classes(css(&["flat"]))
        .hexpand(true)
        .margin_top(2)
        .margin_bottom(2)
        .build()
}

fn small_button(icon: &str, text: &str) -> (Button, Label) {
    let hbox = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(4)
        .build();
    let img = gtk::Image::from_icon_name(icon);
    img.set_icon_size(gtk::IconSize::Inherit);
    let label = Label::builder()
        .label(text)
        .css_classes(css(&["caption"]))
        .build();
    hbox.append(&img);
    hbox.append(&label);
    (
        Button::builder()
            .child(&hbox)
            .css_classes(css(&["flat"]))
            .build(),
        label,
    )
}

fn caption(text: &str) -> Label {
    Label::builder()
        .label(text)
        .css_classes(css(&["caption", "dim-label"]))
        .build()
}

impl Widgets {
    pub fn build(
        app: &adw::Application,
        width: i32,
        height: i32,
        maximized: bool,
        bitrate: BitrateFilter,
    ) -> Self {
        let provider = gtk::CssProvider::new();
        provider
            .load_from_data("list row:selected { background-color: alpha(currentColor, 0.12); }");
        if let Some(display) = gtk::gdk::Display::default() {
            gtk::style_context_add_provider_for_display(
                &display,
                &provider,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        }

        let content_box = Box::new(Orientation::Vertical, 0);

        // Header bar with the main menu.
        let header_bar = adw::HeaderBar::new();
        let menu = gio::Menu::new();
        menu.append(Some("Preferences"), Some("app.preferences"));
        menu.append(Some("Keyboard Shortcuts"), Some("app.shortcuts"));
        menu.append(Some("About Rust Radio"), Some("app.about"));
        let menu_btn = gtk::MenuButton::builder()
            .icon_name("open-menu-symbolic")
            .menu_model(&menu)
            .tooltip_text("Main Menu")
            .build();
        header_bar.pack_end(&menu_btn);
        content_box.append(&header_bar);

        // Sidebar.
        let sidebar = Box::builder()
            .orientation(Orientation::Vertical)
            .margin_top(8)
            .margin_bottom(8)
            .build();
        sidebar.append(
            &Label::builder()
                .label("Rust Radio")
                .css_classes(css(&["title-4"]))
                .halign(Align::Start)
                .margin_start(12)
                .margin_bottom(4)
                .build(),
        );
        let nav = NavButtons {
            tags: nav_button("media-optical-cd-audio-symbolic", "Browse By Tag"),
            top_voted: nav_button("view-sort-descending-symbolic", "Top Voted"),
            languages: nav_button("preferences-desktop-locale-symbolic", "Languages"),
            countries: nav_button("find-location-symbolic", "Countries"),
            favorites: nav_button("starred-symbolic", "Favourites"),
            recent: nav_button("document-open-recent-symbolic", "Recent"),
            playlists: nav_button("view-list-symbolic", "Playlists"),
            custom_tags: nav_button("bookmark-new-symbolic", "Custom Tags"),
        };
        for b in [
            &nav.tags,
            &nav.top_voted,
            &nav.languages,
            &nav.countries,
            &nav.favorites,
            &nav.recent,
            &nav.playlists,
            &nav.custom_tags,
        ] {
            sidebar.append(b);
        }
        let custom_tags_box = Box::builder()
            .orientation(Orientation::Vertical)
            .visible(false)
            .build();
        sidebar.append(&custom_tags_box);
        let sidebar_scroll = ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&sidebar)
            .width_request(170)
            .build();

        // Right side: search + filters.
        let right = Box::builder()
            .orientation(Orientation::Vertical)
            .hexpand(true)
            .build();
        let search_entry = gtk::SearchEntry::builder()
            .placeholder_text("Search stations…")
            .width_request(300)
            .tooltip_text("Search global radio stations (Ctrl+F)")
            .build();
        let (sort_btn, sort_label) = small_button("view-sort-ascending-symbolic", "A–Z");
        sort_btn.set_tooltip_text(Some("Sort by name"));
        let sort_row = Box::builder()
            .orientation(Orientation::Horizontal)
            .spacing(4)
            .build();
        sort_row.append(&caption("Sort:"));
        sort_row.append(&sort_btn);

        let bitrate_area = Box::builder()
            .orientation(Orientation::Horizontal)
            .spacing(4)
            .build();
        bitrate_area.append(&caption("Bitrate:"));
        let bitrate_btn = gtk::MenuButton::builder()
            .label(bitrate.button_label())
            .css_classes(css(&["flat"]))
            .build();
        let bitrate_popover = gtk::Popover::new();
        let options_box = Box::new(Orientation::Vertical, 4);
        let bitrate_options: Vec<(BitrateFilter, Button)> = [
            (BitrateFilter::Any, "Any Bitrate"),
            (BitrateFilter::Low, "Low (≤160 kbps)"),
            (BitrateFilter::High, "High (≥192 kbps)"),
            (BitrateFilter::Flac, "FLAC (Lossless)"),
        ]
        .into_iter()
        .map(|(filter, text)| {
            let b = Button::builder().label(text).has_frame(false).build();
            options_box.append(&b);
            (filter, b)
        })
        .collect();
        bitrate_popover.set_child(Some(&options_box));
        bitrate_btn.set_popover(Some(&bitrate_popover));
        bitrate_area.append(&bitrate_btn);

        let header_search = Box::builder()
            .orientation(Orientation::Horizontal)
            .spacing(12)
            .margin_start(12)
            .margin_end(12)
            .margin_top(6)
            .margin_bottom(4)
            .build();
        header_search.append(&search_entry);
        header_search.append(&sort_row);
        header_search.append(&bitrate_area);
        right.append(&header_search);

        let (back_btn, _) = small_button("go-previous-symbolic", "Back");
        back_btn.set_tooltip_text(Some("Back (Alt+Left)"));
        let import_btn = Button::builder()
            .label("Import Playlist…")
            .css_classes(css(&["flat"]))
            .visible(false)
            .build();
        let clear_recents_btn = Button::builder()
            .label("Clear Recents")
            .css_classes(css(&["flat"]))
            .visible(false)
            .build();
        let shelf = Box::builder()
            .orientation(Orientation::Horizontal)
            .spacing(12)
            .margin_start(12)
            .margin_end(12)
            .margin_top(2)
            .build();
        shelf.append(&back_btn);
        let spacer = Box::new(Orientation::Horizontal, 0);
        spacer.set_hexpand(true);
        shelf.append(&spacer);
        shelf.append(&import_btn);
        shelf.append(&clear_recents_btn);
        right.append(&shelf);

        let title_label = Label::builder()
            .css_classes(css(&["title-4"]))
            .halign(Align::Start)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .margin_start(12)
            .build();
        let count_label = caption("");
        let updated_label = caption("Last update: Never");
        let counters = Box::builder()
            .orientation(Orientation::Horizontal)
            .spacing(12)
            .margin_end(10)
            .build();
        counters.append(&count_label);
        counters.append(&updated_label);
        let browsing_row = Box::builder()
            .orientation(Orientation::Horizontal)
            .spacing(12)
            .margin_top(4)
            .margin_bottom(2)
            .build();
        browsing_row.append(&title_label);
        browsing_row.append(&counters);
        right.append(&browsing_row);

        let list_box = ListBox::builder()
            .margin_top(6)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .css_classes(css(&["boxed-list"]))
            .build();
        let scrolled = ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&list_box)
            .build();
        right.append(&scrolled);

        let body = gtk::Paned::builder()
            .orientation(Orientation::Horizontal)
            .start_child(&sidebar_scroll)
            .end_child(&right)
            .position(170)
            .vexpand(true)
            .shrink_start_child(true)
            .shrink_end_child(false)
            .build();
        content_box.append(&body);
        content_box.append(&gtk::Separator::new(Orientation::Horizontal));

        // Bottom player bar.
        let np_flag = Label::builder()
            .css_classes(css(&["title-1"]))
            .valign(Align::Center)
            .visible(false)
            .build();
        let np_title = Label::builder()
            .label("No Station Selected")
            .css_classes(css(&["title-2"]))
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .max_width_chars(24)
            .halign(Align::Start)
            .build();
        let np_meta = Label::builder()
            .label("Select a station to start listening")
            .css_classes(css(&["body", "dim-label"]))
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .max_width_chars(40)
            .halign(Align::Start)
            .build();
        let np_info = Label::builder()
            .css_classes(css(&["body", "dim-label"]))
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .max_width_chars(40)
            .halign(Align::Start)
            .build();
        let np_text = Box::builder()
            .orientation(Orientation::Vertical)
            .spacing(2)
            .valign(Align::Center)
            .build();
        np_text.append(&np_title);
        np_text.append(&np_meta);
        np_text.append(&np_info);
        let np_left = Box::builder()
            .orientation(Orientation::Horizontal)
            .spacing(10)
            .valign(Align::Center)
            .margin_start(16)
            .margin_end(8)
            .build();
        np_left.append(&np_flag);
        np_left.append(&np_text);

        let time_label = Label::builder()
            .label("00:00")
            .css_classes(css(&["title-2", "numeric", "dim-label"]))
            .valign(Align::Center)
            .build();
        let round = |icon: &str, tip: &str, extra: &[&str]| {
            let mut classes = vec!["circular"];
            classes.extend_from_slice(extra);
            Button::builder()
                .icon_name(icon)
                .css_classes(css(&classes))
                .tooltip_text(tip)
                .valign(Align::Center)
                .build()
        };
        let prev_btn = round(
            "media-skip-backward-symbolic",
            "Previous station (Ctrl+Left)",
            &[],
        );
        let play_btn = round(
            "media-playback-start-symbolic",
            "Play / Stop (Space)",
            &["suggested-action"],
        );
        let next_btn = round(
            "media-skip-forward-symbolic",
            "Next station (Ctrl+Right)",
            &[],
        );
        let fav_btn = Button::builder()
            .icon_name("non-starred-symbolic")
            .css_classes(css(&["flat"]))
            .tooltip_text("Add to favourites (Ctrl+D)")
            .valign(Align::Center)
            .build();
        let volume_icon = gtk::Image::from_icon_name("audio-volume-high-symbolic");
        let volume_scale = gtk::Scale::builder()
            .orientation(Orientation::Horizontal)
            .adjustment(&gtk::Adjustment::new(1.0, 0.0, 1.0, 0.05, 0.1, 0.0))
            .width_request(100)
            .valign(Align::Center)
            .draw_value(false)
            .build();
        let controls = Box::builder()
            .orientation(Orientation::Horizontal)
            .spacing(8)
            .valign(Align::Center)
            .build();
        for w in [
            time_label.upcast_ref::<gtk::Widget>(),
            prev_btn.upcast_ref(),
            play_btn.upcast_ref(),
            next_btn.upcast_ref(),
            fav_btn.upcast_ref(),
            volume_icon.upcast_ref(),
            volume_scale.upcast_ref(),
        ] {
            controls.append(w);
        }
        let analyzer = gtk::DrawingArea::builder()
            .content_height(60)
            .content_width(320)
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(32)
            .margin_end(16)
            .valign(Align::Center)
            .build();
        let center = gtk::CenterBox::builder()
            .hexpand(true)
            .height_request(88)
            .build();
        center.set_start_widget(Some(&np_left));
        center.set_center_widget(Some(&controls));
        center.set_end_widget(Some(&analyzer));
        content_box.append(&center);

        let toasts = adw::ToastOverlay::new();
        toasts.set_child(Some(&content_box));
        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("Rust Radio")
            .default_width(width.max(600))
            .default_height(height.max(400))
            .content(&toasts)
            .build();
        if maximized {
            window.maximize();
        }

        Widgets {
            window,
            toasts,
            nav,
            custom_tags_box,
            search_entry,
            sort_btn,
            sort_label,
            bitrate_area,
            bitrate_btn,
            bitrate_popover,
            bitrate_options,
            back_btn,
            import_btn,
            clear_recents_btn,
            title_label,
            count_label,
            updated_label,
            list_box,
            scrolled,
            np_flag,
            np_title,
            np_meta,
            np_info,
            time_label,
            prev_btn,
            play_btn,
            next_btn,
            fav_btn,
            volume_icon,
            volume_scale,
            analyzer,
        }
    }

    pub fn toast(&self, text: &str) {
        self.toasts
            .add_toast(adw::Toast::builder().title(text).use_markup(false).build());
    }

    pub fn clear_list(&self) {
        while let Some(child) = self.list_box.first_child() {
            self.list_box.remove(&child);
        }
    }
}
