//! Native lyrics for Wavelength and the Wavelength Link mod.
//!
//! One lookup asks every provider at the same time, then scores what comes back
//! by how well the title, artist and length match, and keeps the best:
//!
//!   LRCLIB          synced + plain, open database, exact and fuzzy search
//!   NetEase Music   synced, large catalogue (also Asian and Latin releases), often with translations
//!   KuGou           synced, large catalogue
//!   lyrics.ovh      plain text, last resort
//!
//! Providers that need an account, an API key or a borrowed app token (Genius,
//! Musixmatch, Spotify) are deliberately not used.
//!
//! Only the song's title, artist, album and length leave the computer, and only
//! when lyrics are asked for. Results are kept on disk, so a song is looked up
//! once. The page (`/api/lyrics`) and the Minecraft mod (`/api/bridge/lyrics`)
//! use the same service, so they always show the same lines.

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::task::JoinSet;

const BROWSER_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Safari/537.36";
const PROVIDER_TIMEOUT: Duration = Duration::from_secs(8);
/// Misses stay on disk this long before the providers are asked again.
const MISS_TTL: Duration = Duration::from_secs(3 * 86_400);
/// A match this good (and synced) ends the search without waiting for slower providers.
const GOOD_ENOUGH: f64 = 0.93;
const MIN_SCORE: f64 = 0.6;

/* ───────────────────────── types ───────────────────────── */

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Line {
    pub t: f64,
    pub text: String,
    /// Translation of this line, when the provider has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tr: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Found {
    pub synced: Vec<Line>,
    pub plain: String,
    pub instrumental: bool,
    /// Which provider the lyrics came from ("" when nothing was found).
    pub source: String,
    /// The title and artist the provider matched, so a wrong match is visible.
    pub title: String,
    pub artist: String,
    /// Providers that answered at all.
    pub tried: Vec<String>,
    pub score: f64,
    /// Length of the record that matched, when a provider supplied one.
    #[serde(default)]
    pub duration: f64,
    /// True when this answer came from the on-disk cache.
    #[serde(default)]
    pub cached: bool,
    /// When this record was written, so stale misses can be retried.
    #[serde(default)]
    pub at: u64,
}

impl Found {
    pub fn is_empty(&self) -> bool {
        self.synced.is_empty() && self.plain.trim().is_empty() && !self.instrumental
    }
}

#[derive(Clone, Debug, Default)]
pub struct Query {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub duration: f64,
}

impl Query {
    /// Cleans what a player shows ("Artist - Song (Official Video) [HD]") into what a lyrics database knows.
    pub fn new(title: &str, artist: &str, album: &str, duration: f64) -> Self {
        let mut artist = clean_artist(artist);
        let mut title = clean_title(title, &artist);
        // No artist known (a plain upload): "Artist - Song" usually says it.
        if artist.is_empty() {
            if let Some((l, r)) = split_dash(&title) {
                if !l.trim().is_empty() && !r.trim().is_empty() {
                    artist = l.trim().to_string();
                    title = r.trim().to_string();
                }
            }
        }
        Self {
            title,
            artist,
            album: album.trim().to_string(),
            duration: if duration.is_finite() && duration > 0.0 { duration } else { 0.0 },
        }
    }

    fn key(&self) -> String {
        format!("{}|{}|{}", fold(&self.title), fold(&self.artist), (self.duration / 5.0).round() as i64)
    }
}

#[derive(Clone, Debug, Default)]
struct Cand {
    source: &'static str,
    title: String,
    artist: String,
    duration: f64,
    synced: Vec<Line>,
    plain: String,
    instrumental: bool,
}

/* ───────────────────────── cleaning and matching ───────────────────────── */

const NOISE: [&str; 17] = [
    "official", "video", "audio", "lyric", "visuali", "remaster", "hd", "hq", "4k", "mv", "clip", "live", "explicit",
    "sub", "version", "music", "m/v",
];

/// Removes (...) and [...] groups that only describe the upload.
fn strip_brackets(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        let open = chars[i];
        let close = match open {
            '(' => Some(')'),
            '[' => Some(']'),
            _ => None,
        };
        if let Some(close) = close {
            if let Some(len) = chars[i + 1..].iter().position(|c| *c == close) {
                let inner: String = chars[i + 1..i + 1 + len].iter().collect::<String>().to_lowercase();
                if NOISE.iter().any(|k| inner.contains(k)) {
                    i += len + 2;
                    continue;
                }
            }
        }
        out.push(open);
        i += 1;
    }
    out
}

/// Drops "feat. X" (bare or in brackets) and everything after it.
fn cut_feat(s: &str) -> String {
    let lower = s.to_ascii_lowercase();
    let mut cut = s.len();
    for m in [
        " ft.", " ft ", " feat.", " feat ", " featuring ", "(feat", "(ft.", "(ft ", "(featuring", "[feat", "[ft.", "[ft ",
    ] {
        if let Some(i) = lower.find(m) {
            cut = cut.min(i);
        }
    }
    s[..cut].to_string()
}

fn split_dash(s: &str) -> Option<(&str, &str)> {
    for d in [" - ", " – ", " — "] {
        if let Some(i) = s.find(d) {
            return Some((&s[..i], &s[i + d.len()..]));
        }
    }
    None
}

fn collapse_spaces(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn clean_artist(artist: &str) -> String {
    let a = artist.split(',').next().unwrap_or("").trim();
    let lower = a.to_ascii_lowercase();
    let a = if lower.ends_with(" - topic") { &a[..a.len() - 8] } else { a };
    let lower = a.to_ascii_lowercase();
    let a = if lower.ends_with("vevo") && a.len() > 4 { &a[..a.len() - 4] } else { a };
    collapse_spaces(a)
}

pub fn clean_title(title: &str, artist: &str) -> String {
    let mut t = collapse_spaces(&cut_feat(&strip_brackets(title.trim())));
    if let Some(i) = t.find(" | ") {
        t.truncate(i);
    }
    if let Some((l, r)) = split_dash(&t) {
        let a = fold(&clean_artist(artist));
        let lf = fold(l);
        let rl = r.to_lowercase();
        let tail = ["remaster", "live", "version", "edit", "mono", "stereo", "bonus", "single", "mix"];
        if !a.is_empty() && !lf.is_empty() && (a.contains(&lf) || lf.contains(&a)) {
            t = r.trim().to_string(); // "Artist - Song" -> "Song"
        } else if tail.iter().any(|k| rl.contains(k)) {
            t = l.trim().to_string(); // "Song - Remastered 2011" -> "Song"
        }
    }
    let t = collapse_spaces(&t);
    if t.is_empty() { title.trim().to_string() } else { t }
}

/// Lower-case letters and digits (any script) separated by single spaces.
fn fold(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
        } else if !out.ends_with(' ') && !out.is_empty() {
            out.push(' ');
        }
    }
    out.trim().to_string()
}

fn bigrams(s: &str) -> HashMap<(char, char), i32> {
    let cs: Vec<char> = s.chars().collect();
    let mut m = HashMap::new();
    for w in cs.windows(2) {
        *m.entry((w[0], w[1])).or_insert(0) += 1;
    }
    m
}

/// Sørensen–Dice on character pairs. Works for Latin and CJK alike.
fn dice(a: &str, b: &str) -> f64 {
    if a == b {
        return if a.is_empty() { 0.0 } else { 1.0 };
    }
    let (x, y) = (bigrams(a), bigrams(b));
    let (nx, ny): (i32, i32) = (x.values().sum(), y.values().sum());
    if nx == 0 || ny == 0 {
        return 0.0;
    }
    let shared: i32 = x.iter().map(|(k, v)| (*v).min(*y.get(k).unwrap_or(&0))).sum();
    2.0 * shared as f64 / (nx + ny) as f64
}

fn title_sim(q: &str, c: &str) -> f64 {
    let (a, b) = (fold(q), fold(&clean_title(c, "")));
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    if a == b {
        return 1.0;
    }
    let (short, long) = if a.chars().count() <= b.chars().count() { (&a, &b) } else { (&b, &a) };
    if short.chars().count() >= 3 && long.contains(short.as_str()) {
        return dice(&a, &b).max(0.88);
    }
    dice(&a, &b)
}

fn artist_parts(s: &str) -> Vec<String> {
    let lower = s.to_lowercase();
    lower
        .split(|c| matches!(c, ',' | '&' | ';' | '/' | '、' | '×'))
        .flat_map(|p| p.split(" feat. ").map(str::to_string).collect::<Vec<_>>())
        .map(|p| fold(&p))
        .filter(|p| !p.is_empty())
        .collect()
}

/// Channel names are unreliable ("Topic", labels), so a missing match costs little.
fn artist_sim(q: &str, c: &str) -> f64 {
    if q.trim().is_empty() || c.trim().is_empty() {
        return 0.5;
    }
    let (mut best, qa, ca) = (0.0f64, artist_parts(q), artist_parts(c));
    let whole = (fold(q), fold(c));
    best = best.max(dice(&whole.0, &whole.1));
    for x in &qa {
        for y in &ca {
            let s = if x == y || (x.len() >= 3 && (x.contains(y.as_str()) || y.contains(x.as_str()))) { 1.0 } else { dice(x, y) };
            best = best.max(s);
        }
    }
    best
}

fn dur_part(diff: f64) -> f64 {
    match diff {
        d if d <= 1.5 => 0.20,
        d if d <= 3.0 => 0.17,
        d if d <= 6.0 => 0.10,
        d if d <= 10.0 => 0.03,
        d if d <= 20.0 => -0.05,
        _ => -0.25,
    }
}

/// None when the candidate is not this song.
fn score(q: &Query, c: &Cand) -> Option<f64> {
    if c.synced.is_empty() && c.plain.trim().is_empty() && !c.instrumental {
        return None;
    }
    let ts = title_sim(&q.title, &c.title);
    if ts < 0.6 {
        return None;
    }
    let mut s = 0.5 * ts + 0.25 * artist_sim(&q.artist, &c.artist);
    s += if q.duration > 0.0 && c.duration > 0.0 { dur_part((q.duration - c.duration).abs()) } else { 0.05 };
    if !c.synced.is_empty() {
        s += 0.08;
        // Timestamps running far past the song mean a different cut of it.
        let last = c.synced.last().map_or(0.0, |l| l.t);
        if q.duration > 0.0 && last > q.duration + 25.0 {
            s -= 0.3;
        }
    }
    if c.source == "lyrics.ovh" {
        s -= 0.05; // no metadata to verify against
    }
    if s >= MIN_SCORE { Some(s) } else { None }
}

fn choose(q: &Query, cands: &[Cand]) -> Option<Found> {
    let mut best: Option<(f64, &Cand)> = None;
    for c in cands {
        if let Some(s) = score(q, c) {
            if best.map_or(true, |(b, _)| s > b) {
                best = Some((s, c));
            }
        }
    }
    let (s, c) = best?;
    let plain = if c.plain.trim().is_empty() {
        c.synced.iter().map(|l| l.text.as_str()).collect::<Vec<_>>().join("\n")
    } else {
        c.plain.trim().to_string()
    };
    Some(Found {
        synced: c.synced.clone(),
        plain,
        instrumental: c.instrumental && c.synced.is_empty(),
        source: c.source.to_string(),
        title: c.title.clone(),
        artist: c.artist.clone(),
        tried: Vec::new(),
        score: (s * 1000.0).round() / 1000.0,
        duration: c.duration,
        cached: false,
        at: now_secs(),
    })
}

/* ───────────────────────── LRC ───────────────────────── */

/// "mm:ss", "mm:ss.xx", "mm:ss:xx" or "h:mm:ss.xx" -> seconds. Metadata tags like "ar:Name" are not stamps.
fn stamp(tag: &str) -> Option<f64> {
    let parts: Vec<&str> = tag.split(':').collect();
    if !(2..=3).contains(&parts.len()) {
        return None;
    }
    let digits = |p: &str| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit());
    if parts.len() == 3 && digits(parts[2]) && digits(parts[1]) && digits(parts[0]) && parts[2].len() <= 3 && parts[1].len() == 2 {
        // mm:ss:xx (fraction after a second colon)
        let frac = parts[2].parse::<f64>().ok()? / 10f64.powi(parts[2].len() as i32);
        return Some(parts[0].parse::<f64>().ok()? * 60.0 + parts[1].parse::<f64>().ok()? + frac);
    }
    let sec = parts[parts.len() - 1];
    let (whole, frac) = sec.split_once('.').unwrap_or((sec, ""));
    if !digits(whole) || (!frac.is_empty() && !digits(frac)) {
        return None;
    }
    let mut total = whole.parse::<f64>().ok()?;
    if !frac.is_empty() {
        total += frac.parse::<f64>().ok()? / 10f64.powi(frac.len() as i32);
    }
    let mut mult = 60.0;
    for p in parts[..parts.len() - 1].iter().rev() {
        if !digits(p) {
            return None;
        }
        total += p.parse::<f64>().ok()? * mult;
        mult *= 60.0;
    }
    Some(total)
}

/// Removes enhanced-LRC word stamps such as <00:12.40>.
fn strip_word_stamps(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find('<') {
        match rest[i..].find('>') {
            Some(j) if stamp(&rest[i + 1..i + j]).is_some() => {
                out.push_str(&rest[..i]);
                rest = &rest[i + j + 1..];
            }
            _ => {
                out.push_str(&rest[..i + 1]);
                rest = &rest[i + 1..];
            }
        }
    }
    out.push_str(rest);
    out
}

pub fn parse_lrc(src: &str) -> Vec<Line> {
    let mut out: Vec<Line> = Vec::new();
    let mut offset = 0.0;
    for raw in src.lines() {
        let mut rest = raw.trim_start_matches('\u{feff}').trim();
        let mut stamps = Vec::new();
        while rest.starts_with('[') {
            let Some(end) = rest.find(']') else { break };
            let tag = &rest[1..end];
            if let Some(t) = stamp(tag) {
                stamps.push(t);
            } else if let Some(v) = tag.strip_prefix("offset:") {
                offset = v.trim().parse::<f64>().unwrap_or(0.0) / 1000.0;
            }
            rest = rest[end + 1..].trim_start();
        }
        if stamps.is_empty() {
            continue;
        }
        let text = collapse_spaces(&strip_word_stamps(rest));
        for t in stamps {
            out.push(Line { t, text: text.clone(), tr: None });
        }
    }
    // A positive [offset:] makes lyrics appear that much earlier.
    for l in &mut out {
        l.t = (l.t - offset).max(0.0);
    }
    out.sort_by(|a, b| a.t.partial_cmp(&b.t).unwrap_or(std::cmp::Ordering::Equal));
    out.dedup_by(|b, a| a.text.is_empty() && b.text.is_empty());
    while out.last().map_or(false, |l| l.text.is_empty()) {
        out.pop();
    }
    out
}

/// Credit lines some catalogues put at the very start ("作词 : ...", "Composed by ...").
fn drop_credits(lines: Vec<Line>) -> Vec<Line> {
    const PREFIX: [&str; 12] = ["作词", "作曲", "编曲", "制作", "混音", "母带", "录音", "监制", "词：", "曲：", "lyrics by", "composed by"];
    lines
        .into_iter()
        .filter(|l| {
            let low = l.text.to_lowercase();
            !(l.t < 30.0 && PREFIX.iter().any(|p| low.starts_with(p)))
        })
        .collect()
}

fn merge_translation(synced: &mut [Line], tl: &[Line]) {
    for l in synced.iter_mut() {
        if l.text.is_empty() {
            continue;
        }
        if let Some(t) = tl.iter().find(|t| (t.t - l.t).abs() < 0.3 && !t.text.is_empty()) {
            l.tr = Some(t.text.clone());
        }
    }
}

/* ───────────────────────── providers ───────────────────────── */

fn s(v: &Value, k: &str) -> String {
    v.get(k).and_then(Value::as_str).unwrap_or("").to_string()
}

fn f(v: &Value, k: &str) -> f64 {
    v.get(k).and_then(Value::as_f64).unwrap_or(0.0)
}

async fn get_json(c: &Client, url: &str, query: &[(&str, &str)], headers: &[(&str, &str)]) -> Option<Value> {
    let mut rb = c.get(url).query(query);
    for (k, v) in headers {
        rb = rb.header(*k, *v);
    }
    let r = rb.send().await.ok()?;
    if !r.status().is_success() {
        return None;
    }
    r.json::<Value>().await.ok()
}

fn lrclib_item(v: &Value) -> Option<Cand> {
    let c = Cand {
        source: "LRCLIB",
        title: s(v, "trackName"),
        artist: s(v, "artistName"),
        duration: f(v, "duration"),
        synced: parse_lrc(&s(v, "syncedLyrics")),
        plain: s(v, "plainLyrics"),
        instrumental: v.get("instrumental").and_then(Value::as_bool).unwrap_or(false),
    };
    if c.synced.is_empty() && c.plain.trim().is_empty() && !c.instrumental { None } else { Some(c) }
}

async fn lrclib(c: Client, q: Query) -> Vec<Cand> {
    let mut out = Vec::new();
    let dur = (q.duration.round() as i64).to_string();
    if !q.artist.is_empty() {
        let mut qs = vec![("track_name", q.title.as_str()), ("artist_name", q.artist.as_str())];
        if !q.album.is_empty() {
            qs.push(("album_name", q.album.as_str()));
        }
        if q.duration > 0.0 {
            qs.push(("duration", dur.as_str()));
        }
        if let Some(v) = get_json(&c, "https://lrclib.net/api/get", &qs, &[]).await {
            out.extend(lrclib_item(&v));
        }
    }
    let mut qs = vec![("track_name", q.title.as_str())];
    if !q.artist.is_empty() {
        qs.push(("artist_name", q.artist.as_str()));
    }
    let mut list = get_json(&c, "https://lrclib.net/api/search", &qs, &[]).await;
    if list.as_ref().and_then(Value::as_array).map_or(true, |a| a.is_empty()) {
        let both = format!("{} {}", q.artist, q.title);
        list = get_json(&c, "https://lrclib.net/api/search", &[("q", both.trim())], &[]).await;
    }
    if let Some(arr) = list.as_ref().and_then(Value::as_array) {
        out.extend(arr.iter().take(8).filter_map(lrclib_item));
    }
    out
}

const NETEASE_HEADERS: [(&str, &str); 2] = [
    ("Referer", "https://music.163.com/"),
    ("User-Agent", BROWSER_UA),
];

async fn netease(c: Client, q: Query) -> Vec<Cand> {
    let mut out = Vec::new();
    let term = format!("{} {}", q.title, q.artist);
    let mut rb = c
        .post("https://music.163.com/api/search/get/web")
        .form(&[("s", term.trim()), ("type", "1"), ("offset", "0"), ("limit", "6")]);
    for (k, v) in NETEASE_HEADERS {
        rb = rb.header(k, v);
    }
    let Some(res) = async { rb.send().await.ok()?.json::<Value>().await.ok() }.await else {
        return out;
    };
    let songs = res.pointer("/result/songs").and_then(Value::as_array).cloned().unwrap_or_default();
    for song in songs.iter().take(4) {
        let Some(id) = song.get("id").and_then(Value::as_i64) else { continue };
        let artist = song
            .get("artists")
            .and_then(Value::as_array)
            .map(|a| a.iter().map(|x| s(x, "name")).collect::<Vec<_>>().join(", "))
            .unwrap_or_default();
        let dur_ms = f(song, "duration").max(f(song, "dt"));
        let id_s = id.to_string();
        let Some(l) = get_json(
            &c,
            "https://music.163.com/api/song/lyric",
            &[("id", id_s.as_str()), ("lv", "1"), ("kv", "1"), ("tv", "-1")],
            &NETEASE_HEADERS,
        )
        .await
        else {
            continue;
        };
        if l.get("uncollected").and_then(Value::as_bool).unwrap_or(false) {
            continue;
        }
        let mut cand = Cand { source: "NetEase", title: s(song, "name"), artist, duration: dur_ms / 1000.0, ..Cand::default() };
        if l.get("nolyric").and_then(Value::as_bool).unwrap_or(false) {
            cand.instrumental = true;
            out.push(cand);
            continue;
        }
        let lrc = l.pointer("/lrc/lyric").and_then(Value::as_str).unwrap_or("");
        let mut synced = drop_credits(parse_lrc(lrc));
        if synced.len() <= 3 && synced.iter().any(|x| x.text.contains("纯音乐")) {
            cand.instrumental = true;
            out.push(cand);
            continue;
        }
        if let Some(tl) = l.pointer("/tlyric/lyric").and_then(Value::as_str) {
            merge_translation(&mut synced, &parse_lrc(tl));
        }
        cand.synced = synced;
        out.push(cand);
    }
    out
}

async fn kugou(c: Client, q: Query) -> Vec<Cand> {
    let mut out = Vec::new();
    let keyword = format!("{} - {}", q.artist, q.title);
    let ms = ((q.duration * 1000.0).round() as i64).to_string();
    let mut qs = vec![("ver", "1"), ("man", "yes"), ("client", "pc"), ("keyword", keyword.trim_start_matches(" - "))];
    if q.duration > 0.0 {
        qs.push(("duration", ms.as_str()));
    }
    let Some(res) = get_json(&c, "https://lyrics.kugou.com/search", &qs, &[("User-Agent", BROWSER_UA)]).await else {
        return out;
    };
    let cands = res.get("candidates").and_then(Value::as_array).cloned().unwrap_or_default();
    for cd in cands.iter().take(3) {
        let (id, key) = (cd.get("id").map(|v| v.to_string().trim_matches('"').to_string()).unwrap_or_default(), s(cd, "accesskey"));
        if id.is_empty() || key.is_empty() {
            continue;
        }
        let Some(d) = get_json(
            &c,
            "https://lyrics.kugou.com/download",
            &[("ver", "1"), ("client", "pc"), ("id", id.as_str()), ("accesskey", key.as_str()), ("fmt", "lrc"), ("charset", "utf8")],
            &[("User-Agent", BROWSER_UA)],
        )
        .await
        else {
            continue;
        };
        let Ok(bytes) = B64.decode(s(&d, "content").trim()) else { continue };
        let synced = drop_credits(parse_lrc(&String::from_utf8_lossy(&bytes)));
        if synced.is_empty() {
            continue;
        }
        out.push(Cand { source: "KuGou", title: s(cd, "song"), artist: s(cd, "singer"), duration: f(cd, "duration") / 1000.0, synced, ..Cand::default() });
    }
    out
}

fn path_segment(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

async fn lyrics_ovh(c: Client, q: Query) -> Vec<Cand> {
    if q.artist.is_empty() || q.title.is_empty() {
        return Vec::new();
    }
    let url = format!("https://api.lyrics.ovh/v1/{}/{}", path_segment(&q.artist), path_segment(&q.title));
    let Some(v) = get_json(&c, &url, &[], &[]).await else { return Vec::new() };
    let raw = s(&v, "lyrics");
    let mut lines: Vec<&str> = raw.lines().collect();
    if lines.first().map_or(false, |l| l.starts_with("Paroles de la chanson")) {
        lines.remove(0);
    }
    let plain = lines.join("\n").trim().to_string();
    if plain.is_empty() {
        return Vec::new();
    }
    vec![Cand { source: "lyrics.ovh", title: q.title.clone(), artist: q.artist.clone(), plain, ..Cand::default() }]
}

fn start<F, Fut>(set: &mut JoinSet<Vec<Cand>>, c: &Client, q: &Query, provider: F)
where
    F: FnOnce(Client, Query) -> Fut + Send + 'static,
    Fut: Future<Output = Vec<Cand>> + Send + 'static,
{
    let (c, q) = (c.clone(), q.clone());
    set.spawn(async move { tokio::time::timeout(PROVIDER_TIMEOUT, provider(c, q)).await.unwrap_or_default() });
}

/* ───────────────────────── service ───────────────────────── */

pub struct Lyrics {
    client: Client,
    dir: PathBuf,
    misses: Mutex<HashMap<String, (Instant, Found)>>,
}

fn fnv(s: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn write_cache_file(path: &std::path::Path, found: &Found) {
    if std::fs::create_dir_all(path.parent().unwrap_or_else(|| std::path::Path::new("."))).is_err() {
        return;
    }
    let Ok(text) = serde_json::to_string(found) else { return };
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    if std::fs::write(&tmp, text).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

impl Lyrics {
    pub fn new(dir: PathBuf) -> Self {
        let client = Client::builder()
            .user_agent("Wavelength (https://github.com/thegoldertrone7-cyber/wavelength-tauri)")
            .timeout(PROVIDER_TIMEOUT)
            .build()
            .unwrap_or_else(|_| Client::new());
        Self { client, dir, misses: Mutex::new(HashMap::new()) }
    }

    fn file(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{}.json", fnv(key)))
    }

    /// Keeps lyrics found some other way (the on-device generator) in the same cache the databases use.
    pub fn store(&self, key: &str, found: &Found) {
        if found.is_empty() || std::fs::create_dir_all(&self.dir).is_err() {
            return;
        }
        let mut record = found.clone();
        record.at = now_secs();
        write_cache_file(&self.file(key), &record);
        self.misses.lock().unwrap_or_else(|e| e.into_inner()).remove(key);
    }

    /// Forgets what is kept for a song, so the next lookup asks every provider again.
    pub fn forget(&self, key: &str) {
        let _ = std::fs::remove_file(self.file(key));
        self.misses.lock().unwrap_or_else(|e| e.into_inner()).remove(key);
    }

    pub async fn find(&self, title: &str, artist: &str, album: &str, duration: f64) -> Found {
        let q = Query::new(title, artist, album, duration);
        if q.title.is_empty() {
            return Found::default();
        }
        let key = q.key();
        if let Ok(text) = std::fs::read_to_string(self.file(&key)) {
            if let Ok(mut found) = serde_json::from_str::<Found>(&text) {
                let fresh = found.at == 0 || now_secs().saturating_sub(found.at) < MISS_TTL.as_secs();
                if !found.is_empty() || fresh {
                    found.cached = true;
                    return found;
                }
            }
        }
        if let Some((_, found)) = self.misses.lock().unwrap_or_else(|e| e.into_inner()).get(&key) {
            if found.at == 0 || now_secs().saturating_sub(found.at) < MISS_TTL.as_secs() {
                let mut hit = found.clone();
                hit.cached = true;
                return hit;
            }
        }

        let mut set = JoinSet::new();
        start(&mut set, &self.client, &q, lrclib);
        start(&mut set, &self.client, &q, netease);
        start(&mut set, &self.client, &q, kugou);
        start(&mut set, &self.client, &q, lyrics_ovh);
        let (mut all, mut tried): (Vec<Cand>, Vec<String>) = (Vec::new(), Vec::new());
        while let Some(done) = set.join_next().await {
            let Ok(list) = done else { continue };
            for c in &list {
                if !tried.iter().any(|t| t == c.source) {
                    tried.push(c.source.to_string());
                }
            }
            all.extend(list);
            if let Some(best) = choose(&q, &all) {
                if best.score >= GOOD_ENOUGH && !best.synced.is_empty() {
                    break; // dropping the set stops the slower providers
                }
            }
        }
        drop(set);

        let mut found = choose(&q, &all).unwrap_or_default();
        found.tried = tried;
        found.at = now_secs();
        write_cache_file(&self.file(&key), &found);
        if found.is_empty() {
            let mut m = self.misses.lock().unwrap_or_else(|e| e.into_inner());
            if m.len() > 200 {
                m.clear();
            }
            m.insert(key, (Instant::now(), found.clone()));
        }
        found
    }
}

#[derive(Deserialize)]
struct LyrQuery {
    title: Option<String>,
    artist: Option<String>,
    album: Option<String>,
    duration: Option<f64>,
    /// The page's track id ("yt:ID", "local:ID"): what the generator listens to when no database has the song.
    id: Option<String>,
    /// "1" asks for generation now, even if the setting is off (the Generate lyrics button).
    generate: Option<String>,
    /// "1" forgets what is kept for this song first (the Search again button).
    refresh: Option<String>,
}

pub fn routes() -> axum::Router<std::sync::Arc<crate::api::AppState>> {
    axum::Router::new()
        .route("/api/lyrics", axum::routing::get(lyrics_route))
        .route("/api/lyrics/generator", axum::routing::get(generator_get).post(generator_set))
        .route("/api/lyrics/generator/model", axum::routing::post(generator_model))
}

/// What a lookup came to: the lyrics, or a generator job that is still working on them.
pub struct Outcome {
    pub found: Found,
    /// A generator job for this song (running, done or failed), if one exists.
    pub job: Option<crate::generator::JobStatus>,
    /// Whether this build and this song allow generating lyrics at all.
    pub can_generate: bool,
}

impl Outcome {
    pub fn pending(&self) -> bool {
        self.found.is_empty() && self.job.as_ref().map_or(false, |j| !j.finished())
    }
}

fn generated_found(q: &Query, lines: Vec<Line>, source: String) -> Found {
    Found {
        plain: lines.iter().map(|l| l.text.as_str()).collect::<Vec<_>>().join("\n"),
        synced: lines,
        instrumental: false,
        source,
        title: q.title.clone(),
        artist: q.artist.clone(),
        tried: Vec::new(),
        score: 1.0,
        duration: q.duration,
        cached: false,
        at: now_secs(),
    }
}

/// The one lookup the page and the mod share: every database first, then (when allowed) the generator.
/// `generate` is a request for generation right now; the Settings toggle (`Generator::enabled`) allows it automatically.
pub async fn lookup(
    st: &std::sync::Arc<crate::api::AppState>,
    title: &str,
    artist: &str,
    album: &str,
    duration: f64,
    track_id: &str,
    generate: bool,
) -> Outcome {
    let found = st.lyrics.find(title, artist, album, duration).await;
    let q = Query::new(title, artist, album, duration);
    if !found.is_empty() || q.title.is_empty() {
        return Outcome { found, job: None, can_generate: false };
    }
    let key = q.key();
    let source = crate::generator::Source::from_track_id(track_id);
    let can_generate = crate::generator::Generator::compiled() && source.is_some();
    // A job that already exists is always reported, so a poll that does not ask for generation still sees it finish.
    let mut job = st.generator.status(&key);
    if job.is_none() && can_generate && (generate || st.generator.enabled()) {
        if let Some(source) = source {
            let cached = match &source {
                crate::generator::Source::Youtube(id) => st.audio.cached(&crate::audio::track_id(&format!("/api/stream/{id}"))),
                crate::generator::Source::Local(_) => None,
            };
            let spec = crate::generator::Spec {
                key: key.clone(),
                source,
                title: q.title.clone(),
                artist: q.artist.clone(),
                duration: q.duration,
                cached,
                port: crate::PORT,
            };
            let (st2, q2, key2) = (st.clone(), q.clone(), key.clone());
            job = st.generator.ensure(
                spec,
                Box::new(move |lines, src| st2.lyrics.store(&key2, &generated_found(&q2, lines, src))),
            );
        }
    }
    Outcome { found, job, can_generate }
}

/// GET /api/lyrics?title=&artist=&album=&duration=&id=&generate=
async fn lyrics_route(
    axum::extract::State(st): axum::extract::State<std::sync::Arc<crate::api::AppState>>,
    axum::extract::Query(q): axum::extract::Query<LyrQuery>,
) -> axum::Json<Value> {
    let (title, artist, album) = (q.title.as_deref().unwrap_or(""), q.artist.as_deref().unwrap_or(""), q.album.as_deref().unwrap_or(""));
    let duration = q.duration.unwrap_or(0.0);
    if q.refresh.as_deref() == Some("1") {
        st.lyrics.forget(&Query::new(title, artist, album, duration).key());
    }
    let out = lookup(&st, title, artist, album, duration, q.id.as_deref().unwrap_or(""), q.generate.as_deref() == Some("1")).await;
    axum::Json(outcome_json(&out))
}

/// GET /api/lyrics/generator: what Settings shows.
async fn generator_get(axum::extract::State(st): axum::extract::State<std::sync::Arc<crate::api::AppState>>) -> axum::Json<Value> {
    axum::Json(st.generator.describe())
}

#[derive(Deserialize)]
struct GeneratorSet {
    generate: Option<bool>,
    model: Option<String>,
}

/// POST /api/lyrics/generator {generate?, model?}. The core owns the setting, so the mod follows it.
async fn generator_set(
    axum::extract::State(st): axum::extract::State<std::sync::Arc<crate::api::AppState>>,
    headers: axum::http::HeaderMap,
    axum::Json(b): axum::Json<GeneratorSet>,
) -> Result<axum::Json<Value>, axum::http::StatusCode> {
    if crate::bridge::guard(&headers, true).is_err() {
        return Err(axum::http::StatusCode::FORBIDDEN);
    }
    st.generator.configure(b.generate, b.model.as_deref());
    Ok(axum::Json(st.generator.describe()))
}

#[derive(Deserialize)]
struct ModelBody {
    id: Option<String>,
}

/// POST /api/lyrics/generator/model {id}: download a speech model now, so the first song does not wait for it.
async fn generator_model(
    axum::extract::State(st): axum::extract::State<std::sync::Arc<crate::api::AppState>>,
    headers: axum::http::HeaderMap,
    axum::Json(b): axum::Json<ModelBody>,
) -> Result<axum::Json<Value>, axum::http::StatusCode> {
    if crate::bridge::guard(&headers, true).is_err() {
        return Err(axum::http::StatusCode::FORBIDDEN);
    }
    let id = b.id.unwrap_or_else(|| st.generator.model_id());
    st.generator.prefetch_model(&id);
    Ok(axum::Json(st.generator.describe()))
}

/// What both the page and the mod receive for lyrics that were found.
pub fn to_json(f: &Found) -> Value {
    serde_json::json!({
        "ok": true,
        "synced": f.synced,
        "plain": f.plain,
        "instrumental": f.instrumental,
        "source": f.source,
        "found": !f.is_empty(),
        "cached": f.cached,
        "matched": { "track": f.title, "artist": f.artist, "duration": f.duration },
        "tried": f.tried,
    })
}

/// `to_json` plus the generator's state: `pending` while a job runs, `canGenerate` for the page's button.
pub fn outcome_json(o: &Outcome) -> Value {
    let mut v = to_json(&o.found);
    let (stage, progress, label, error) = match &o.job {
        Some(j) => (j.stage.clone(), j.overall(), j.label(), j.error.clone()),
        None => (String::new(), 0.0, "", None),
    };
    if let Some(m) = v.as_object_mut() {
        m.insert("pending".into(), Value::Bool(o.pending()));
        m.insert("stage".into(), Value::String(if o.found.is_empty() { stage } else { String::new() }));
        m.insert("progress".into(), serde_json::json!(progress));
        m.insert("label".into(), Value::String(label.into()));
        m.insert("generated".into(), Value::Bool(o.found.source.starts_with("Generated")));
        m.insert("canGenerate".into(), Value::Bool(o.can_generate));
        m.insert("generateError".into(), error.map_or(Value::Null, Value::String));
    }
    v
}

/// Distinct sources, for tests and diagnostics.
#[allow(dead_code)]
fn distinct(c: &[Cand]) -> HashSet<&'static str> {
    c.iter().map(|x| x.source).collect()
}

/* ───────────────────────── tests ───────────────────────── */

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(source: &'static str, title: &str, artist: &str, dur: f64, synced: &str, plain: &str) -> Cand {
        Cand { source, title: title.into(), artist: artist.into(), duration: dur, synced: parse_lrc(synced), plain: plain.into(), instrumental: false }
    }

    #[test]
    fn lrc_stamps_offsets_and_junk() {
        let l = parse_lrc("\u{feff}[ar:Someone]\n[ti:Song]\n[offset:+500]\n[00:10.50]Hello\n[01:02.5][00:30]Twice <00:31.00>word\nnot a line\n[00:40]\n[00:41]\n[1:02:03.25]Long");
        let t: Vec<(f64, &str)> = l.iter().map(|x| (x.t, x.text.as_str())).collect();
        assert_eq!(t[0], (10.0, "Hello"));
        assert_eq!(t[1], (29.5, "Twice word"));
        assert!(t.contains(&(62.0, "Twice word")));
        assert_eq!(l.iter().filter(|x| x.text.is_empty()).count(), 1, "blank lines collapse");
        assert!(l.last().unwrap().t > 3700.0, "hour stamps work: {:?}", l.last());
        assert!(l.windows(2).all(|w| w[0].t <= w[1].t), "sorted");
    }

    #[test]
    fn stamps() {
        assert_eq!(stamp("01:02"), Some(62.0));
        assert_eq!(stamp("00:10.50"), Some(10.5));
        assert_eq!(stamp("00:10:50"), Some(10.5));
        assert_eq!(stamp("ar:Name"), None);
        assert_eq!(stamp("by:me"), None);
        assert_eq!(stamp("length: 3:21"), None);
    }

    #[test]
    fn titles_are_cleaned_for_lookup() {
        assert_eq!(clean_title("Artist - Song (Official Video) [HD]", "Artist"), "Song");
        assert_eq!(clean_title("Song (feat. Someone Else)", "Artist"), "Song");
        assert_eq!(clean_title("Song - Remastered 2011", "Artist"), "Song");
        assert_eq!(clean_title("Song (Acoustic)", "Artist"), "Song (Acoustic)", "real subtitles stay");
        assert_eq!(clean_title("Song | Official Audio", ""), "Song");
        assert_eq!(clean_artist("Artist - Topic"), "Artist");
        assert_eq!(clean_artist("ArtistVEVO"), "Artist");
        assert_eq!(clean_artist("A, B"), "A");
        let q = Query::new("Queen - Bohemian Rhapsody (Official Video)", "", "", 354.0);
        assert_eq!((q.artist.as_str(), q.title.as_str()), ("Queen", "Bohemian Rhapsody"));
    }

    #[test]
    fn similarity() {
        assert!(title_sim("Bohemian Rhapsody", "Bohemian Rhapsody - Remastered 2011") > 0.95);
        assert!(title_sim("Bohemian Rhapsody", "Another One Bites the Dust") < 0.5);
        assert!(title_sim("夜曲", "夜曲") == 1.0);
        assert!(artist_sim("Simon & Garfunkel", "Simon & Garfunkel") > 0.99);
        assert!(artist_sim("Drake", "Drake, Rihanna") > 0.99, "a featured artist does not hurt");
        assert_eq!(artist_sim("", "Anyone"), 0.5);
    }

    #[test]
    fn prefers_the_synced_version_of_the_right_length() {
        let q = Query::new("Song", "Band", "", 200.0);
        let best = choose(&q, &[
            cand("LRCLIB", "Song", "Band", 200.0, "", "plain words"),
            cand("NetEase", "Song", "Band", 201.0, "[00:01]a\n[00:05]b", ""),
            cand("KuGou", "Song", "Band", 260.0, "[00:01]x\n[00:05]y", ""),
        ])
        .unwrap();
        assert_eq!(best.source, "NetEase");
        assert_eq!(best.plain, "a\nb", "plain text is derived from the synced lines");
    }

    #[test]
    fn rejects_other_songs_and_wrong_cuts() {
        let q = Query::new("Song", "Band", "", 200.0);
        assert!(choose(&q, &[cand("LRCLIB", "Totally Different", "Other", 200.0, "[00:01]a", "")]).is_none());
        // Timestamps that run far past the song are a different cut: lose to a plain match of the right one.
        let best = choose(&q, &[
            cand("KuGou", "Song", "Band", 0.0, "[00:01]a\n[05:30]b", ""),
            cand("LRCLIB", "Song", "Band", 200.0, "", "right plain"),
        ])
        .unwrap();
        assert_eq!(best.source, "LRCLIB");
    }

    #[test]
    fn unknown_duration_still_matches() {
        let q = Query::new("Song", "Band", "", 0.0);
        assert!(choose(&q, &[cand("LRCLIB", "Song", "Band", 200.0, "[00:01]a", "")]).is_some());
    }

    #[test]
    fn instrumental_is_reported() {
        let q = Query::new("Song", "Band", "", 100.0);
        let mut c = cand("NetEase", "Song", "Band", 100.0, "", "");
        c.instrumental = true;
        let f = choose(&q, &[c]).unwrap();
        assert!(f.instrumental && f.synced.is_empty());
    }

    #[test]
    fn netease_extras() {
        let lines = drop_credits(parse_lrc("[00:00.00]作词 : A\n[00:01.00]作曲 : B\n[00:12.00]real line"));
        assert_eq!(lines.len(), 1);
        let mut synced = parse_lrc("[00:12.00]hello\n[00:15.00]world");
        merge_translation(&mut synced, &parse_lrc("[00:12.00]你好\n[00:15.10]世界"));
        assert_eq!(synced[0].tr.as_deref(), Some("你好"));
        assert_eq!(synced[1].tr.as_deref(), Some("世界"));
    }

    #[test]
    fn path_segments_are_escaped() {
        assert_eq!(path_segment("AC/DC"), "AC%2FDC");
        assert_eq!(path_segment("a b"), "a%20b");
        assert_eq!(path_segment("夜"), "%E5%A4%9C");
    }

    #[test]
    fn json_shape_for_the_page_and_the_mod() {
        let f = Found { synced: parse_lrc("[00:01]a"), source: "LRCLIB".into(), ..Found::default() };
        let v = to_json(&f);
        assert_eq!(v["synced"][0]["t"], serde_json::json!(1.0));
        assert!(v["synced"][0].get("tr").is_none());
        assert_eq!(distinct(&[cand("LRCLIB", "a", "b", 1.0, "", "x")]).len(), 1);
    }

    #[test]
    fn pending_is_reported_only_while_nothing_was_found() {
        let running = crate::generator::JobStatus { stage: "listen".into(), progress: 0.5, error: None };
        let o = Outcome { found: Found::default(), job: Some(running.clone()), can_generate: true };
        let v = outcome_json(&o);
        assert_eq!(v["pending"], serde_json::json!(true));
        assert_eq!(v["stage"], serde_json::json!("listen"));
        assert!(v["progress"].as_f64().unwrap() > 0.4 && v["progress"].as_f64().unwrap() < 1.0);
        assert_eq!(v["canGenerate"], serde_json::json!(true));
        // a finished lookup is never pending, whatever the job says
        let f = Found { synced: parse_lrc("[00:01]a"), source: "Generated on this device (base)".into(), ..Found::default() };
        let v = outcome_json(&Outcome { found: f, job: Some(running), can_generate: true });
        assert_eq!(v["pending"], serde_json::json!(false));
        assert_eq!(v["generated"], serde_json::json!(true));
        assert_eq!(outcome_json(&Outcome { found: Found::default(), job: None, can_generate: false })["pending"], serde_json::json!(false));
    }

    #[test]
    fn generated_lyrics_are_stored_like_database_lyrics() {
        let dir = std::env::temp_dir().join(format!("wl-lyr-{}", std::process::id()));
        let l = Lyrics::new(dir.clone());
        let q = Query::new("Song", "Artist", "", 200.0);
        let found = generated_found(&q, parse_lrc("[00:05]one\n[00:09]two"), "Generated on this device (base)".into());
        assert_eq!(found.plain, "one\ntwo");
        l.store(&q.key(), &found);
        let back: Found = serde_json::from_str(&std::fs::read_to_string(l.file(&q.key())).unwrap()).unwrap();
        assert_eq!(back.synced.len(), 2);
        l.forget(&q.key());
        assert!(!l.file(&q.key()).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
