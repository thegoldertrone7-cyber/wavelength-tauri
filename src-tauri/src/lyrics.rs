//! Lyrics from LRCLIB, cached on disk.
//!
//! The web view used to call lrclib.net itself. Doing the lookup in the core
//! keeps working when a webview blocks cross-origin requests, gives every song
//! one canonical match, and lets us keep the answer: a solved song never needs
//! the network again, so lyrics survive going offline and reopening the app.

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;

const LRCLIB: &str = "https://lrclib.net/api";
const UA: &str = concat!(
    "Wavelength/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/thegoldertrone7-cyber/wavelength-tauri)"
);

#[derive(Clone, Debug, Default)]
pub struct Wanted {
    pub track: String,
    pub artist: String,
    pub album: String,
    pub duration: i64,
}

fn hash(key: &str) -> u64 {
    // FNV-1a: stable across runs and versions, unlike RandomState.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in key.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

pub fn cache_file(dir: &Path, key: &str) -> PathBuf {
    dir.join("lyrics").join(format!("{:016x}.json", hash(key)))
}

pub fn read_cache(dir: &Path, key: &str) -> Option<Value> {
    if key.is_empty() {
        return None;
    }
    let raw = std::fs::read_to_string(cache_file(dir, key)).ok()?;
    serde_json::from_str::<Value>(&raw).ok()
}

pub fn write_cache(dir: &Path, key: &str, value: &Value) {
    if key.is_empty() {
        return;
    }
    let path = cache_file(dir, key);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(raw) = serde_json::to_vec(value) {
        let tmp = path.with_extension("tmp");
        if std::fs::write(&tmp, raw).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

fn norm(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut space = false;
    for c in s.to_lowercase().chars() {
        if c.is_alphanumeric() {
            out.push(c);
            space = false;
        } else if !space {
            out.push(' ');
            space = true;
        }
    }
    out.trim().to_string()
}

/// Drop "(Official Video)", "[HD]", "(Remastered 2011)" and friends.
fn strip_brackets(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut depth = 0usize;
    for c in s.chars() {
        match c {
            '(' | '[' => depth += 1,
            ')' | ']' => {
                depth = depth.saturating_sub(1)
            }
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn strip_feat(s: &str) -> String {
    let lower = s.to_lowercase();
    for needle in [" feat. ", " feat ", " ft. ", " ft ", " featuring "] {
        if let Some(i) = lower.find(needle) {
            return s[..i].trim().to_string();
        }
    }
    s.trim().to_string()
}

fn split_dash(title: &str) -> Option<(String, String)> {
    for sep in [" - ", " – ", " — "] {
        if let Some((a, b)) = title.split_once(sep) {
            let (a, b) = (a.trim(), b.trim());
            if !a.is_empty() && !b.is_empty() {
                return Some((a.to_string(), b.to_string()));
            }
        }
    }
    None
}

fn has_lyrics(hit: &Value) -> bool {
    let text = hit
        .get("syncedLyrics")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .len()
        > 0;
    let plain = hit
        .get("plainLyrics")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .len()
        > 0;
    let inst = hit
        .get("instrumental")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    text || plain || inst
}

fn score(hit: &Value, want: &Wanted) -> i32 {
    if !has_lyrics(hit) {
        return i32::MIN;
    }
    let synced = hit
        .get("syncedLyrics")
        .and_then(|v| v.as_str())
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    let mut s = if synced { 40 } else { 0 };
    let have_track = norm(hit.get("trackName").and_then(|v| v.as_str()).unwrap_or(""));
    let want_track = norm(&want.track);
    if !have_track.is_empty() && !want_track.is_empty() {
        if have_track == want_track {
            s += 35;
        } else if have_track.contains(&want_track) || want_track.contains(&have_track) {
            // Closer titles win: "Get Lucky" beats "Daft Punk - Get Lucky
            // (Full Video)" among otherwise similar results.
            let (short, long) = (
                have_track.chars().count().min(want_track.chars().count()),
                have_track.chars().count().max(want_track.chars().count()),
            );
            s += 8 + (10.0 * short as f64 / long.max(1) as f64) as i32;
        }
    }
    let have_artist = norm(hit.get("artistName").and_then(|v| v.as_str()).unwrap_or(""));
    let want_artist = norm(&want.artist);
    if !have_artist.is_empty() && !want_artist.is_empty() {
        if have_artist == want_artist {
            s += 25;
        } else if have_artist.contains(&want_artist) || want_artist.contains(&have_artist) {
            s += 10;
        }
    }
    if want.duration > 0 {
        if let Some(d) = hit.get("duration").and_then(|v| v.as_f64()) {
            if d > 0.0 {
                let diff = (d - want.duration as f64).abs();
                s += (25.0 - diff.min(25.0)) as i32;
            }
        }
    }
    s
}

fn fetch(url: &str) -> Result<Option<Value>, String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(9))
        .user_agent(UA)
        .build()
        .map_err(|e| format!("lyrics client: {e}"))?;
    let res = client
        .get(url)
        .header("Accept", "application/json")
        .send()
        .map_err(|e| format!("lyrics service: {e}"))?;
    let status = res.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !status.is_success() {
        return Err(format!("lyrics service answered {status}"));
    }
    res.json::<Value>()
        .map(Some)
        .map_err(|e| format!("lyrics response: {e}"))
}

fn q(pairs: &[(&str, String)]) -> String {
    pairs
        .iter()
        .filter(|(_, v)| !v.is_empty())
        .map(|(k, v)| format!("{}={}", k, percent(v)))
        .collect::<Vec<_>>()
        .join("&")
}

fn percent(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn pack(hit: &Value, source: &str) -> Value {
    json!({
        "found": true,
        "synced": hit.get("syncedLyrics").and_then(|v| v.as_str()).unwrap_or(""),
        "plain": hit.get("plainLyrics").and_then(|v| v.as_str()).unwrap_or(""),
        "instrumental": hit.get("instrumental").and_then(|v| v.as_bool()).unwrap_or(false),
        "matched": {
            "track": hit.get("trackName").and_then(|v| v.as_str()).unwrap_or(""),
            "artist": hit.get("artistName").and_then(|v| v.as_str()).unwrap_or(""),
            "album": hit.get("albumName").and_then(|v| v.as_str()).unwrap_or(""),
            "duration": hit.get("duration").and_then(|v| v.as_f64()).unwrap_or(0.0),
        },
        "source": source,
    })
}

fn best_of(list: &[Value], want: &Wanted) -> Option<Value> {
    let mut best: Option<(i32, &Value)> = None;
    for hit in list {
        let s = score(hit, want);
        if s == i32::MIN {
            continue;
        }
        // When both lengths are known, refuse a match that is nowhere near the
        // song: a live version or a different track with the same name.
        if want.duration > 0 {
            if let Some(d) = hit.get("duration").and_then(|v| v.as_f64()) {
                if d > 0.0 && (d - want.duration as f64).abs() > 20.0 {
                    continue;
                }
            }
        }
        if best.map(|(bs, _)| s > bs).unwrap_or(true) {
            best = Some((s, hit));
        }
    }
    best.map(|(_, hit)| hit.clone())
}

fn search(url: &str, want: &Wanted, errors: &mut Vec<String>) -> Option<Value> {
    match fetch(url) {
        Ok(Some(Value::Array(list))) => best_of(&list, want),
        Ok(Some(other)) => {
            if other.is_array() {
                None
            } else if has_lyrics(&other) {
                Some(other)
            } else {
                None
            }
        }
        Ok(None) => None,
        Err(e) => {
            errors.push(e);
            None
        }
    }
}

/// Look a song up. Returns `{ "found": false }` when LRCLIB has nothing, and
/// an error only when the service could not be reached at all.
pub fn lookup(want: &Wanted) -> Result<Value, String> {
    let mut errors: Vec<String> = Vec::new();
    let duration = if want.duration > 0 {
        want.duration.to_string()
    } else {
        String::new()
    };

    if !want.track.is_empty() {
        let url = format!(
            "{LRCLIB}/get?{}",
            q(&[
                ("track_name", want.track.clone()),
                ("artist_name", want.artist.clone()),
                ("album_name", want.album.clone()),
                ("duration", duration.clone()),
            ])
        );
        match fetch(&url) {
            Ok(Some(hit)) if has_lyrics(&hit) => return Ok(pack(&hit, "get")),
            Ok(_) => {}
            Err(e) => errors.push(e),
        }
    }

    // "Song (Official Video)" and "Song feat. Someone" still name the record.
    let simple = strip_feat(&strip_brackets(&want.track));

    // "Artist - Song" in the title: use the half after the dash as the song.
    // Uploads often put the artist only in the title; doing this before the
    // plain title search stops a upload titled "Artist - Song (Full Video)"
    // from matching a different upload of the same video.
    if let Some((left, right)) = split_dash(&simple) {
        let artist = if want.artist.is_empty() {
            left
        } else {
            want.artist.clone()
        };
        let url = format!(
            "{LRCLIB}/search?{}",
            q(&[("track_name", right), ("artist_name", artist)])
        );
        if let Some(hit) = search(&url, want, &mut errors) {
            return Ok(pack(&hit, "search-split"));
        }
    }

    if !want.track.is_empty() {
        let url = format!(
            "{LRCLIB}/search?{}",
            q(&[
                ("track_name", want.track.clone()),
                ("artist_name", want.artist.clone()),
                ("album_name", want.album.clone()),
            ])
        );
        if let Some(hit) = search(&url, want, &mut errors) {
            return Ok(pack(&hit, "search"));
        }
    }

    if !simple.is_empty() && simple != want.track {
        let url = format!(
            "{LRCLIB}/search?{}",
            q(&[("track_name", simple.clone()), ("artist_name", want.artist.clone())])
        );
        if let Some(hit) = search(&url, want, &mut errors) {
            return Ok(pack(&hit, "search-clean"));
        }
    }

    let text = format!("{} {}", want.artist, simple).trim().to_string();
    if !text.is_empty() {
        let url = format!("{LRCLIB}/search?{}", q(&[("q", text)]));
        if let Some(hit) = search(&url, want, &mut errors) {
            return Ok(pack(&hit, "search-text"));
        }
    }

    if errors.len() >= 4 {
        return Err(errors.remove(0));
    }
    Ok(json!({ "found": false }))
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Days to wait before asking again about a song LRCLIB did not have. New
/// lyrics are added all the time, but a miss should not cost a lookup on every
/// play either.
const MISS_TTL: u64 = 3 * 86_400;

/// Cached lookup used by the API. A hit is kept forever; a miss is retried
/// after a few days. Pass `refresh` to skip the cache (the panel's "Try again"
/// button) while still storing the new answer. The reply is marked so the UI
/// can say "saved offline".
pub fn lookup_cached(dir: &Path, key: &str, want: &Wanted, refresh: bool) -> Result<Value, String> {
    if !refresh {
        if let Some(mut v) = read_cache(dir, key) {
            let found = v.get("found").and_then(|x| x.as_bool()).unwrap_or(false);
            let at = v.get("at").and_then(|x| x.as_u64()).unwrap_or(0);
            if found || now_secs().saturating_sub(at) < MISS_TTL {
                if let Some(obj) = v.as_object_mut() {
                    obj.insert("cached".into(), json!(true));
                }
                return Ok(v);
            }
        }
    }
    let mut v = lookup(want)?;
    if let Some(obj) = v.as_object_mut() {
        obj.insert("at".into(), json!(now_secs()));
    }
    write_cache(dir, key, &v);
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalises_titles() {
        assert_eq!(norm("Don’t Stop Me Now!"), "don t stop me now");
        assert_eq!(norm("  A.B  "), "a b");
    }

    #[test]
    fn strips_youtube_noise() {
        assert_eq!(strip_brackets("Song (Official Video) [HD]"), "Song");
        assert_eq!(strip_brackets("Song (Live at Wembley)"), "Song");
        assert_eq!(strip_feat("Song feat. Someone"), "Song");
        assert_eq!(strip_feat("Song ft. A & B"), "Song");
    }

    #[test]
    fn splits_artist_and_title() {
        assert_eq!(
            split_dash("Artist - Song"),
            Some(("Artist".into(), "Song".into()))
        );
        assert_eq!(split_dash("Song"), None);
    }

    #[test]
    fn prefers_synced_and_close_length() {
        let want = Wanted {
            track: "Song".into(),
            artist: "Artist".into(),
            album: "".into(),
            duration: 200,
        };
        let synced = json!({"trackName":"Song","artistName":"Artist","duration":201.0,"syncedLyrics":"[00:01.00] hi"});
        let plain = json!({"trackName":"Song","artistName":"Artist","duration":199.0,"plainLyrics":"hi"});
        let wrong = json!({"trackName":"Song","artistName":"Artist","duration":400.0,"syncedLyrics":"[00:01.00] hi"});
        assert!(score(&synced, &want) > score(&plain, &want));
        assert!(score(&wrong, &want) > 0); // scored on names, but rejected by length
        let list = [plain.clone(), synced.clone(), wrong.clone()];
        let hit = best_of(&list, &want).unwrap();
        assert_eq!(hit["duration"], 201.0);
        assert!(hit.get("syncedLyrics").is_some());
    }

    #[test]
    fn caches_hits_forever_and_retries_misses() {
        let dir = std::env::temp_dir().join(format!("wl-lyrics-cached-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let want = Wanted {
            track: "Song".into(),
            artist: "Artist".into(),
            album: String::new(),
            duration: 0,
        };

        // A miss from today is served from disk, marked as cached.
        write_cache(&dir, "k1", &json!({"found": false, "at": now_secs()}));
        let miss = lookup_cached(&dir, "k1", &want, false).unwrap();
        assert_eq!(miss["found"], false);
        assert_eq!(miss["cached"], true);

        // An old miss is ignored (and would be looked up again).
        write_cache(&dir, "k2", &json!({"found": false, "at": 1}));
        assert!(read_cache(&dir, "k2").is_some());
        let stale = read_cache(&dir, "k2").unwrap();
        assert!(now_secs().saturating_sub(stale["at"].as_u64().unwrap()) >= MISS_TTL);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cache_files_are_stable_and_content_round_trips() {
        let dir = std::env::temp_dir().join(format!("wl-lyrics-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let value = json!({"found": true, "synced": "[00:01.00] hi"});
        assert!(read_cache(&dir, "yt:abcdefghijk").is_none());
        write_cache(&dir, "yt:abcdefghijk", &value);
        let back = read_cache(&dir, "yt:abcdefghijk").unwrap();
        assert_eq!(back["synced"], "[00:01.00] hi");
        assert_eq!(
            cache_file(&dir, "yt:abcdefghijk"),
            cache_file(&dir, "yt:abcdefghijk")
        );
        assert!(cache_file(&dir, "yt:abcdefghijk") != cache_file(&dir, "yt:abcdefghijX"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn packs_the_shape_the_ui_expects() {
        let hit = json!({
            "trackName": "Song", "artistName": "Artist", "albumName": "Album",
            "duration": 201.4, "syncedLyrics": "[00:01.00] hi", "plainLyrics": "hi",
            "instrumental": false
        });
        let v = pack(&hit, "search");
        assert_eq!(v["found"], true);
        assert_eq!(v["synced"], "[00:01.00] hi");
        assert_eq!(v["matched"]["track"], "Song");
        assert_eq!(v["matched"]["duration"], 201.4);
        assert_eq!(v["source"], "search");
    }
}
