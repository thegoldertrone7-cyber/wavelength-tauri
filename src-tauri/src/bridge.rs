//! Native game link for the Wavelength Link Minecraft mod (NeoForge 1.21.1).
//!
//! The mod used to need `gamelink.patch` applied to the Node server. In this
//! build the Rust core answers the same `/api/bridge/*` protocol itself, so the
//! mod works out of the box, and it answers better than the patch could:
//!
//! * position, play state and duration come straight from the audio engine, so
//!   they stay right even when the UI window is hidden or throttled;
//! * the level meter is the engine's real FFT of what is being played, not a
//!   copy the page has to compute and push;
//! * commands reach the page through a long poll (`/api/bridge/wait`), so they
//!   land in milliseconds instead of waiting for the next 2 s heartbeat.
//!
//! The page (`public/js/bridge.js`) still supplies what only it knows: the
//! track metadata, liked state, shuffle/repeat and the live theme tokens. It
//! pushes those only while the mod is reading, and the page applies commands
//! because the queue, shuffle and like logic live there.
//!
//! Security: the listener is bound to 127.0.0.1 only. On top of that every
//! bridge request must carry a loopback `Host`, any `Origin` must be this
//! app's own origin (so a web page in the user's browser cannot read what is
//! playing or skip tracks, even though the rest of the API is CORS-open), and
//! POSTs must carry `X-Wavelength: 1`.

use crate::api::{ApiError, AppState, VERSION};
use crate::audio::AudioStatus;
use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::Notify;

const CMDS: [&str; 8] = ["toggle", "play", "pause", "next", "prev", "seek", "like", "volume"];
const TOKENS: [&str; 12] = [
    "bg", "card", "card2", "line", "text", "text2", "text3", "dot", "invert", "onInvert", "accent",
    "onAccent",
];
/// The mod counts as "watching" this long after its last read of `/state`.
const WATCH_MS: u64 = 6000;
/// A command nobody collected within this long is dropped, so a closed or
/// frozen page never replays an old "next" minutes later.
const CMD_TTL: Duration = Duration::from_secs(5);
const MAX_QUEUE: usize = 16;
const BANDS: usize = 24;

static NULL: Value = Value::Null;

type R<T> = Result<T, ApiError>;

#[derive(Default)]
struct Inner {
    /// The page's last sanitised snapshot (track, theme, transport intent).
    meta: Option<Value>,
    /// Last time the page talked to us (push, hello, or a long poll).
    page_seen: Option<Instant>,
    /// Last time the mod read `/state`.
    watched: Option<Instant>,
    cmds: VecDeque<(String, f64, Instant)>,
}

pub struct Bridge {
    inner: Mutex<Inner>,
    wake: Notify,
    /// Long polls currently parked: while one is open the page is alive.
    waiting: AtomicUsize,
}

impl Default for Bridge {
    fn default() -> Self {
        Self::new()
    }
}

impl Bridge {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            wake: Notify::new(),
            waiting: AtomicUsize::new(0),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn is_watched(i: &Inner) -> bool {
        i.watched
            .map_or(false, |t| t.elapsed() < Duration::from_millis(WATCH_MS))
    }

    /// Records a read by the mod. Returns true when it was not already watching.
    fn mark_watched(&self) -> bool {
        let mut i = self.lock();
        let was = Self::is_watched(&i);
        i.watched = Some(Instant::now());
        !was
    }

    fn touch_page(&self) {
        self.lock().page_seen = Some(Instant::now());
    }

    fn store_meta(&self, meta: Value) {
        let mut i = self.lock();
        i.meta = Some(meta);
        i.page_seen = Some(Instant::now());
    }

    /// Queues a command for the page and wakes its long poll. Returns the queue length.
    pub fn queue(&self, cmd: &str, value: f64) -> usize {
        let mut i = self.lock();
        // A held volume key or a scrubbed seek bar only needs the latest value.
        if cmd == "volume" || cmd == "seek" {
            i.cmds.retain(|(c, _, _)| c.as_str() != cmd);
        }
        if i.cmds.len() < MAX_QUEUE {
            i.cmds.push_back((cmd.to_string(), value, Instant::now()));
        }
        let n = i.cmds.len();
        drop(i);
        self.wake.notify_one();
        n
    }

    /// Drains the queue (dropping anything stale) and says whether the mod is watching.
    fn take(&self) -> (bool, Vec<Value>) {
        let mut i = self.lock();
        let cmds = i
            .cmds
            .drain(..)
            .filter(|(_, _, at)| at.elapsed() < CMD_TTL)
            .map(|(c, v, _)| json!({ "cmd": c, "value": v }))
            .collect();
        (Self::is_watched(&i), cmds)
    }

    /// The page's snapshot and how long ago the page was last heard from
    /// (-1 never, 0 while a long poll is parked).
    fn view(&self) -> (Option<Value>, i64) {
        let i = self.lock();
        let age = if self.waiting.load(Ordering::Relaxed) > 0 {
            0
        } else {
            i.page_seen
                .map_or(-1, |t| t.elapsed().as_millis().min(i64::MAX as u128) as i64)
        };
        (i.meta.clone(), age)
    }
}

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/bridge/state", get(state))
        .route("/api/bridge/hello", get(hello))
        .route("/api/bridge/wait", get(wait))
        .route("/api/bridge/push", post(push))
        .route("/api/bridge/cmd", post(cmd))
}

/* ───────────────────────── request guard ───────────────────────── */

fn deny(code: StatusCode, msg: &str) -> ApiError {
    ApiError(code, msg.to_string())
}

fn loopback_host(host: &str) -> bool {
    let name = match host.rsplit_once(':') {
        Some((n, p)) if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) => n,
        _ => host,
    };
    name.eq_ignore_ascii_case("127.0.0.1") || name.eq_ignore_ascii_case("localhost")
}

/// Only this app's own page may call the bridge from a browser context.
fn origin_ok(origin: &str) -> bool {
    let Some(rest) = origin.strip_prefix("http://") else {
        return false;
    };
    let Some((name, port)) = rest.rsplit_once(':') else {
        return false;
    };
    port == crate::PORT.to_string()
        && (name.eq_ignore_ascii_case("127.0.0.1") || name.eq_ignore_ascii_case("localhost"))
}

fn guard(h: &HeaderMap, is_post: bool) -> R<()> {
    let host = h.get(header::HOST).and_then(|v| v.to_str().ok()).unwrap_or("");
    if !loopback_host(host) {
        return Err(deny(StatusCode::FORBIDDEN, "The game link only answers this computer"));
    }
    if let Some(o) = h.get(header::ORIGIN) {
        if !origin_ok(o.to_str().unwrap_or("")) {
            return Err(deny(StatusCode::FORBIDDEN, "The game link does not answer other web pages"));
        }
    }
    if is_post && h.get("x-wavelength").and_then(|v| v.to_str().ok()) != Some("1") {
        return Err(deny(StatusCode::FORBIDDEN, "Missing X-Wavelength header"));
    }
    Ok(())
}

/* ───────────────────────── handlers ───────────────────────── */

/// What the mod polls. Page metadata overlaid with the engine's live numbers.
async fn state(State(st): State<Arc<AppState>>, headers: HeaderMap) -> R<Json<Value>> {
    guard(&headers, false)?;
    let b = &st.bridge;
    if b.mark_watched() {
        // The mod just started reading: ask the page for a fresh snapshot now.
        b.wake.notify_one();
    }
    let (meta, age) = b.view();
    let state = meta.map(|m| overlay(m, &st.audio.status(), || st.audio.spectrum()));
    Ok(Json(json!({
        "ok": true,
        "native": true,
        "app": "wavelength-tauri",
        "version": VERSION,
        "serverTime": now_ms(),
        "age": age,
        "state": state,
    })))
}

/// Quiet heartbeat kept for pages that do not long-poll.
async fn hello(State(st): State<Arc<AppState>>, headers: HeaderMap) -> R<Json<Value>> {
    guard(&headers, false)?;
    st.bridge.touch_page();
    let (watch, cmds) = st.bridge.take();
    Ok(Json(json!({ "ok": true, "watch": watch, "cmds": cmds })))
}

#[derive(Deserialize)]
struct WaitQuery {
    /// Longest time to hold the request open, in ms (capped at 25 s).
    ms: Option<u64>,
    /// The watch flag the page already knows (0/1). A change ends the wait.
    w: Option<u8>,
}

struct Waiting<'a>(&'a Bridge);

impl<'a> Waiting<'a> {
    fn enter(b: &'a Bridge) -> Self {
        b.waiting.fetch_add(1, Ordering::Relaxed);
        Self(b)
    }
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        self.0.waiting.fetch_sub(1, Ordering::Relaxed);
        self.0.touch_page();
    }
}

/// Long poll for the page: returns as soon as a command is queued or the
/// mod starts or stops watching, otherwise after `ms`.
async fn wait(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<WaitQuery>,
) -> R<Json<Value>> {
    guard(&headers, false)?;
    let b = &st.bridge;
    let known = q.w.unwrap_or(0) != 0;
    let limit = Duration::from_millis(q.ms.unwrap_or(20_000).min(25_000));
    let started = Instant::now();
    let _parked = Waiting::enter(b);
    loop {
        b.touch_page();
        let (watch, cmds) = b.take();
        if !cmds.is_empty() || watch != known || started.elapsed() >= limit {
            return Ok(Json(json!({ "ok": true, "watch": watch, "cmds": cmds })));
        }
        // notify_one stores a permit, so a command queued between take() and
        // here still wakes us. A stale permit only costs one extra loop.
        let left = limit.saturating_sub(started.elapsed());
        let _ = tokio::time::timeout(left, b.wake.notified()).await;
    }
}

/// The page publishes its snapshot and collects any queued commands.
async fn push(State(st): State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> R<Json<Value>> {
    guard(&headers, true)?;
    let raw: Value = serde_json::from_slice(&body)
        .map_err(|e| deny(StatusCode::BAD_REQUEST, &format!("Bad JSON: {e}")))?;
    st.bridge.store_meta(clean_meta(&raw));
    let (watch, cmds) = st.bridge.take();
    Ok(Json(json!({ "ok": true, "watch": watch, "cmds": cmds })))
}

#[derive(Deserialize)]
struct CmdBody {
    cmd: String,
    value: Option<f64>,
}

/// The mod queues a transport command.
async fn cmd(State(st): State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> R<Json<Value>> {
    guard(&headers, true)?;
    let b: CmdBody = serde_json::from_slice(&body)
        .map_err(|e| deny(StatusCode::BAD_REQUEST, &format!("Bad JSON: {e}")))?;
    if !CMDS.contains(&b.cmd.as_str()) {
        return Err(deny(StatusCode::BAD_REQUEST, "Unknown command"));
    }
    let value = b.value.filter(|v| v.is_finite()).unwrap_or(0.0).clamp(0.0, 1.0);
    let queued = st.bridge.queue(&b.cmd, value);
    Ok(Json(json!({ "ok": true, "queued": queued })))
}

/* ───────────────────────── engine overlay ───────────────────────── */

fn sanitize_id(v: &str) -> String {
    v.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(64)
        .collect()
}

/// True when the engine is playing the track the page is showing. During a
/// track change the page and the engine can briefly disagree, and then the
/// page's own numbers are the safer ones.
fn engine_matches(meta_id: &str, engine_id: &str) -> bool {
    match meta_id.strip_prefix("yt:") {
        Some(yt) => sanitize_id(yt) == engine_id,
        None => true,
    }
}

fn round3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

/// Replaces the page's transport numbers with the engine's and adds real levels.
fn overlay(mut s: Value, eng: &AudioStatus, spectrum: impl FnOnce() -> Vec<u8>) -> Value {
    let id = s.pointer("/track/id").and_then(Value::as_str).unwrap_or("").to_string();
    let live = !eng.src.is_empty() && engine_matches(&id, &eng.track_id);
    let mut playing = s.get("playing").and_then(Value::as_bool).unwrap_or(false);
    if live {
        // `loading` with paused=false is "starting": show it as playing so the
        // HUD does not flicker to a pause icon while a track buffers.
        playing = eng.playing || (eng.loading && !eng.paused);
        s["playing"] = json!(playing);
        s["pos"] = json!(round3(eng.position.max(0.0)));
        if eng.duration > 0.0 && s.get("track").map_or(false, Value::is_object) {
            s["track"]["dur"] = json!(round3(eng.duration));
        }
    }
    // The tap keeps its last samples while paused, so only report levels while playing.
    s["bands"] = if live && playing {
        json!(bands_from(&spectrum()))
    } else {
        json!([])
    };
    s
}

/// 24 bands (0-100) from the engine's 256-bin spectrum, using the same
/// mapping the app's own full-screen visualiser uses.
fn bands_from(bins: &[u8]) -> Vec<u8> {
    if bins.is_empty() {
        return vec![0; BANDS];
    }
    (0..BANDS)
        .map(|i| {
            let idx = ((i as f32 / BANDS as f32).powf(1.55) * bins.len() as f32 * 0.7) as usize + 1;
            let raw = bins[idx.min(bins.len() - 1)] as f32 / 255.0 * 1.12;
            (raw.clamp(0.0, 1.0) * 100.0).round() as u8
        })
        .collect()
}

/* ───────────────────────── sanitising the page's snapshot ───────────────────────── */

fn field<'a>(o: &'a Value, k: &str) -> &'a Value {
    o.get(k).unwrap_or(&NULL)
}

fn truthy(v: &Value) -> bool {
    v.as_bool().unwrap_or(false)
}

fn num(v: &Value, lo: f64, hi: f64, default: f64) -> f64 {
    match v.as_f64() {
        Some(x) if x.is_finite() => x.clamp(lo, hi),
        _ => default,
    }
}

fn text(v: &Value, max: usize) -> String {
    v.as_str()
        .map(|s| s.chars().take(max).collect::<String>())
        .unwrap_or_default()
}

fn art(v: &Value) -> String {
    match v.as_str() {
        Some(s) if s.len() < 600 && (s.starts_with("http://") || s.starts_with("https://") || s.starts_with('/')) => {
            s.to_string()
        }
        _ => String::new(),
    }
}

/// `#rgb`..`#rrggbbaa`, or `rgb(...)` / `rgba(...)` containing only numbers,
/// spaces and `. , % /`. Anything else would end up in the mod's colour parser.
fn color_ok(s: &str) -> bool {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix('#') {
        return (3..=8).contains(&hex.len()) && hex.bytes().all(|b| b.is_ascii_hexdigit());
    }
    let lower = s.to_ascii_lowercase();
    let body = lower
        .strip_prefix("rgba(")
        .or_else(|| lower.strip_prefix("rgb("));
    match body {
        Some(rest) if rest.ends_with(')') && rest.len() > 1 => rest[..rest.len() - 1]
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b' ' | b'.' | b',' | b'%' | b'/')),
        _ => false,
    }
}

fn color(v: &Value) -> String {
    match v.as_str() {
        Some(s) if color_ok(s) => s.trim().chars().take(48).collect::<String>(),
        _ => String::new(),
    }
}

fn clean_meta(b: &Value) -> Value {
    let t = field(b, "track");
    let th = field(b, "theme");

    let mut theme = Map::new();
    theme.insert("id".into(), json!(text(field(th, "id"), 40)));
    theme.insert("style".into(), json!(text(field(th, "style"), 24)));
    theme.insert(
        "mode".into(),
        json!(if field(th, "mode").as_str() == Some("light") { "light" } else { "dark" }),
    );
    theme.insert("reduced".into(), json!(truthy(field(th, "reduced"))));
    for k in TOKENS {
        theme.insert(k.into(), json!(color(field(th, k))));
    }

    let track = if t.is_object() {
        json!({
            "id": text(field(t, "id"), 80),
            "title": text(field(t, "title"), 200),
            "artist": text(field(t, "artist"), 200),
            "album": text(field(t, "album"), 200),
            "art": art(field(t, "art")),
            "liked": truthy(field(t, "liked")),
            "dur": num(field(t, "dur"), 0.0, 86400.0, 0.0),
            "local": truthy(field(t, "local")),
        })
    } else {
        Value::Null
    };

    json!({
        "v": 1,
        "track": track,
        "playing": truthy(field(b, "playing")),
        "pos": num(field(b, "pos"), 0.0, 86400.0, 0.0),
        "speed": num(field(b, "speed"), 0.25, 4.0, 1.0),
        "volume": num(field(b, "volume"), 0.0, 1.0, 1.0),
        "shuffle": truthy(field(b, "shuffle")),
        "repeat": num(field(b, "repeat"), 0.0, 2.0, 0.0) as i64,
        "theme": Value::Object(theme),
        "bands": [],
    })
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/* ───────────────────────── tests ───────────────────────── */

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(host: &str, origin: Option<&str>, marker: bool) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(header::HOST, HeaderValue::from_str(host).unwrap());
        if let Some(o) = origin {
            h.insert(header::ORIGIN, HeaderValue::from_str(o).unwrap());
        }
        if marker {
            h.insert("x-wavelength", HeaderValue::from_static("1"));
        }
        h
    }

    #[test]
    fn guard_accepts_the_mod_and_the_page() {
        // The Java client: loopback host, no Origin, marker on POST.
        assert!(guard(&headers("127.0.0.1:47823", None, true), true).is_ok());
        assert!(guard(&headers("127.0.0.1:47823", None, false), false).is_ok());
        // The app's own page.
        let page = headers("127.0.0.1:47823", Some("http://127.0.0.1:47823"), true);
        assert!(guard(&page, true).is_ok());
        assert!(guard(&headers("localhost:47823", Some("http://localhost:47823"), true), true).is_ok());
    }

    #[test]
    fn guard_refuses_other_hosts_origins_and_missing_marker() {
        assert!(guard(&headers("192.168.0.5:47823", None, true), true).is_err());
        assert!(guard(&headers("evil.example:47823", None, false), false).is_err());
        assert!(guard(&headers("127.0.0.1:47823", Some("https://evil.example"), true), true).is_err());
        assert!(guard(&headers("127.0.0.1:47823", Some("http://127.0.0.1:3000"), true), true).is_err());
        assert!(guard(&headers("127.0.0.1:47823", Some("null"), true), true).is_err());
        assert!(guard(&headers("127.0.0.1:47823", None, false), true).is_err());
    }

    #[test]
    fn queue_keeps_only_the_latest_volume_and_seek() {
        let b = Bridge::new();
        b.queue("volume", 0.2);
        b.queue("volume", 0.4);
        b.queue("seek", 0.1);
        b.queue("seek", 0.9);
        b.queue("next", 0.0);
        let (_, cmds) = b.take();
        let names: Vec<&str> = cmds.iter().map(|c| c["cmd"].as_str().unwrap()).collect();
        assert_eq!(names, vec!["volume", "seek", "next"]);
        assert_eq!(cmds[0]["value"], json!(0.4));
        assert_eq!(cmds[1]["value"], json!(0.9));
        assert!(b.take().1.is_empty());
    }

    #[test]
    fn queue_is_bounded() {
        let b = Bridge::new();
        for _ in 0..40 {
            b.queue("next", 0.0);
        }
        assert_eq!(b.take().1.len(), MAX_QUEUE);
    }

    #[test]
    fn watching_is_tracked_and_reported() {
        let b = Bridge::new();
        assert!(b.mark_watched(), "first read starts watching");
        assert!(!b.mark_watched(), "second read was already watching");
        assert!(b.take().0);
    }

    #[test]
    fn clean_meta_clamps_and_drops_junk() {
        let raw = json!({
            "track": { "id": "yt:abc", "title": "T".repeat(500), "artist": "A", "art": "javascript:alert(1)",
                       "liked": true, "dur": 1e9 },
            "playing": true, "pos": -5, "speed": 99, "volume": 7, "repeat": 9,
            "theme": { "id": "mocha", "mode": "light", "accent": "rgb(10, 20, 30)", "bg": "url(http://x)",
                       "text": "#fff; background:red" },
            "bands": [1, 2, 3]
        });
        let m = clean_meta(&raw);
        assert_eq!(m["track"]["title"].as_str().unwrap().chars().count(), 200);
        assert_eq!(m["track"]["art"], json!(""));
        assert_eq!(m["track"]["dur"], json!(86400.0));
        assert_eq!(m["pos"], json!(0.0));
        assert_eq!(m["speed"], json!(4.0));
        assert_eq!(m["volume"], json!(1.0));
        assert_eq!(m["repeat"], json!(2));
        assert_eq!(m["theme"]["mode"], json!("light"));
        assert_eq!(m["theme"]["accent"], json!("rgb(10, 20, 30)"));
        assert_eq!(m["theme"]["bg"], json!(""));
        assert_eq!(m["theme"]["text"], json!(""));
        assert_eq!(m["bands"], json!([]), "bands come from the engine, never the page");
    }

    #[test]
    fn clean_meta_without_a_track_is_null() {
        assert!(clean_meta(&json!({})).get("track").unwrap().is_null());
        assert!(clean_meta(&json!({ "track": "x" })).get("track").unwrap().is_null());
    }

    #[test]
    fn colours() {
        for ok in ["#fff", "#ffcc00", "#ffcc0080", "rgb(1, 2, 3)", "rgba(1,2,3,0.5)", "rgb(1 2 3 / 50%)"] {
            assert!(color_ok(ok), "{ok}");
        }
        for bad in ["", "red", "#ff", "#fffffffff", "rgb(1,2,3", "rgb(a,b,c)", "rgb(1,2,3);x", "url(x)"] {
            assert!(!color_ok(bad), "{bad}");
        }
    }

    #[test]
    fn bands_follow_the_signal() {
        assert_eq!(bands_from(&[0u8; 256]), vec![0u8; BANDS]);
        assert_eq!(bands_from(&[255u8; 256]), vec![100u8; BANDS]);
        assert_eq!(bands_from(&[]).len(), BANDS);
        let mut ramp = [0u8; 256];
        for (i, v) in ramp.iter_mut().enumerate() {
            *v = i as u8;
        }
        let b = bands_from(&ramp);
        assert_eq!(b.len(), BANDS);
        assert!(b.windows(2).all(|w| w[0] <= w[1]), "a rising spectrum gives rising bands");
    }

    fn meta(id: &str, playing: bool) -> Value {
        clean_meta(&json!({ "track": { "id": id, "title": "T", "dur": 100 }, "playing": playing, "pos": 5 }))
    }

    #[test]
    fn overlay_prefers_the_engine_when_it_plays_the_same_track() {
        let mut eng = AudioStatus::default();
        eng.src = "/api/stream/abc123?q=best".into();
        eng.track_id = "abc123".into();
        eng.playing = true;
        eng.paused = false;
        eng.position = 42.0;
        eng.duration = 200.0;
        let s = overlay(meta("yt:abc123", false), &eng, || vec![255u8; 256]);
        assert_eq!(s["playing"], json!(true));
        assert_eq!(s["pos"], json!(42.0));
        assert_eq!(s["track"]["dur"], json!(200.0));
        assert_eq!(s["bands"].as_array().unwrap().len(), BANDS);
    }

    #[test]
    fn overlay_keeps_the_page_while_the_engine_is_on_another_track() {
        let mut eng = AudioStatus::default();
        eng.src = "/api/stream/old?q=best".into();
        eng.track_id = "old".into();
        eng.playing = true;
        eng.paused = false;
        eng.position = 99.0;
        let s = overlay(meta("yt:new", true), &eng, || panic!("no levels for a different track"));
        assert_eq!(s["pos"], json!(5.0));
        assert_eq!(s["bands"], json!([]));
    }

    #[test]
    fn overlay_has_no_levels_while_paused() {
        let mut eng = AudioStatus::default();
        eng.src = "/api/stream/abc?q=best".into();
        eng.track_id = "abc".into();
        eng.playing = false;
        eng.paused = true;
        eng.position = 10.0;
        let s = overlay(meta("yt:abc", true), &eng, || panic!("tap is stale while paused"));
        assert_eq!(s["playing"], json!(false));
        assert_eq!(s["pos"], json!(10.0));
        assert_eq!(s["bands"], json!([]));
    }

    #[test]
    fn overlay_shows_a_buffering_track_as_playing() {
        let mut eng = AudioStatus::default();
        eng.src = "/api/stream/abc?q=best".into();
        eng.track_id = "abc".into();
        eng.playing = false;
        eng.paused = false;
        eng.loading = true;
        let s = overlay(meta("yt:abc", false), &eng, || vec![0u8; 256]);
        assert_eq!(s["playing"], json!(true));
    }

    #[test]
    fn local_tracks_use_the_engine_too() {
        assert!(engine_matches("local:song.flac", "whatever"));
        assert!(engine_matches("yt:a-b_c", "a-b_c"));
        assert!(!engine_matches("yt:a", "b"));
    }
}
