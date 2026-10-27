# Release Notes

## v1.5.0

### New Features
- **Real spectrum analyzer**: the bars now follow the actual audio (GStreamer `spectrum`), instead of random values.
- **Media keys & desktop media controls (MPRIS)**: play/stop, next/previous, volume and "now playing" in GNOME/KDE and `playerctl`.
- **Keyboard shortcuts**: Space, Ctrl+←/→, Ctrl+F, Ctrl+D, Alt+←, Ctrl+, , Ctrl+Q; Ctrl+? shows the list.
- **Allow Local Network Streams** option for home Icecast/Snapcast servers.
- **Mirror failover**: if a radio-browser server fails, the next one is tried automatically.
- **Click counting**: plays are reported to radio-browser (as its API guidelines ask), with a versioned User-Agent.
- Window size and maximized state are remembered; Back now remembers several steps.
- New About dialog and a "Keyboard Shortcuts" window; errors show a Retry button; toasts confirm imports, backups and favourites (with Undo for Clear Recents).

### Bug Fixes
- **Recent view played the wrong station** after the first click (the list re-ordered behind the rows).
- **"Hide Broken Stations" had no effect** — the app sent an unsupported `lastcheckok` parameter; it now uses `hidebroken`.
- **"Prefer High Bitrate" had no effect** on tag/language/country lists; turning it off now sorts by popularity.
- **Launching the app twice opened a second, independent window** (two players, data overwriting each other); it now raises the existing window.
- Offline/DNS failures no longer claim the stream "resolves to a local/private address"; browsing while a station is connecting no longer cancels playback.
- A failing station skips to the next one at most 3 times, and only stations that actually played are added to Recent; a dropped stream reconnects up to 2 times.
- Stop/Play on live radio reconnects instead of resuming a stale, possibly dropped connection.
- Saving no longer blocks the UI (all saves are written in the background).
- A–Z sort no longer permanently re-orders your play history; "Clear Recents" no longer disappears.
- Playlist import: `.m3u8` files, files starting with a BOM, and `#EXTINF` lines with commas inside attributes now import correctly; skipped entries are reported.
- Backup/restore errors are reported instead of silently ignored.
- FLAC filter uses the codec; country lists match exactly (Niger ≠ Nigeria).
- Tag icons match whole words ("spanish" no longer gets the spa icon, "Alaska" no longer gets ska).
- Play time shows hours (`2:01:40`) after the first hour.
- Local-address checks now also cover CGNAT/Tailscale, multicast, reserved and NAT64 ranges.

### Under the Hood
- `main.rs` (6,600 lines) split into focused modules; duplicated handlers removed.
- Upgraded to GStreamer bindings 0.23 (one copy of glib instead of two), slimmer Tokio features, current GTK/libadwaita APIs (`FileDialog`, `PreferencesDialog`, `AlertDialog`, `AboutDialog`).
- Test suite grew from 7 to 25+ tests.

## v1.4.1

### Bug Fixes
- **About dialog no longer disappears behind Preferences**: The About dialog is now transient for the Preferences window it is opened from (and closes with it). Previously, clicking Preferences could raise it above the modal About dialog, hiding it while it still blocked all input — making the app appear frozen.

## v1.4.0

### New Features
- **Instant station lists**: Station lists are now cached in memory and on disk (`~/.cache/RustRadioGTK/stations/`), so previously visited tags, languages, countries, and searches open immediately — even after a restart. Lists younger than 30 minutes skip the network entirely; older ones are shown right away and refreshed in the background.
- **Faster "Load More Stations"**: Only the next page is downloaded and appended, instead of re-fetching the entire list. Already-listed stations are skipped if the server order shifted.
- **Startup network prewarming**: Radio-Browser server discovery and the first connection are prepared while the UI is being built, so the first list opens faster.

### Performance
- **Much faster list rendering**: The first 30 rows are rendered immediately to fill the window, then the rest in batches of 40. Single-line ellipsized titles/subtitles avoid slow height measurement, with the full text available in a tooltip.
- **Background refresh keeps your place**: Auto-updates and stale-cache refreshes redraw the list in place, preserving the scroll position, and are skipped entirely when nothing changed.
- **Saves no longer block the UI**: Favorites, history, playlists, and preferences are serialized and written on a background thread using atomic file replacement.

### Bug Fixes
- **Bitrates reported in bit/s are normalized**: Entries such as `192000` (instead of `192`) no longer break the bitrate filter or sort order — bounds and ordering are re-applied locally after normalization.
- **Station names with special characters**: Names, tags, and URLs containing `&`, `<`, or `>` no longer cause Pango markup failures in favorites, recent, playlists, or category rows.
- **More reliable server discovery**: All mirror reverse lookups run in parallel under a single bounded timeout, concurrent requests share one discovery, and the fallback host is no longer retried on every request.
- **Gzip-compressed API responses** are now accepted, reducing download size for large station lists.

## v1.3.2

### New Features
- **Country flags in the Countries menu**: Country entries now show their ISO country flag before the country name.

### Reliability & Security
- Added request cancellation and stale-response protection for navigation, rendering, and playback callbacks.
- Hardened persistence, playback failure handling, URL validation, pagination, playlist imports, and application shutdown.

## v1.3.1

### Bug Fixes
- **Play/pause no longer panics when a pipeline fails to create** — the currently-playing state is only set after the GStreamer pipeline builds successfully.
- **Imported playlist stations are no longer treated as one favourite** — favourites now compare by UUID for API stations and by URL+name for imported stations.
- **“Load More Stations” now actually fetches more stations** — duplicate `limit=` parameters are replaced instead of ignored.
- **Search queries are URL-encoded** — searches with spaces or special characters now produce valid requests.
- **Playlist remove button now removes the UI row** and updates the station count immediately.
- **Auto-update no longer overwrites Favorites / Recent / Playlist views** — it only refreshes live station lists.
- **Preferences mutex no longer held across async await points** — network fetches no longer block the UI thread on preferences.

### Persistence
- `prefer_high_bitrate`, `hide_broken`, and `auto_update` preferences are now saved and restored.
- `Station::lastcheckok` now has a serde default for backward compatibility with older saved data.

### Reliability & UX
- Added shared Tokio runtime and HTTP client to avoid creating a new thread/runtime per request.
- Failed network/category fetches now surface an error message instead of leaving the UI stuck on “Loading…”.
- Back button now works correctly when navigating from Favorites, Recent, and Playlists.

### Maintenance
- Removed unused `directories` dependency.
- Resolved all outstanding Clippy warnings.

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
