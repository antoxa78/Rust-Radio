//! Pick a symbolic icon for a radio-browser tag.
//!
//! Matching is done on whole words (or word prefixes marked with `*`, or
//! multi-word phrases), so "spanish" no longer matches "spa", "alaska" no
//! longer matches "ska" and "therapy" no longer matches "rap".

const RULES: &[(&str, &[&str])] = &[
    // Audio quality / format
    (
        "audio-headphones-symbolic",
        &[
            "flac",
            "lossless",
            "hifi",
            "hi-fi",
            "audiophile",
            "320",
            "high quality",
        ],
    ),
    // Specific compound genres first, so their parts don't win.
    (
        "weather-clear-night-symbolic",
        &["trip-hop", "triphop", "downtempo", "down tempo"],
    ),
    ("audio-x-generic-symbolic", &["jazz fusion"]),
    (
        "media-record-symbolic",
        &["new wave", "post-punk", "prog", "progressive*"],
    ),
    ("starred-symbolic", &["k-pop", "j-pop", "kpop", "jpop"]),
    // Electronic / club
    (
        "utilities-terminal-symbolic",
        &[
            "electro*",
            "techno",
            "house",
            "trance",
            "edm",
            "dance",
            "dubstep",
            "drum and bass",
            "drum & bass",
            "dnb",
            "d&b",
            "jungle",
            "breakbeat",
            "hardcore",
            "rave",
            "industrial",
            "noise",
        ],
    ),
    // Hip-hop / urban
    (
        "microphone-sensitivity-high-symbolic",
        &[
            "hip-hop", "hiphop", "hip hop", "rap", "trap", "grime", "drill",
        ],
    ),
    // Rock / metal
    (
        "media-optical-cd-audio-symbolic",
        &[
            "metal",
            "punk",
            "grunge",
            "thrash",
            "doom",
            "heavy",
            "rock",
            "j-rock",
            "rockabilly",
        ],
    ),
    // Classical / instrumental
    (
        "emblem-music-symbolic",
        &[
            "classical",
            "orchestra*",
            "symphon*",
            "opera",
            "chamber",
            "baroque",
            "choral",
            "philharmonic",
            "instrumental*",
            "piano",
            "guitar",
            "violin",
            "cello",
            "trumpet",
            "saxophone",
        ],
    ),
    // Jazz / blues
    (
        "audio-x-generic-symbolic",
        &[
            "jazz",
            "swing",
            "big band",
            "bebop",
            "dixieland",
            "blues",
            "boogie",
        ],
    ),
    // Religious (before soul so "gospel" lands here)
    (
        "emblem-default-symbolic",
        &[
            "christian",
            "gospel",
            "spiritual",
            "religio*",
            "worship",
            "church",
            "catholic",
            "orthodox",
            "islamic",
            "quran",
            "jewish",
            "buddhist",
        ],
    ),
    // Soul / R&B / funk
    (
        "emblem-favorite-symbolic",
        &["soul", "funk", "rnb", "r&b", "motown", "rhythm"],
    ),
    // Ambient / chill
    (
        "weather-clear-night-symbolic",
        &[
            "ambient",
            "chill*",
            "lofi",
            "lo-fi",
            "relax*",
            "meditation",
            "sleep",
            "new age",
            "newage",
            "nature",
            "spa",
            "healing",
            "lounge",
            "easy listening",
        ],
    ),
    // Talk / news / info
    (
        "dialog-information-symbolic",
        &[
            "news",
            "talk",
            "speech",
            "podcast*",
            "interview*",
            "debate",
            "current affairs",
            "audiobook*",
            "audio book",
            "audio drama",
            "public radio",
            "npr",
            "community",
            "traffic",
            "weather",
            "emergency",
            "politic*",
            "government",
        ],
    ),
    // Sports
    (
        "applications-games-symbolic",
        &[
            "sport*",
            "football",
            "soccer",
            "basketball",
            "baseball",
            "cricket",
            "rugby",
            "tennis",
        ],
    ),
    // Children / education / comedy
    (
        "face-smile-symbolic",
        &[
            "child*",
            "kids",
            "school",
            "education*",
            "learn*",
            "student*",
            "college",
            "universit*",
            "campus",
        ],
    ),
    (
        "face-laugh-symbolic",
        &["comedy", "humor", "humour", "satire"],
    ),
    // Decades / oldies
    (
        "document-open-recent-symbolic",
        &[
            "50s",
            "60s",
            "70s",
            "80s",
            "90s",
            "00s",
            "2000s",
            "oldies",
            "vintage",
            "retro",
            "golden*",
            "classic hits",
            "throwback",
        ],
    ),
    // Pop / charts
    (
        "starred-symbolic",
        &[
            "pop",
            "hits",
            "top 40",
            "top40",
            "chart*",
            "christmas",
            "holiday",
            "xmas",
            "adult contemporary",
        ],
    ),
    // Country / folk
    (
        "find-location-symbolic",
        &[
            "country",
            "folk",
            "bluegrass",
            "acoustic",
            "singer-songwriter",
            "americana",
            "roots",
            "celtic",
            "irish",
            "scottish",
            "welsh",
            "breton",
            "afro*",
            "african",
            "highlife",
            "juju",
            "mbalax",
        ],
    ),
    // Latin / South Asian
    (
        "emoji-activities-symbolic",
        &[
            "latin*",
            "salsa",
            "reggaeton",
            "cumbia",
            "bachata",
            "bossa*",
            "samba",
            "merengue",
            "tango",
            "flamenco",
            "fado",
            "spanish",
            "espanol",
            "español",
            "indian",
            "hindi",
            "bollywood",
            "bhangra",
            "carnatic",
            "filmi",
            "bengali",
            "tamil",
            "telugu",
            "malayalam",
            "punjabi",
            "thai",
            "vietnamese",
            "indonesian",
            "philippine",
            "filipino",
            "malay",
        ],
    ),
    // Reggae / Mediterranean / Middle East
    (
        "weather-clear-symbolic",
        &[
            "reggae",
            "ska",
            "dub",
            "dancehall",
            "arabic",
            "arab",
            "khaleeji",
            "middle east",
            "levant",
            "persian",
            "farsi",
            "iranian",
            "turkish",
            "kurdish",
            "greek",
            "laika",
            "rebetiko",
        ],
    ),
    // World
    (
        "applications-internet-symbolic",
        &[
            "world",
            "international",
            "global",
            "multicultural",
            "japanese",
            "anime",
            "korean",
            "chinese",
            "mandarin",
            "cantonese",
            "taiwanese",
        ],
    ),
    // Indie / live
    (
        "media-record-symbolic",
        &[
            "indie",
            "alternative",
            "live",
            "concert",
            "festival",
            "karaoke",
        ],
    ),
    // European
    (
        "emblem-music-symbolic",
        &[
            "russian",
            "ukraine",
            "ukrainian",
            "polish",
            "czech",
            "slovak",
            "balkan",
            "serbian",
            "croatian",
            "bulgarian",
            "romanian",
            "german",
            "deutsch",
            "schlager",
            "volksmusik",
        ],
    ),
    (
        "emblem-favorite-symbolic",
        &[
            "french",
            "chanson",
            "italian",
            "italiano",
            "cantautore",
            "urban",
            "contemporary",
        ],
    ),
    (
        "weather-clear-night-symbolic",
        &[
            "nordic",
            "scandinavian",
            "swedish",
            "norwegian",
            "danish",
            "finnish",
        ],
    ),
    (
        "video-display-symbolic",
        &["soundtrack*", "film", "movie*", "cinema", "score"],
    ),
    (
        "audio-input-microphone-symbolic",
        &["a cappella", "acappella", "vocal*", "choir"],
    ),
    ("view-list-symbolic", &["variety", "mix", "various"]),
    // Generic
    (
        "audio-x-generic-symbolic",
        &["music", "musica", "música", "musik", "musique"],
    ),
    (
        "audio-input-microphone-symbolic",
        &["radio", "broadcast*", "stream*", "fm", "am"],
    ),
];

fn words(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !(c.is_alphanumeric() || c == '-' || c == '&'))
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect()
}

fn matches(keyword: &str, tag_words: &[String], joined: &str) -> bool {
    if keyword.contains(' ') {
        // Phrase: match on word boundaries within the normalised tag.
        let padded = format!(" {} ", joined);
        return padded.contains(&format!(" {} ", keyword));
    }
    if let Some(prefix) = keyword.strip_suffix('*') {
        return tag_words.iter().any(|w| w.starts_with(prefix));
    }
    tag_words.iter().any(|w| w == keyword)
}

pub fn tag_to_icon(tag: &str) -> &'static str {
    let tag_words = words(tag);
    let joined = tag_words.join(" ");
    for (icon, keywords) in RULES {
        if keywords.iter().any(|k| matches(k, &tag_words, &joined)) {
            return icon;
        }
    }
    "audio-x-generic-symbolic"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_false_substring_matches() {
        assert_eq!(tag_to_icon("spanish"), "emoji-activities-symbolic");
        assert_eq!(tag_to_icon("español"), "emoji-activities-symbolic");
        assert_eq!(tag_to_icon("alaska"), "audio-x-generic-symbolic");
        assert_eq!(tag_to_icon("dublin"), "audio-x-generic-symbolic");
        assert_eq!(tag_to_icon("therapy"), "audio-x-generic-symbolic");
        assert_eq!(tag_to_icon("lighthouse"), "audio-x-generic-symbolic");
    }

    #[test]
    fn genres_and_compounds() {
        assert_eq!(tag_to_icon("ska"), "weather-clear-symbolic");
        assert_eq!(
            tag_to_icon("christian rap"),
            "microphone-sensitivity-high-symbolic"
        );
        assert_eq!(tag_to_icon("gospel"), "emblem-default-symbolic");
        assert_eq!(tag_to_icon("jazz fusion"), "audio-x-generic-symbolic");
        assert_eq!(tag_to_icon("k-pop"), "starred-symbolic");
        assert_eq!(tag_to_icon("Deep House"), "utilities-terminal-symbolic");
        assert_eq!(tag_to_icon("electronic"), "utilities-terminal-symbolic");
        assert_eq!(
            tag_to_icon("classic rock"),
            "media-optical-cd-audio-symbolic"
        );
        assert_eq!(tag_to_icon("trip-hop"), "weather-clear-night-symbolic");
        assert_eq!(tag_to_icon("news talk"), "dialog-information-symbolic");
        assert_eq!(tag_to_icon("80s"), "document-open-recent-symbolic");
    }
}
