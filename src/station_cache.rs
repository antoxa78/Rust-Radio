//! Station-list cache.
//!
//! Two layers:
//! * **Memory** – instant back-and-forth navigation within a session.
//! * **Disk** (`~/.cache/RustRadioGTK/stations/`) – lists open instantly after
//!   a restart too.
//!
//! Lists are keyed by the request path + query *without* the host and the
//! paging parameters (`limit`, `offset`), so the key doesn't depend on which
//! radio-browser mirror served it or how many "Load More" pages were fetched.
//! Every filter that changes the result (tag/language/country, bitrate,
//! order, hide-broken) is part of the query, so different filters never share
//! an entry.
//!
//! The caller decides freshness: a fresh entry is shown without touching the
//! network; a stale one is shown immediately and refreshed in the background.

use crate::Station;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

/// Entries younger than this are shown without contacting the server.
pub const FRESH_SECS: u64 = 30 * 60;
/// Entries older than this are ignored and deleted.
const MAX_AGE_SECS: u64 = 7 * 24 * 60 * 60;
/// Maximum number of lists kept, in memory and on disk.
const MAX_ENTRIES: usize = 40;
/// Refuse to parse cache files larger than this.
const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;
/// Bump when the on-disk format changes; older files are then ignored.
/// 2: bitrates normalized to kbps and re-sorted (older lists may be misordered).
const FORMAT_VERSION: u32 = 2;

#[derive(Clone, Serialize, Deserialize)]
pub struct CacheEntry {
    version: u32,
    key: String,
    /// Unix time (seconds) when the list was last fetched from the server.
    pub fetched_at: u64,
    /// Total number of stations requested from the server (initial limit plus
    /// any "Load More" pages); used as the next page offset.
    pub limit: usize,
    /// Whether the server had more stations beyond `limit`.
    pub has_more: bool,
    pub stations: Vec<Station>,
}

impl CacheEntry {
    pub fn is_fresh(&self) -> bool {
        now().saturating_sub(self.fetched_at) < FRESH_SECS
    }

    fn is_expired(&self) -> bool {
        now().saturating_sub(self.fetched_at) > MAX_AGE_SECS
    }
}

struct MemoryEntry {
    entry: CacheEntry,
    last_used: u64,
}

static MEMORY: OnceLock<Mutex<HashMap<String, MemoryEntry>>> = OnceLock::new();
static USE_COUNTER: AtomicU64 = AtomicU64::new(0);
/// Serialises disk writes and evictions.
static DISK_LOCK: Mutex<()> = Mutex::new(());

fn memory() -> std::sync::MutexGuard<'static, HashMap<String, MemoryEntry>> {
    MEMORY
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn next_use() -> u64 {
    USE_COUNTER.fetch_add(1, Ordering::Relaxed)
}

/// Build the cache key for a station-list request URL.
pub fn key_for(url: &str) -> String {
    let after_scheme = url.find("://").map(|i| &url[i + 3..]).unwrap_or(url);
    let path_and_query = after_scheme
        .find('/')
        .map(|i| &after_scheme[i..])
        .unwrap_or("/");
    let (path, query) = path_and_query
        .split_once('?')
        .unwrap_or((path_and_query, ""));
    let mut params: Vec<&str> = query
        .split('&')
        .filter(|p| !p.is_empty() && !p.starts_with("limit=") && !p.starts_with("offset="))
        .collect();
    params.sort_unstable();
    format!("{}?{}", path, params.join("&"))
}

fn insert_memory(entry: CacheEntry) {
    let mut map = memory();
    map.insert(
        entry.key.clone(),
        MemoryEntry {
            entry,
            last_used: next_use(),
        },
    );
    while map.len() > MAX_ENTRIES {
        let Some(oldest) = map
            .iter()
            .min_by_key(|(_, e)| e.last_used)
            .map(|(k, _)| k.clone())
        else {
            break;
        };
        map.remove(&oldest);
    }
}

/// Look up a list: memory first, then disk. Blocking (may read a file), so
/// call it from a blocking-capable context.
pub fn get(key: &str) -> Option<CacheEntry> {
    {
        let mut map = memory();
        if let Some(found) = map.get_mut(key) {
            if found.entry.is_expired() {
                map.remove(key);
            } else {
                found.last_used = next_use();
                return Some(found.entry.clone());
            }
        }
    }
    let dir = cache_dir()?;
    let entry = read_from(&dir, key)?;
    insert_memory(entry.clone());
    Some(entry)
}

/// Store a freshly fetched list in memory. Call [`persist`] afterwards
/// (off the UI thread) to write it to disk.
pub fn put(key: &str, stations: Vec<Station>, limit: usize, has_more: bool) {
    insert_memory(CacheEntry {
        version: FORMAT_VERSION,
        key: key.to_string(),
        fetched_at: now(),
        limit,
        has_more,
        stations,
    });
}

/// Append a "Load More" page to a cached list (skipping stations already
/// present). Returns `false` if the list isn't cached.
pub fn append(key: &str, new_stations: &[Station], limit: usize, has_more: bool) -> bool {
    let mut map = memory();
    let Some(found) = map.get_mut(key) else {
        return false;
    };
    let entry = &mut found.entry;
    for station in new_stations {
        let duplicate = !station.stationuuid.is_empty()
            && entry
                .stations
                .iter()
                .any(|s| s.stationuuid == station.stationuuid);
        if !duplicate {
            entry.stations.push(station.clone());
        }
    }
    entry.limit = entry.limit.max(limit);
    entry.has_more = has_more;
    found.last_used = next_use();
    true
}

/// Write the current in-memory version of `key` to disk. Blocking.
pub fn persist(key: &str) {
    let entry = match memory().get(key) {
        Some(found) => found.entry.clone(),
        None => return,
    };
    let Some(dir) = cache_dir() else {
        return;
    };
    let _guard = DISK_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    // Re-read under the lock so a slower writer can't replace a newer version.
    let entry = memory()
        .get(key)
        .map(|found| found.entry.clone())
        .unwrap_or(entry);
    if let Err(err) = write_to(&dir, &entry) {
        eprintln!("Failed to write station cache: {}", err);
        return;
    }
    evict_disk(&dir);
}

fn cache_dir() -> Option<PathBuf> {
    let mut dir = gtk::glib::user_cache_dir();
    dir.push("RustRadioGTK");
    dir.push("stations");
    Some(dir)
}

/// Stable 64-bit FNV-1a hash, used for cache file names.
fn fnv1a(text: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn file_for(dir: &Path, key: &str) -> PathBuf {
    dir.join(format!("{:016x}.json", fnv1a(key)))
}

fn read_from(dir: &Path, key: &str) -> Option<CacheEntry> {
    let path = file_for(dir, key);
    let metadata = std::fs::metadata(&path).ok()?;
    if metadata.len() > MAX_FILE_BYTES {
        let _ = std::fs::remove_file(&path);
        return None;
    }
    let bytes = std::fs::read(&path).ok()?;
    match serde_json::from_slice::<CacheEntry>(&bytes) {
        Ok(entry) if entry.version == FORMAT_VERSION && entry.key == key => {
            if entry.is_expired() {
                let _ = std::fs::remove_file(&path);
                None
            } else {
                Some(entry)
            }
        }
        // Hash collision with another key: leave the other key's file alone.
        Ok(entry) if entry.version == FORMAT_VERSION => None,
        _ => {
            let _ = std::fs::remove_file(&path);
            None
        }
    }
}

fn write_to(dir: &Path, entry: &CacheEntry) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let path = file_for(dir, &entry.key);
    let temp_path = path.with_extension("json.tmp");
    let json = serde_json::to_vec(entry).map_err(std::io::Error::other)?;
    let result = (|| {
        let mut file = std::fs::File::create(&temp_path)?;
        file.write_all(&json)?;
        // No fsync: a cache file lost in a crash is simply refetched.
        std::fs::rename(&temp_path, &path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp_path);
    }
    result
}

/// Keep at most `MAX_ENTRIES` cache files, removing the least recently written.
fn evict_disk(dir: &Path) {
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(SystemTime, PathBuf)> = read_dir
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "json"))
        .filter_map(|e| {
            let modified = e.metadata().ok()?.modified().ok()?;
            Some((modified, e.path()))
        })
        .collect();
    if files.len() <= MAX_ENTRIES {
        return;
    }
    files.sort_by_key(|(modified, _)| *modified);
    let excess = files.len() - MAX_ENTRIES;
    for (_, path) in files.into_iter().take(excess) {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn station(uuid: &str) -> Station {
        Station {
            stationuuid: uuid.to_string(),
            name: format!("Station {}", uuid),
            url_resolved: format!("https://radio.example/{}", uuid),
            tags: "rock".to_string(),
            country: "Nowhere".to_string(),
            countrycode: "NW".to_string(),
            lastcheckok: 1,
            bitrate: Some(128),
        }
    }

    #[test]
    fn key_ignores_host_paging_and_param_order() {
        let a = key_for(
            "https://de1.api.radio-browser.info/json/stations/search?tag=rock&order=bitrate&reverse=true&limit=250&lastcheckok=1",
        );
        let b = key_for(
            "https://fi1.api.radio-browser.info/json/stations/search?tag=rock&order=bitrate&reverse=true&limit=50&offset=250&lastcheckok=1",
        );
        assert_eq!(a, b);
        let other_filter = key_for(
            "https://de1.api.radio-browser.info/json/stations/search?tag=rock&bitrateMin=192&order=bitrate&reverse=true&limit=250",
        );
        assert_ne!(a, other_filter);
        let other_tag = key_for(
            "https://de1.api.radio-browser.info/json/stations/search?tag=jazz&order=bitrate&reverse=true&limit=250&lastcheckok=1",
        );
        assert_ne!(a, other_tag);
    }

    #[test]
    fn disk_round_trip_and_key_check() {
        let dir = std::env::temp_dir().join(format!("rr-cache-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let entry = CacheEntry {
            version: FORMAT_VERSION,
            key: "/json/stations/search?tag=rock".to_string(),
            fetched_at: now(),
            limit: 250,
            has_more: true,
            stations: vec![station("a"), station("b")],
        };
        write_to(&dir, &entry).unwrap();
        let read = read_from(&dir, &entry.key).expect("entry should be readable");
        assert_eq!(read.stations, entry.stations);
        assert_eq!(read.limit, 250);
        assert!(read.has_more);
        assert!(read.is_fresh());
        assert!(read_from(&dir, "/json/stations/search?tag=jazz").is_none());

        let mut old = entry.clone();
        old.fetched_at = now() - MAX_AGE_SECS - 10;
        write_to(&dir, &old).unwrap();
        assert!(read_from(&dir, &old.key).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn append_extends_without_duplicates() {
        let key = "/test/append?x=1";
        put(key, vec![station("a"), station("b")], 250, true);
        assert!(append(key, &[station("b"), station("c")], 300, false));
        let entry = get(key).unwrap();
        let uuids: Vec<&str> = entry.stations.iter().map(|s| s.stationuuid.as_str()).collect();
        assert_eq!(uuids, ["a", "b", "c"]);
        assert_eq!(entry.limit, 300);
        assert!(!entry.has_more);
        assert!(!append("/test/missing", &[station("z")], 300, false));
    }
}
