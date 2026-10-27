# Rust Radio

A GTK4 internet radio player that browses and plays stations from the [radio-browser.info](https://www.radio-browser.info) database.

Built with [Rust](https://www.rust-lang.org/) · [GTK4](https://gtk.org/) · [libadwaita](https://gnome.pages.gitlab.gnome.org/libadwaita/) · [GStreamer](https://gstreamer.freedesktop.org/)

## Features

- Browse stations by **tag**, **language**, or **country**, plus **Top Voted** and **Search**
- **Favourites** and **Recently Played** history (only stations that actually played)
- **Custom tags** — pin frequently used tags to the sidebar
- **Playlist import** (M3U, M3U8, PLS — format detected from the content)
- **Bitrate filtering** (Any / Low ≤160 / High ≥192 / FLAC) and A–Z / Z–A sorting
- **Real spectrum analyzer** driven by the audio (four styles)
- **Media keys & desktop media controls** via MPRIS (GNOME, KDE, `playerctl`)
- **Keyboard shortcuts** — Space play/stop, Ctrl+←/→ previous/next, Ctrl+F search, Ctrl+D favourite, Alt+← back (Ctrl+? shows them all)
- **Resilient networking** — automatic failover between radio-browser mirrors, retry on errors, auto-skip of dead stations and reconnect when a stream drops
- **Instant lists** — station lists cached in memory and on disk, refreshed in the background
- Auto-update of the open list (configurable, 6 h interval)
- Backup / restore of favourites, playlists and tags
- Optional playback of streams on your **local network** (Preferences › Audio)

## Build from source

### Dependencies

- Rust (stable, 1.82 or newer)
- GTK ≥ 4.10, libadwaita ≥ 1.5, GStreamer ≥ 1.20 with *gst-plugins-good*, and their development headers
  (Ubuntu 24.04+, Debian 13+, Fedora 40+)

**Debian/Ubuntu:**
```bash
sudo apt install rustc cargo libgtk-4-dev libadwaita-1-dev libgstreamer1.0-dev \
    gstreamer1.0-plugins-good gstreamer1.0-plugins-ugly
```

### Build & test

```bash
cargo build --release
cargo test
```

To develop against a different (or mock) radio-browser server, set
`RUST_RADIO_API=http://127.0.0.1:8080` before starting the app.

### Code layout

| File | Purpose |
|------|---------|
| `src/main.rs` | Startup (single instance) |
| `src/api.rs` | radio-browser client, mirror discovery & failover, query building |
| `src/player.rs` | GStreamer playback, spectrum, buffering |
| `src/mpris.rs` | MPRIS2 D-Bus interface |
| `src/model.rs` | Station / view types, filters |
| `src/playlist.rs` | M3U / PLS import |
| `src/net_safety.rs` | Stream URL validation |
| `src/persistence.rs` | Saved settings, favourites, history |
| `src/station_cache.rs` | Memory + disk cache of station lists |
| `src/icons.rs` | Tag → icon mapping |
| `src/ui/` | Window, navigation, preferences, analyzer drawing |

## Install

### Debian package

```bash
cargo install cargo-deb   # once
cargo deb
sudo dpkg -i target/debian/rust-radio_*_amd64.deb
```

### Manual

```bash
cargo build --release
sudo cp target/release/rust-radio /usr/local/bin/
```

## Support

Enjoying this project? A small donation is greatly appreciated and helps keep it going:

<p align="center">
  <a href="https://www.paypal.me/antoxab1978" target="_blank">
    <img src="https://www.paypalobjects.com/en_US/i/btn/btn_donateCC_LG.gif" alt="Donate with PayPal" />
  </a>
</p>

## License

GNU General Public License v3.0
