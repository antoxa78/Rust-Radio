//! Core data types shared by the API layer, the player and the UI.

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Station {
    pub stationuuid: String,
    pub name: String,
    pub url_resolved: String,
    pub tags: String,
    pub country: String,
    #[serde(default)]
    pub countrycode: String,
    #[serde(default)]
    pub lastcheckok: i32,
    #[serde(default, deserialize_with = "deserialize_bitrate_kbps")]
    pub bitrate: Option<u32>,
    #[serde(default)]
    pub codec: String,
}

impl Station {
    /// A station imported from a playlist file (no radio-browser UUID).
    pub fn imported(name: String, url: String) -> Self {
        Station {
            stationuuid: String::new(),
            name,
            url_resolved: url,
            tags: "imported".to_string(),
            country: String::new(),
            countrycode: String::new(),
            lastcheckok: 1,
            bitrate: None,
            codec: String::new(),
        }
    }

    /// Same station? API stations compare by UUID, imported ones by URL + name.
    pub fn same_as(&self, other: &Station) -> bool {
        if !self.stationuuid.is_empty() && !other.stationuuid.is_empty() {
            self.stationuuid == other.stationuuid
        } else {
            self.url_resolved == other.url_resolved && self.name == other.name
        }
    }

    pub fn is_flac(&self) -> bool {
        self.codec.eq_ignore_ascii_case("flac")
            || self.bitrate.is_some_and(|b| b >= 500)
            || self.tags.to_lowercase().contains("flac")
    }
}

/// Some radio-browser entries list their bitrate in bit/s (e.g. `192000`)
/// instead of kbit/s. No real stream comes close to 10,000 kbps, so anything
/// above that is treated as bit/s and converted.
const BITRATE_BPS_THRESHOLD: u32 = 10_000;

pub fn normalize_bitrate_kbps(bitrate: u32) -> u32 {
    if bitrate > BITRATE_BPS_THRESHOLD {
        (bitrate + 500) / 1000
    } else {
        bitrate
    }
}

fn deserialize_bitrate_kbps<'de, D>(deserializer: D) -> Result<Option<u32>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<u32>::deserialize(deserializer)?.map(normalize_bitrate_kbps))
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq)]
pub struct BrowseItem {
    pub name: String,
    #[serde(default)]
    pub stationcount: u32,
    #[serde(default, alias = "iso_3166_1")]
    pub countrycode: String,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AnalyzerType {
    Bars,
    Wave,
    BlocksWithHold,
    DigitalVuBlocks,
}

impl AnalyzerType {
    pub const ALL: [AnalyzerType; 4] = [
        AnalyzerType::Bars,
        AnalyzerType::Wave,
        AnalyzerType::BlocksWithHold,
        AnalyzerType::DigitalVuBlocks,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            AnalyzerType::Bars => "Bars",
            AnalyzerType::Wave => "Wave",
            AnalyzerType::BlocksWithHold => "BlocksWithHold",
            AnalyzerType::DigitalVuBlocks => "DigitalVuBlocks",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            AnalyzerType::Bars => "Classic Audio Bars",
            AnalyzerType::Wave => "Oscilloscope Wave",
            AnalyzerType::BlocksWithHold => "Digital VU Matrix (With Peak Hold)",
            AnalyzerType::DigitalVuBlocks => "Digital VU Blocks (No Hold)",
        }
    }

    pub fn parse(text: &str) -> Self {
        Self::ALL
            .into_iter()
            .find(|a| a.as_str() == text)
            .unwrap_or(AnalyzerType::BlocksWithHold)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BitrateFilter {
    Any,
    Low,
    High,
    Flac,
}

impl BitrateFilter {
    pub fn as_str(self) -> &'static str {
        match self {
            BitrateFilter::Any => "None",
            BitrateFilter::Low => "Low",
            BitrateFilter::High => "High",
            BitrateFilter::Flac => "Flac",
        }
    }

    pub fn parse(text: &str) -> Self {
        match text {
            "Low" => BitrateFilter::Low,
            "High" => BitrateFilter::High,
            "Flac" => BitrateFilter::Flac,
            _ => BitrateFilter::Any,
        }
    }

    pub fn button_label(self) -> &'static str {
        match self {
            BitrateFilter::Any => "⊟ Bitrate",
            BitrateFilter::Low => "⊟ Low ≤160",
            BitrateFilter::High => "⊟ High ≥192",
            BitrateFilter::Flac => "⊟ FLAC",
        }
    }

    /// Filter a local list (favorites, recent, playlists) the same way the
    /// server filters search results.
    pub fn apply(self, stations: &[Station]) -> Vec<Station> {
        let keep = |s: &Station| match self {
            BitrateFilter::Any => true,
            BitrateFilter::Low => s.bitrate.is_none_or(|b| b <= 160),
            BitrateFilter::High => s.bitrate.is_none_or(|b| b >= 192),
            BitrateFilter::Flac => s.is_flac(),
        };
        let mut out: Vec<Station> = stations.iter().filter(|s| keep(s)).cloned().collect();
        if self != BitrateFilter::Any {
            out.sort_by_key(|s| std::cmp::Reverse(s.bitrate.unwrap_or(0)));
        }
        out
    }
}

/// What a station list shows. Every variant maps to one radio-browser
/// `/json/stations/search` query.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum StationSource {
    Tag(String),
    Language(String),
    Country(String),
    CustomTag(String),
    TopVoted,
    Search(String),
}

impl StationSource {
    pub fn title(&self) -> String {
        match self {
            StationSource::Tag(t) => format!("Now browsing: {}", t),
            StationSource::Language(l) => format!("Language: {}", l),
            StationSource::Country(c) => format!("Country: {}", c),
            StationSource::CustomTag(t) => format!("Custom Tags › {}", t),
            StationSource::TopVoted => "Top Voted".to_string(),
            StationSource::Search(q) => format!("Search: {}", q),
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum View {
    Tags,
    Languages,
    Countries,
    Stations(StationSource),
    Favorites,
    Recent,
    Playlists,
}

impl View {
    pub fn is_category(&self) -> bool {
        matches!(self, View::Tags | View::Languages | View::Countries)
    }
}

pub fn country_code_to_flag(code: &str) -> String {
    let code = code.trim();
    if code.len() != 2 || !code.chars().all(|c| c.is_ascii_alphabetic()) {
        return String::new();
    }
    code.chars()
        .filter_map(|c| char::from_u32(0x1F1E6 + (c.to_ascii_uppercase() as u32 - 'A' as u32)))
        .collect()
}

/// `m:ss`-style clock that switches to `h:mm:ss` after an hour.
pub fn format_time(duration: std::time::Duration) -> String {
    let total = duration.as_secs();
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{}:{:02}:{:02}", h, m, s)
    } else {
        format!("{:02}:{:02}", m, s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(uuid: &str, bitrate: Option<u32>, codec: &str) -> Station {
        Station {
            stationuuid: uuid.into(),
            name: uuid.into(),
            url_resolved: format!("https://x.example/{}", uuid),
            tags: String::new(),
            country: String::new(),
            countrycode: String::new(),
            lastcheckok: 1,
            bitrate,
            codec: codec.into(),
        }
    }

    #[test]
    fn bitrates_in_bps_are_converted() {
        let json = r#"[
            {"stationuuid":"a","name":"A","url_resolved":"https://a.example/s","tags":"","country":"","bitrate":192000},
            {"stationuuid":"b","name":"B","url_resolved":"https://b.example/s","tags":"","country":"","bitrate":320},
            {"stationuuid":"d","name":"D","url_resolved":"https://d.example/s","tags":"","country":"","bitrate":null},
            {"stationuuid":"e","name":"E","url_resolved":"https://e.example/s","tags":"","country":""}
        ]"#;
        let stations: Vec<Station> = serde_json::from_str(json).unwrap();
        let rates: Vec<Option<u32>> = stations.iter().map(|s| s.bitrate).collect();
        assert_eq!(rates, [Some(192), Some(320), None, None]);
        assert_eq!(normalize_bitrate_kbps(10_000), 10_000);
        assert_eq!(normalize_bitrate_kbps(127_999), 128);
    }

    #[test]
    fn local_bitrate_filters() {
        let list = vec![
            st("low", Some(64), "MP3"),
            st("high", Some(320), "MP3"),
            st("flac", Some(0), "FLAC"),
            st("unknown", None, "AAC"),
        ];
        let ids = |f: BitrateFilter| -> Vec<String> {
            f.apply(&list).into_iter().map(|s| s.stationuuid).collect()
        };
        assert_eq!(ids(BitrateFilter::Any), ["low", "high", "flac", "unknown"]);
        assert_eq!(ids(BitrateFilter::Low), ["low", "flac", "unknown"]);
        assert_eq!(ids(BitrateFilter::High), ["high", "unknown"]);
        assert_eq!(ids(BitrateFilter::Flac), ["flac"]);
    }

    #[test]
    fn time_and_flags() {
        use std::time::Duration;
        assert_eq!(format_time(Duration::from_secs(75)), "01:15");
        assert_eq!(format_time(Duration::from_secs(7300)), "2:01:40");
        assert_eq!(country_code_to_flag("gb"), "🇬🇧");
        assert_eq!(country_code_to_flag("123"), "");
        assert_eq!(country_code_to_flag("USA"), "");
    }

    #[test]
    fn station_identity() {
        let a = st("a", None, "");
        let mut b = st("a", Some(1), "");
        b.name = "renamed".into();
        assert!(a.same_as(&b));
        let i1 = Station::imported("X".into(), "http://h.example/1".into());
        let i2 = Station::imported("Y".into(), "http://h.example/1".into());
        assert!(!i1.same_as(&i2));
    }
}
