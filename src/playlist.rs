//! M3U / M3U8 / PLS playlist import.

use crate::model::Station;

pub const MAX_PLAYLIST_STATIONS: usize = 5_000;

#[derive(Debug, Default, PartialEq)]
pub struct ParsedPlaylist {
    pub stations: Vec<Station>,
    /// Entries dropped because their URL isn't an allowed stream URL.
    pub skipped: usize,
}

/// Parse a playlist, detecting the format from its content (not the file
/// extension, which is often wrong or `.m3u8`).
pub fn parse_playlist(content: &str, allow_local: bool) -> ParsedPlaylist {
    let content = content.trim_start_matches('\u{FEFF}');
    let looks_like_pls = content.lines().map(str::trim).any(|line| {
        line.eq_ignore_ascii_case("[playlist]")
            || line
                .get(..4)
                .is_some_and(|p| p.eq_ignore_ascii_case("file") && line.contains('='))
    });
    if looks_like_pls {
        parse_pls(content, allow_local)
    } else {
        parse_m3u(content, allow_local)
    }
}

/// Title of an `#EXTINF:<duration> [attr="..."...],<title>` line: the text
/// after the first comma that isn't inside a quoted attribute value.
fn extinf_title(line: &str) -> Option<String> {
    let rest = line.strip_prefix("#EXTINF:")?;
    let mut in_quotes = false;
    for (i, c) in rest.char_indices() {
        match c {
            '"' => in_quotes = !in_quotes,
            ',' if !in_quotes => {
                let title = rest[i + 1..].trim();
                return (!title.is_empty()).then(|| title.to_string());
            }
            _ => {}
        }
    }
    None
}

pub fn parse_m3u(content: &str, allow_local: bool) -> ParsedPlaylist {
    let mut out = ParsedPlaylist::default();
    let mut next_name: Option<String> = None;
    for line in content.trim_start_matches('\u{FEFF}').lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with("#EXTINF:") {
            next_name = extinf_title(trimmed);
            continue;
        }
        if trimmed.starts_with('#') {
            continue;
        }
        if out.stations.len() >= MAX_PLAYLIST_STATIONS {
            break;
        }
        let name = next_name.take();
        if !crate::net_safety::is_allowed_stream_url(trimmed, allow_local) {
            out.skipped += 1;
            continue;
        }
        out.stations.push(Station::imported(
            name.unwrap_or_else(|| trimmed.to_string()),
            trimmed.to_string(),
        ));
    }
    out
}

pub fn parse_pls(content: &str, allow_local: bool) -> ParsedPlaylist {
    let mut out = ParsedPlaylist::default();
    let mut files: Vec<(String, String)> = Vec::new();
    let mut titles = std::collections::HashMap::new();
    for line in content.trim_start_matches('\u{FEFF}').lines() {
        let trimmed = line.trim();
        let Some((key, value)) = trimmed.split_once('=') else {
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim().to_string();
        if let Some(num) = key.strip_prefix("file") {
            files.push((num.to_string(), value));
        } else if let Some(num) = key.strip_prefix("title") {
            if !value.is_empty() {
                titles.insert(num.to_string(), value);
            }
        }
    }
    for (num, url) in files {
        if out.stations.len() >= MAX_PLAYLIST_STATIONS {
            break;
        }
        if !crate::net_safety::is_allowed_stream_url(&url, allow_local) {
            out.skipped += 1;
            continue;
        }
        let name = titles.get(&num).cloned().unwrap_or_else(|| url.clone());
        out.stations.push(Station::imported(name, url));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(p: &ParsedPlaylist) -> Vec<&str> {
        p.stations.iter().map(|s| s.name.as_str()).collect()
    }

    #[test]
    fn m3u_with_bom_keeps_first_entry() {
        let p = parse_playlist("\u{FEFF}http://a.example/s\nhttp://b.example/s", false);
        assert_eq!(p.stations.len(), 2);
    }

    #[test]
    fn extinf_attributes_with_commas() {
        let p = parse_playlist(
            "#EXTM3U\n#EXTINF:-1 tvg-logo=\"x,y\" group-title=\"A, B\",Real Name\nhttp://a.example/s\n#EXTINF:-1,Plain, with comma\nhttp://b.example/s",
            false,
        );
        assert_eq!(names(&p), ["Real Name", "Plain, with comma"]);
    }

    #[test]
    fn name_does_not_leak_to_next_entry_after_skip() {
        let p = parse_playlist(
            "#EXTINF:-1,Local\nhttp://192.168.1.2/s\nhttp://b.example/s",
            false,
        );
        assert_eq!(p.skipped, 1);
        assert_eq!(names(&p), ["http://b.example/s"]);
        let p = parse_playlist("#EXTINF:-1,Local\nhttp://192.168.1.2/s", true);
        assert_eq!(names(&p), ["Local"]);
    }

    #[test]
    fn pls_any_case_and_detected_by_content() {
        let p = parse_playlist(
            "[playlist]\nfile1=http://a.example/s\ntitle1=A\nFile2=http://b.example/s\nNumberOfEntries=2\nVersion=2",
            false,
        );
        assert_eq!(names(&p), ["A", "http://b.example/s"]);
    }

    #[test]
    fn odd_unicode_never_panics() {
        let pieces = [
            "File",
            "fıle",
            "ﬁle",
            "Title",
            "TİTLE",
            "ß",
            "=",
            "1",
            "http://a.example/x",
            "\n",
            "#EXTINF:",
            ",",
            "\"",
            "[playlist]",
        ];
        let mut seed: u64 = 7;
        for _ in 0..20_000 {
            let mut s = String::new();
            for _ in 0..8 {
                seed = seed
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                s.push_str(pieces[(seed >> 33) as usize % pieces.len()]);
            }
            parse_playlist(&s, false);
        }
    }
}
