# Rust Radio

A GTK4 internet radio player that browses and plays stations from the [radio-browser.info](https://www.radio-browser.info) database.

Built with [Rust](https://www.rust-lang.org/) · [GTK4](https://gtk.org/) · [libadwaita](https://gnome.pages.gitlab.gnome.org/libadwaita/) · [GStreamer](https://gstreamer.freedesktop.org/)

## Features

- Browse stations by **tag**, **language**, or **country**
- **Search** stations by name
- **Top Voted** stations
- **Favorites** and **Recently Played** history (persisted to disk)
- **Custom tags** — pin frequently used tags to the sidebar
- **Playlist import** (M3U/PLS)
- **Bitrate filtering** (Any / Low ≤160 / High ≥192 / FLAC)
- Sort by name (A–Z / Z–A)
- Auto-update background refresh (configurable, 6h interval)
- **Batch-rendered lists** — station rows rendered in small idle batches for a smooth, responsive UI even with 250 stations
- **Cached categories** — tags, languages, and countries cached after first fetch

## Build from source

### Dependencies

- Rust 2021 edition
- GTK4, libadwaita (0.7), GStreamer, and their development headers

**Debian/Ubuntu:**
```bash
sudo apt install rustc cargo libgtk-4-dev libadwaita-1-dev libgstreamer1.0-dev
```

### Build

```bash
cargo build --release
```

## Install

### Debian package

```bash
cargo deb
sudo dpkg -i target/debian/rust-radio_0.1.8-1_amd64.deb
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
