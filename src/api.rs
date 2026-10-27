//! radio-browser.info API client.
//!
//! * One background Tokio runtime for all network work; results are awaited
//!   from the GTK main loop (`run`).
//! * Server discovery via DNS (`all.api.radio-browser.info`), with automatic
//!   failover to the next mirror when a request fails.
//! * Every station list is a `/json/stations/search` query, so bitrate
//!   filters, ordering and "hide broken" work the same everywhere.

use crate::model::{BitrateFilter, BrowseItem, Station, StationSource};
use serde::de::DeserializeOwned;
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;
use std::time::Duration;

pub const INITIAL_STATION_LIMIT: usize = 250;
pub const STATION_PAGE_SIZE: usize = 50;
pub const MAX_STATION_LIMIT: usize = 5_000;
pub const MAX_CATEGORIES: usize = 100;
const MAX_API_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
const FALLBACK_HOSTS: [&str; 3] = [
    "de1.api.radio-browser.info",
    "fi1.api.radio-browser.info",
    "de2.api.radio-browser.info",
];

pub fn user_agent() -> String {
    format!("RustRadio/{}", env!("CARGO_PKG_VERSION"))
}

fn runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("rust-radio-net")
            .build()
            .expect("Failed to create Tokio runtime")
    })
}

fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .user_agent(user_agent())
            .connect_timeout(Duration::from_secs(8))
            .timeout(Duration::from_secs(25))
            .pool_idle_timeout(Duration::from_secs(120))
            .build()
            .expect("Failed to create HTTP client")
    })
}

/// Run `fut` on the network runtime. The returned future can be awaited from
/// the GTK main loop; it yields `None` if the task was aborted.
pub fn run<F>(
    fut: F,
) -> (
    impl Future<Output = Option<F::Output>>,
    tokio::task::AbortHandle,
)
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let handle = runtime().spawn(fut);
    let abort = handle.abort_handle();
    (async move { handle.await.ok() }, abort)
}

/// Fire and forget.
pub fn spawn<F>(fut: F)
where
    F: Future<Output = ()> + Send + 'static,
{
    runtime().spawn(fut);
}

/// Run a blocking closure on the runtime's blocking pool.
pub fn blocking<T, F>(f: F) -> impl Future<Output = Option<T>>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let handle = runtime().spawn_blocking(f);
    async move { handle.await.ok() }
}

// ---------------------------------------------------------------------------
// Server discovery and failover

static HOSTS: tokio::sync::OnceCell<Vec<String>> = tokio::sync::OnceCell::const_new();
static CURRENT_HOST: AtomicUsize = AtomicUsize::new(0);

/// Base URLs (`https://host`) of the API mirrors, in the order to try them.
/// `RUST_RADIO_API=http://127.0.0.1:8080` points the app at a single
/// custom/test server instead.
async fn hosts() -> &'static [String] {
    HOSTS
        .get_or_init(|| async {
            if let Ok(base) = std::env::var("RUST_RADIO_API") {
                let base = base.trim().trim_end_matches('/').to_string();
                if base.starts_with("http://") || base.starts_with("https://") {
                    return vec![base];
                }
            }
            discover_hosts()
                .await
                .into_iter()
                .map(|host| format!("https://{}", host))
                .collect()
        })
        .await
}

fn display_host(base: &str) -> &str {
    base.split_once("://").map(|(_, h)| h).unwrap_or(base)
}

/// Resolve `all.api.radio-browser.info`, reverse-lookup every IP to get the
/// mirror names, and return them in random order. Falls back to a fixed list.
async fn discover_hosts() -> Vec<String> {
    use std::net::ToSocketAddrs;
    let fallback = || {
        FALLBACK_HOSTS
            .iter()
            .map(|h| h.to_string())
            .collect::<Vec<_>>()
    };
    let ips = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::task::spawn_blocking(|| {
            ("all.api.radio-browser.info", 443u16)
                .to_socket_addrs()
                .map(|iter| iter.map(|sa| sa.ip()).collect::<Vec<_>>())
        }),
    )
    .await;
    let ips = match ips {
        Ok(Ok(Ok(v))) if !v.is_empty() => v,
        _ => return fallback(),
    };
    let mut lookups = tokio::task::JoinSet::new();
    for ip in ips {
        lookups.spawn_blocking(move || dns_lookup::lookup_addr(&ip));
    }
    let mut names: Vec<String> = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(3), async {
        while let Some(result) = lookups.join_next().await {
            if let Ok(Ok(hostname)) = result {
                let host = hostname.trim_end_matches('.').to_string();
                if host.ends_with(".api.radio-browser.info") && !names.contains(&host) {
                    names.push(host);
                }
            }
        }
    })
    .await;
    lookups.detach_all();
    if names.is_empty() {
        return fallback();
    }
    names.sort();
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos() as usize;
    let count = names.len();
    names.rotate_left(seed % count);
    // Keep the fallbacks at the end in case every discovered mirror fails.
    for host in FALLBACK_HOSTS {
        if !names.iter().any(|n| n == host) {
            names.push(host.to_string());
        }
    }
    names
}

/// Discover the mirrors and open a connection while the UI is being built.
pub fn prewarm() {
    spawn(async {
        let hosts = hosts().await;
        if let Some(host) = hosts.first() {
            let _ = client().head(format!("{}/json/stats", host)).send().await;
        }
    });
}

/// GET `path` (e.g. `/json/tags?...`) from the current mirror, failing over
/// to the next mirrors on network errors or 5xx responses.
pub async fn get_json<T: DeserializeOwned>(path: &str) -> Result<T, String> {
    let hosts = hosts().await;
    let start = CURRENT_HOST.load(Ordering::Relaxed);
    let attempts = hosts.len().min(3);
    let mut last_error = String::from("no radio-browser server available");
    for attempt in 0..attempts {
        let index = (start + attempt) % hosts.len();
        let url = format!("{}{}", hosts[index], path);
        let host = display_host(&hosts[index]);
        match client().get(&url).send().await {
            Ok(response) if response.status().is_server_error() => {
                last_error = format!("{} answered {}", host, response.status());
            }
            Ok(response) => {
                if attempt > 0 {
                    CURRENT_HOST.store(index, Ordering::Relaxed);
                }
                return read_json_limited(response).await;
            }
            Err(err) => {
                last_error = if err.is_timeout() {
                    format!("{} timed out", host)
                } else if err.is_connect() {
                    format!("couldn't connect to {}", host)
                } else {
                    err.to_string()
                };
            }
        }
    }
    Err(last_error)
}

async fn read_json_limited<T: DeserializeOwned>(response: reqwest::Response) -> Result<T, String> {
    let response = response.error_for_status().map_err(|err| err.to_string())?;
    if response
        .content_length()
        .is_some_and(|length| length > MAX_API_RESPONSE_BYTES as u64)
    {
        return Err("response is too large".to_string());
    }
    let body = response.bytes().await.map_err(|err| err.to_string())?;
    if body.len() > MAX_API_RESPONSE_BYTES {
        return Err("response is too large".to_string());
    }
    serde_json::from_slice(&body).map_err(|err| format!("invalid response: {}", err))
}

// ---------------------------------------------------------------------------
// Query building

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CategoryKind {
    Tags,
    Languages,
    Countries,
}

impl CategoryKind {
    pub fn path(self) -> String {
        let endpoint = match self {
            CategoryKind::Tags => "tags",
            CategoryKind::Languages => "languages",
            CategoryKind::Countries => "countries",
        };
        // Ask for a few more than shown, since entries with blank names are dropped.
        format!(
            "/json/{}?order=stationcount&reverse=true&hidebroken=true&limit={}",
            endpoint,
            MAX_CATEGORIES + 50
        )
    }
}

pub async fn fetch_categories(kind: CategoryKind) -> Result<Vec<BrowseItem>, String> {
    let items: Vec<BrowseItem> = get_json(&kind.path()).await?;
    Ok(items
        .into_iter()
        .filter(|c| !c.name.trim().is_empty())
        .take(MAX_CATEGORIES)
        .collect())
}

pub struct StationQueryOptions {
    pub bitrate: BitrateFilter,
    pub prefer_high_bitrate: bool,
    pub hide_broken: bool,
}

/// Search path (without `limit`/`offset`) for a station list.
pub fn station_search_path(source: &StationSource, opts: &StationQueryOptions) -> String {
    let enc = |s: &str| urlencoding::encode(s).into_owned();
    let mut params: Vec<String> = Vec::new();
    match source {
        StationSource::Tag(t) | StationSource::CustomTag(t) => {
            params.push(format!("tag={}", enc(t)))
        }
        StationSource::Language(l) => params.push(format!("language={}", enc(l))),
        StationSource::Country(c) => {
            params.push(format!("country={}", enc(c)));
            params.push("countryExact=true".into());
        }
        StationSource::Search(q) => params.push(format!("name={}", enc(q))),
        StationSource::TopVoted => {}
    }
    match opts.bitrate {
        BitrateFilter::Any => {}
        BitrateFilter::Low => params.push("bitrateMax=160".into()),
        BitrateFilter::High => params.push("bitrateMin=192".into()),
        BitrateFilter::Flac => params.push("codec=FLAC".into()),
    }
    let order = if *source == StationSource::TopVoted {
        "votes"
    } else if opts.prefer_high_bitrate {
        "bitrate"
    } else {
        "clickcount"
    };
    params.push(format!("order={}", order));
    params.push("reverse=true".into());
    if opts.hide_broken {
        params.push("hidebroken=true".into());
    }
    format!("/json/stations/search?{}", params.join("&"))
}

/// Find the start of the value for query parameter `key` (exact name match).
fn url_param_value_start(url: &str, key: &str) -> Option<usize> {
    let query_start = url.find('?')? + 1;
    let mut pos = query_start;
    for pair in url[query_start..].split('&') {
        if let Some(rest) = pair.strip_prefix(key) {
            if rest.starts_with('=') {
                return Some(pos + key.len() + 1);
            }
        }
        pos += pair.len() + 1;
    }
    None
}

fn param_value<'a>(url: &'a str, key: &str) -> Option<&'a str> {
    let start = url_param_value_start(url, key)?;
    Some(url[start..].split('&').next().unwrap_or(""))
}

pub fn parse_usize_param(url: &str, key: &str) -> Option<usize> {
    let digits: String = param_value(url, key)?
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().ok()
}

pub fn set_usize_param(mut url: String, key: &str, value: usize) -> String {
    if let Some(start) = url_param_value_start(&url, key) {
        let end = url[start..]
            .find(|c: char| !c.is_ascii_digit())
            .map(|offset| start + offset)
            .unwrap_or(url.len());
        url.replace_range(start..end, &value.to_string());
    } else {
        let separator = if !url.contains('?') {
            "?"
        } else if url.ends_with('?') || url.ends_with('&') {
            ""
        } else {
            "&"
        };
        url.push_str(&format!("{}{}={}", separator, key, value));
    }
    url
}

pub fn clamp_station_limit(limit: usize) -> usize {
    limit.clamp(INITIAL_STATION_LIMIT, MAX_STATION_LIMIT)
}

/// The server filters and sorts by the raw bitrate value, so a station
/// listed as 128000 passes "≥192" and sorts first. After normalizing,
/// re-apply the query's bitrate bounds and ordering locally.
pub fn apply_bitrate_query_locally(path: &str, mut stations: Vec<Station>) -> Vec<Station> {
    let min = parse_usize_param(path, "bitrateMin");
    let max = parse_usize_param(path, "bitrateMax");
    if min.is_some() || max.is_some() {
        stations.retain(|s| match s.bitrate {
            Some(b) => {
                let b = b as usize;
                min.is_none_or(|min| b >= min) && max.is_none_or(|max| b <= max)
            }
            None => true,
        });
    }
    if param_value(path, "order") == Some("bitrate") {
        if param_value(path, "reverse") == Some("true") {
            stations.sort_by_key(|s| std::cmp::Reverse(s.bitrate.unwrap_or(0)));
        } else {
            stations.sort_by_key(|s| s.bitrate.unwrap_or(0));
        }
    }
    stations
}

/// Fetch one page of a station search.
pub async fn fetch_station_page(
    search_path: String,
    offset: usize,
    limit: usize,
    allow_local: bool,
) -> Result<(Vec<Station>, bool), String> {
    let path = set_usize_param(
        set_usize_param(search_path, "limit", limit),
        "offset",
        offset,
    );
    let stations: Vec<Station> = get_json(&path).await?;
    let has_more = stations.len() >= limit;
    let hide_broken = param_value(&path, "hidebroken") == Some("true");
    let stations = stations
        .into_iter()
        .filter(|s| crate::net_safety::is_allowed_stream_url(&s.url_resolved, allow_local))
        .filter(|s| !hide_broken || s.lastcheckok == 1)
        .collect();
    Ok((apply_bitrate_query_locally(&path, stations), has_more))
}

#[derive(serde::Deserialize)]
struct Stats {
    lastchangetime: String,
}

pub async fn fetch_last_change_time() -> Option<String> {
    get_json::<Stats>("/json/stats")
        .await
        .ok()
        .map(|s| s.lastchangetime)
}

/// radio-browser asks clients to report each station play ("click
/// counting"), which feeds its popularity rankings.
pub fn count_click(stationuuid: &str) {
    if stationuuid.is_empty() {
        return;
    }
    let path = format!("/json/url/{}", urlencoding::encode(stationuuid));
    spawn(async move {
        let _ = get_json::<serde_json::Value>(&path).await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(bitrate: BitrateFilter, prefer_high: bool, hide_broken: bool) -> StationQueryOptions {
        StationQueryOptions {
            bitrate,
            prefer_high_bitrate: prefer_high,
            hide_broken,
        }
    }

    #[test]
    fn search_paths_use_documented_params() {
        let p = station_search_path(
            &StationSource::Tag("rock & roll".into()),
            &opts(BitrateFilter::High, true, true),
        );
        assert_eq!(p, "/json/stations/search?tag=rock%20%26%20roll&bitrateMin=192&order=bitrate&reverse=true&hidebroken=true");
        assert!(!p.contains("lastcheckok"));
        let p = station_search_path(
            &StationSource::Country("Niger".into()),
            &opts(BitrateFilter::Any, false, false),
        );
        assert_eq!(
            p,
            "/json/stations/search?country=Niger&countryExact=true&order=clickcount&reverse=true"
        );
        let p = station_search_path(
            &StationSource::TopVoted,
            &opts(BitrateFilter::Flac, true, true),
        );
        assert_eq!(
            p,
            "/json/stations/search?codec=FLAC&order=votes&reverse=true&hidebroken=true"
        );
        let p = station_search_path(
            &StationSource::Search("jazz fm".into()),
            &opts(BitrateFilter::Low, false, true),
        );
        assert!(
            p.starts_with("/json/stations/search?name=jazz%20fm&bitrateMax=160&order=clickcount")
        );
    }

    #[test]
    fn prefer_high_bitrate_changes_order() {
        let a = station_search_path(
            &StationSource::Tag("jazz".into()),
            &opts(BitrateFilter::Any, true, true),
        );
        let b = station_search_path(
            &StationSource::Tag("jazz".into()),
            &opts(BitrateFilter::Any, false, true),
        );
        assert_ne!(a, b);
    }

    #[test]
    fn page_params_are_set_and_replaced() {
        let base = "/json/stations/search?tag=rock&order=bitrate&reverse=true".to_string();
        let url = set_usize_param(set_usize_param(base, "limit", 50), "offset", 250);
        assert_eq!(parse_usize_param(&url, "limit"), Some(50));
        assert_eq!(parse_usize_param(&url, "offset"), Some(250));
        let url = set_usize_param(url, "offset", 300);
        assert_eq!(url.matches("offset=").count(), 1);
        // A search-term value containing "limit=" must not be mistaken for the param.
        assert_eq!(
            parse_usize_param("/s?name=xlimit=9&limit=20", "limit"),
            Some(20)
        );
        assert_eq!(
            set_usize_param("/s?tag=a&".to_string(), "limit", 5),
            "/s?tag=a&limit=5"
        );
        assert_eq!(clamp_station_limit(1), INITIAL_STATION_LIMIT);
        assert_eq!(
            clamp_station_limit(MAX_STATION_LIMIT + 1),
            MAX_STATION_LIMIT
        );
    }

    #[test]
    fn local_bitrate_reapplied() {
        let json = r#"[
            {"stationuuid":"a","name":"A","url_resolved":"https://a.example/s","tags":"","country":"","bitrate":192000},
            {"stationuuid":"b","name":"B","url_resolved":"https://b.example/s","tags":"","country":"","bitrate":320},
            {"stationuuid":"c","name":"C","url_resolved":"https://c.example/s","tags":"","country":"","bitrate":128000},
            {"stationuuid":"d","name":"D","url_resolved":"https://d.example/s","tags":"","country":"","bitrate":null}
        ]"#;
        let stations: Vec<Station> = serde_json::from_str(json).unwrap();
        let path = "/json/stations/search?bitrateMin=192&order=bitrate&reverse=true&limit=250";
        let ids: Vec<String> = apply_bitrate_query_locally(path, stations.clone())
            .into_iter()
            .map(|s| s.stationuuid)
            .collect();
        assert_eq!(ids, ["b", "a", "d"]);
        let same = apply_bitrate_query_locally("/json/stations/search?name=x", stations.clone());
        assert_eq!(same, stations);
    }
}
