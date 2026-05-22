# Rust Radio GTK4

A GTK4 internet radio player that browses and plays stations from the [radio-browser.info](https://www.radio-browser.info) database.

## Features

- Browse stations by **tag**, **language**, or **country**
- **Search** stations by name
- **Top Voted** stations
- **Favorites** and **Recently Played** history
- **Custom tags** — pin frequently used tags to the sidebar
- **Playlist import** (M3U/PLS)
- Bitrate filtering
- Sort by name (A–Z / Z–A)
- Auto-update background refresh (configurable, 6h interval)

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
sudo dpkg -i target/debian/rust-radio-gtk_0.1.0-1_amd64.deb
```

### Manual

```bash
cargo build --release
sudo cp target/release/rust-radio-gtk /usr/local/bin/
```

## License

GNU General Public License v3.0
