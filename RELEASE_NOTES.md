# Release Notes

## v1.3.0

### New Features
- **"Load More Stations" button**: Station lists now display a "Load More Stations" button at the bottom, fetching 50 additional stations per click — no more hard 250-station cap
- **Real-time station count**: Counter shows the actual total number of stations loaded rather than a fixed limit
- **Scroll position restored after loading more**: The view automatically scrolls back to the first newly loaded station after "Load More"

### UX Improvements
- **"Updating stations..." message** shown during station fetch so the UI never appears frozen
- **Faster sync**: DNS host resolution is cached after the first lookup, reducing subsequent fetch delays
- **App logo in About dialog**: The application icon now appears in the About dialog

### Bug Fixes
- **Station count persistence**: The working station count now persists correctly across preference window opens
- **Live stats update**: "Total Working Stations Loaded" counter updates in real time instead of staying at 0 until the next window open

## v1.2.0

### Improvements
- **Stable stream title during buffering**: Station title no longer jumps while buffering — buffering progress is now shown appended to the stream properties line (e.g. `MP3 · 128 kbps · ⏳ 42%`)
- **Live stats in preferences**: "Total Working Stations Loaded" counter now updates dynamically instead of staying at 0 until the next window open
- **Faster sync**: DNS host resolution is cached after the first lookup, reducing subsequent fetch delays
- **App logo in About dialog**: The application icon now appears in the About dialog

## v1.1.0

### New Features
- **Audio Buffer Settings**: Added "Disable Audio Buffer (Faster Start)" toggle in Preferences → Audio Output
  - Disabling the buffer skips GStreamer prebuffering, starting playback immediately
  - Useful for low-latency listening or when buffering causes stream delays

### Packaging
- Debian `.deb` packaging via `cargo deb` with version tracking

## v0.1.8

### Project Renamed
- **rust-radio-gtk** → **Rust Radio** (binary renamed to `rust-radio`)
- Desktop file, icons, and .deb package name updated accordingly

### Performance
- **Batch-rendered station lists**: Stations are rendered in small batches (5 per cycle) via idle callbacks, keeping the UI responsive even with 250 stations
- **Deferred Station cloning**: Station data is cloned only when the user toggles a favorite, not during list rendering
- **Pre-computed display data**: All markup escaping, flag emoji lookup, and bitrate formatting happens in a single pass before the idle loop — zero string work in render callbacks
- **Merged flag & bitrate into subtitle**: Eliminates 2 GTK widget creations per station row
- **HashSet for favorites lookup**: O(1) check during pre-computation vs O(N) previously
- **Category caching**: Tags, languages, and countries are cached in-memory after first fetch

### UX Improvements
- **Loading indicator**: "Loading stations..." message shown while fetching and rendering station lists, using large prominent font
- **Deferred label updates**: Browsing info and station count update only after all rows finish rendering

### Bug Fixes
- **Markup parsing errors**: All dynamic text (station names, countries, tags, metadata) is properly escaped with `glib::markup_escape_text()` to prevent Pango markup parse failures
- **Two glib version bridge**: Handles mismatch between glib 0.19 (direct) and 0.20 (via GTK4/libadwaita) with explicit `.to_string()` calls

### Packaging
- Debian `.deb` packaging via `cargo deb`
- Desktop entry, hicolor icons (48×48, 256×256, 512×512)
- Postinst/prerm scripts for icon cache updates

## v0.1.0 — Initial Release

- Browse internet radio stations by **tag**, **language**, or **country** via the [radio-browser.info](https://www.radio-browser.info) API
- **Search** stations by name
- **Top Voted** stations listing
- **Favorites** and **Recently Played** history (persisted to disk)
- **Custom tags** — pin frequently used tags to the sidebar for quick access
- **Import playlists** from M3U/PLS files
- **Bitrate filtering** (Any / Low ≤160 kbps / High ≥192 kbps / FLAC)
- **Sort** stations alphabetically (A–Z / Z–A)
- **Auto-skip** to next station when the selected stream fails to play
- **Auto-update** background refresh every 6 hours (toggleable in Preferences)
- Stream metadata display (codec, bitrate, samplerate)
- Dark‑theme aware colour accent on transport buttons
- Deb packaging via `cargo deb`
