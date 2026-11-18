//! HTTP API used by the existing web UI, plus the LAN sync endpoints.

use crate::audio::{track_id, AudioEngine};
use crate::store::{merge_listening, Store};
use axum::body::Bytes;
use axum::extract::DefaultBodyLimit;
use axum::extract::{Path as AxPath, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{any, get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tower_http::cors::CorsLayer;
use tower_http::services::{ServeDir, ServeFile};

pub const BASE_URL: &str = "http://127.0.0.1:47823";
pub const LAN_PORT: u16 = 47824;
/// UDP port the Android app broadcasts `WAVELENGTH_DISCOVER_V1` on.
pub const DISCOVERY_PORT: u16 = 38423;
const DISCOVERY_PROBE: &[u8] = b"WAVELENGTH_DISCOVER_V1";
pub const VERSION: &str = "2.9.4-tauri";

pub struct AppState {
    pub store: Arc<Store>,
    pub audio: AudioEngine,
    pub downloads: Mutex<Vec<Value>>,
    pub info_cache: Mutex<HashMap<String, (Instant, Value)>>,
    pub stream_cache: Mutex<HashMap<String, (Instant, String)>>,
    pub lan_started: Mutex<bool>,
    pub discovery_started: Mutex<bool>,
    /// Native game link for the Wavelength Link Minecraft mod.
    pub bridge: crate::bridge::Bridge,
    /// In-app updates from GitHub Releases.
    pub updater: crate::updater::Updates,
    /// Lyrics from several providers, for the page and the mod.
    pub lyrics: crate::lyrics::Lyrics,
    /// Writes lyrics down by listening to the song when no database has them (needs the `whisper` feature).
    pub generator: Arc<crate::generator::Generator>,
    /// At most this many yt-dlp processes run at once, however many rows the page asks about.
    pub ytdlp: tokio::sync::Semaphore,
}

impl AppState {
    pub fn new(store: Store) -> Self {
        let cache = store.dir.join("cache/audio");
        let lyric_dir = store.dir.join("cache/lyrics");
        let generator_dir = store.dir.join("lyrics-generator");
        Self {
            store: Arc::new(store),
            audio: AudioEngine::new(cache),
            downloads: Mutex::new(Vec::new()),
            info_cache: Mutex::new(HashMap::new()),
            stream_cache: Mutex::new(HashMap::new()),
            lan_started: Mutex::new(false),
            discovery_started: Mutex::new(false),
            bridge: crate::bridge::Bridge::new(),
            updater: crate::updater::Updates::new(),
            lyrics: crate::lyrics::Lyrics::new(lyric_dir),
            generator: Arc::new(crate::generator::Generator::new(generator_dir)),
            ytdlp: tokio::sync::Semaphore::new(3),
        }
    }
}

pub struct ApiError(pub StatusCode, pub String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

type ApiResult<T> = Result<T, ApiError>;

fn bad(msg: impl Into<String>) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, msg.into())
}

pub fn router(state: Arc<AppState>, public_dir: PathBuf) -> Router {
    let index = public_dir.join("index.html");
    let covers_dir = state.store.dir.join("covers");
    let api = Router::new()
        .route("/api/state", get(get_state).put(put_state))
        .route("/api/health", get(health))
        .route("/api/config", get(get_config).put(put_config))
        .route("/api/stats", get(stats))
        .route("/api/search", get(search))
        .route("/api/match", post(match_track))
        .route("/api/info/{id}", get(info))
        .route("/api/related/{id}", get(related))
        .route("/api/stream/{id}", get(stream))
        .route("/api/warm/{id}", get(warm))
        .route("/api/cached/{id}", get(cached))
        .route("/api/local", get(local_list))
        .route("/api/local/upload", post(upload_local))
        .route("/api/local/{id}/stream", get(local_stream))
        .route("/api/local/{id}", axum::routing::delete(delete_local))
        .route("/api/cover", post(upload_cover))
        .route("/api/cover/{name}", axum::routing::delete(delete_cover))
        .route("/api/silence/{id}", get(silence))
        .route("/api/download", post(download))
        .route("/api/downloads", get(downloads))
        .route("/api/downloads/clear", post(clear_downloads))
        .route("/api/cache/clear", post(clear_cache))
        .route("/api/discord", post(discord))
        .route("/api/spotify-url", post(spotify_stub))
        .route("/api/spotify/{*rest}", any(spotify_stub))
        .route("/api/sync/status", get(sync_status))
        .route("/api/sync/host", post(sync_host))
        .route("/api/sync/join", post(sync_join))
        .route("/api/sync/stop", post(sync_stop))
        .route("/api/sync/disconnect", post(sync_stop))
        .route("/api/sync/listening", get(sync_listening_get).put(sync_listening_put).post(sync_listening_put))
        .route("/api/sync/push", post(sync_push))
        .route("/api/sync/pull", post(sync_pull))
        .route("/api/sync/remote", post(sync_remote))
        .route("/api/sync/find", get(sync_find))
        .route("/api/sync/folder", get(folder_status).post(folder_status))
        .route("/api/sync/folder/merge", post(empty_ok))
        .route("/api/sync/merge", post(sync_merge))
        .route("/api/sync/merge/preview", post(sync_preview))
        .route("/api/audio/status", get(audio_status))
        .route("/api/audio/spectrum", get(audio_spectrum))
        .route("/api/audio/{action}", post(audio_action))
        .route("/api/log", post(client_log))
        .merge(crate::bridge::routes())
        .merge(crate::updater::routes())
        .merge(crate::lyrics::routes())
        .with_state(state);

    Router::new()
        .merge(api)
        .nest_service("/covers", ServeDir::new(covers_dir))
        .fallback_service(ServeDir::new(&public_dir).fallback(ServeFile::new(index)))
        .layer(DefaultBodyLimit::max(64 * 1024 * 1024))
        .layer(CorsLayer::permissive())
}

async fn empty_ok() -> Json<Value> {
    Json(json!({ "ok": true }))
}

/// Folder sync (a shared Dropbox/iCloud directory) is a Node-only feature in
/// this pass. Report it honestly instead of failing with a 405.
async fn folder_status() -> Json<Value> {
    Json(json!({
        "ok": false,
        "rev": 0,
        "dir": "",
        "files": [],
        "error": "Sync folder is unavailable in the native build. Pair devices with Host / Join instead."
    }))
}

/* ───────────────────────── state ───────────────────────── */

async fn get_state(State(st): State<Arc<AppState>>) -> Json<Value> {
    Json(st.store.state())
}

async fn put_state(State(st): State<Arc<AppState>>, body: Bytes) -> ApiResult<Json<Value>> {
    let value: Value = serde_json::from_slice(&body).map_err(|e| bad(e.to_string()))?;
    st.store.write_state(value).map_err(bad)?;
    st.store.bump_rev();
    Ok(Json(json!({ "ok": true })))
}

async fn get_config(State(st): State<Arc<AppState>>) -> Json<Value> {
    Json(st.store.config())
}

async fn put_config(State(st): State<Arc<AppState>>, body: Bytes) -> ApiResult<Json<Value>> {
    let patch: Value = serde_json::from_slice(&body).map_err(|e| bad(e.to_string()))?;
    let mut config = st.store.config();
    if let (Some(dst), Some(src)) = (config.as_object_mut(), patch.as_object()) {
        for (k, v) in src {
            dst.insert(k.clone(), v.clone());
        }
    }
    st.store.kv_set("config", &config).map_err(bad)?;
    Ok(Json(json!({ "ok": true })))
}

async fn health(State(st): State<Arc<AppState>>) -> Json<Value> {
    let ytdlp = has_cmd("yt-dlp");
    let ffmpeg = has_cmd("ffmpeg");
    let ffprobe = has_cmd("ffprobe");
    let version = st
        .store
        .state()
        .get("version")
        .and_then(|v| v.as_str())
        .unwrap_or(VERSION)
        .to_string();
    Json(json!({
        "ok": true,
        "native": true,
        "version": version,
        "ytdlp": ytdlp,
        "ffmpeg": ffmpeg,
        "ffprobe": ffprobe,
        "spotify": false,
        "spotifyConnected": false,
        "sync": true,
        "audio": "native",
        "platform": std::env::consts::OS
    }))
}

async fn stats(State(st): State<Arc<AppState>>) -> Json<Value> {
    let cache_count = st.audio.cached_count();
    let cache_bytes = dir_size(&st.store.dir.join("cache"));
    let covers_dir = st.store.dir.join("covers");
    let local_dir = st.store.dir.join("local");
    let cover_count = count_files(&covers_dir);
    let cover_bytes = dir_size(&covers_dir);
    let (local_count, local_bytes) = walk_stats(&local_dir);
    let resolution_count = st.stream_cache.lock().map(|c| c.len()).unwrap_or(0);
    let downloads = st.downloads.lock().map(|d| d.len()).unwrap_or(0);
    Json(json!({
        "version": VERSION,
        "cacheCount": cache_count,
        "cacheBytes": cache_bytes,
        "cacheLimit": 8u64 * 1024 * 1024 * 1024,
        "coverCount": cover_count,
        "coverBytes": cover_bytes,
        "localCount": local_count,
        "localBytes": local_bytes,
        "resolutionCount": resolution_count,
        "dataBytes": dir_size(&st.store.dir),
        "matchedCount": st.store.state().get("matchedCount").and_then(|v| v.as_u64()).unwrap_or(0),
        "downloads": downloads
    }))
}

fn count_files(dir: &std::path::Path) -> u64 {
    let Ok(rd) = std::fs::read_dir(dir) else { return 0 };
    rd.flatten().filter(|e| e.path().is_file()).count() as u64
}

fn walk_stats(dir: &std::path::Path) -> (u64, u64) {
    let Ok(rd) = std::fs::read_dir(dir) else { return (0, 0) };
    let mut count = 0;
    let mut bytes = 0;
    for entry in rd.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let (c, b) = walk_stats(&path);
            count += c;
            bytes += b;
        } else if path.is_file() {
            count += 1;
            bytes += entry.metadata().map(|m| m.len()).unwrap_or(0);
        }
    }
    (count, bytes)
}

/* ───────────────────────── YouTube ───────────────────────── */

#[derive(Deserialize)]
struct SearchQuery {
    q: Option<String>,
}

pub(crate) fn ytdlp_bin() -> String {
    std::env::var("WL_YTDLP")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| find_bin("yt-dlp"))
        .unwrap_or_else(|| "yt-dlp".into())
}

pub(crate) fn find_bin(name: &str) -> Option<String> {
    let home = std::env::var("HOME").unwrap_or_default();
    let mut dirs: Vec<String> = vec![
        "/opt/homebrew/bin".into(),
        "/usr/local/bin".into(),
        "/usr/bin".into(),
        "/bin".into(),
        format!("{home}/.local/bin"),
        format!("{home}/bin"),
    ];
    if let Ok(path) = std::env::var("PATH") {
        for dir in path.split(':') {
            if !dir.is_empty() && !dirs.iter().any(|d| d == dir) {
                dirs.push(dir.to_string());
            }
        }
    }
    for dir in dirs {
        let candidate = std::path::Path::new(&dir).join(name);
        if candidate.is_file() {
            return Some(candidate.to_string_lossy().to_string());
        }
    }
    None
}

fn ffmpeg_bin() -> String {
    std::env::var("WL_FFMPEG")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| find_bin("ffmpeg"))
        .unwrap_or_else(|| "ffmpeg".into())
}

fn has_cmd(name: &str) -> bool {
    let bin = find_bin(name).unwrap_or_else(|| name.to_string());
    for flag in ["--version", "-version"] {
        if Command::new(&bin)
            .arg(flag)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
        {
            return true;
        }
    }
    false
}

/// At most this many yt-dlp processes run at once. Opening a playlist makes the page ask about every visible
/// song, and an unbounded burst of Python processes can exhaust memory or file handles (seen on Linux).
const YTDLP_SLOTS: usize = 3;

struct YtSlot;

fn yt_slots() -> &'static (Mutex<usize>, std::sync::Condvar) {
    static SLOTS: std::sync::OnceLock<(Mutex<usize>, std::sync::Condvar)> = std::sync::OnceLock::new();
    SLOTS.get_or_init(|| (Mutex::new(0), std::sync::Condvar::new()))
}

impl YtSlot {
    /// Blocks (we are always on a blocking thread here) until a slot is free.
    fn take() -> YtSlot {
        let (lock, cv) = yt_slots();
        let mut n = lock.lock().unwrap_or_else(|e| e.into_inner());
        while *n >= YTDLP_SLOTS {
            n = cv.wait(n).unwrap_or_else(|e| e.into_inner());
        }
        *n += 1;
        YtSlot
    }
}

impl Drop for YtSlot {
    fn drop(&mut self) {
        let (lock, cv) = yt_slots();
        let mut n = lock.lock().unwrap_or_else(|e| e.into_inner());
        *n = n.saturating_sub(1);
        cv.notify_one();
    }
}

fn run_ytdlp(args: &[String], timeout: Duration) -> Result<String, String> {
    let _slot = YtSlot::take();
    let mut child = Command::new(ytdlp_bin())
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("yt-dlp: {e}"))?;
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if start.elapsed() < timeout => {
                std::thread::sleep(Duration::from_millis(40));
            }
            Ok(None) => {
                let _ = child.kill();
                return Err("yt-dlp timed out".into());
            }
            Err(e) => return Err(e.to_string()),
        }
    }
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(err
            .lines()
            .last()
            .unwrap_or("yt-dlp failed")
            .trim_start_matches("ERROR: ")
            .to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

fn search_sync(q: &str) -> Result<Vec<Value>, String> {
    let out = run_ytdlp(
        &[
            "--flat-playlist".into(),
            "-j".into(),
            "--no-warnings".into(),
            format!("ytsearch15:{q}"),
        ],
        Duration::from_secs(25),
    )?;
    Ok(parse_tracks(&out))
}

fn parse_tracks(out: &str) -> Vec<Value> {
    out.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter_map(|j| to_track(&j))
        .collect()
}

fn to_track(j: &Value) -> Option<Value> {
    let id = j.get("id")?.as_str()?.to_string();
    if id.is_empty() {
        return None;
    }
    let title = j.get("title").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let artist = j
        .get("channel")
        .or_else(|| j.get("uploader"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim_end_matches(" - Topic")
        .to_string();
    let duration = j
        .get("duration")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0)
        .round() as i64;
    if title.is_empty() {
        return None;
    }
    Some(json!({
        "ytId": id,
        "title": title,
        "artist": artist,
        "duration": duration,
        "art": format!("https://i.ytimg.com/vi/{id}/mqdefault.jpg")
    }))
}

async fn search(
    State(st): State<Arc<AppState>>,
    Query(q): Query<SearchQuery>,
) -> ApiResult<Json<Value>> {
    let query = q.q.unwrap_or_default();
    if query.trim().is_empty() {
        return Ok(Json(json!([])));
    }
    let _slot = st.ytdlp.acquire().await.map_err(|e| bad(e.to_string()))?;
    let items = tokio::task::spawn_blocking(move || search_sync(&query))
        .await
        .map_err(|e| bad(e.to_string()))?
        .map_err(bad)?;
    Ok(Json(json!(items)))
}

#[derive(Deserialize)]
struct MatchBody {
    title: Option<String>,
    artist: Option<String>,
    duration: Option<f64>,
}

async fn match_track(
    State(st): State<Arc<AppState>>,
    Json(body): Json<MatchBody>,
) -> ApiResult<Json<Value>> {
    let title = body.title.unwrap_or_default();
    let artist = body.artist.unwrap_or_default();
    let dur = body.duration.unwrap_or(0.0);
    if title.is_empty() && artist.is_empty() {
        return Ok(Json(json!({ "ytId": null })));
    }
    let query = format!("{} {}", artist, title).trim().to_string();
    let _slot = st.ytdlp.acquire().await.map_err(|e| bad(e.to_string()))?;
    let items = tokio::task::spawn_blocking(move || search_sync(&query))
        .await
        .map_err(|e| bad(e.to_string()))?
        .map_err(bad)?;
    let mut best: Option<(f64, String)> = None;
    for t in items {
        let Some(id) = t.get("ytId").and_then(|v| v.as_str()) else {
            continue;
        };
        let d = t.get("duration").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let mut score = 0.0;
        if dur > 0.0 {
            score -= (d - dur).abs().min(90.0);
            if (d - dur).abs() > dur * 0.5 {
                score -= 60.0;
            }
        }
        if best.as_ref().map(|(s, _)| score > *s).unwrap_or(true) {
            best = Some((score, id.to_string()));
        }
    }
    Ok(Json(json!({
        "ytId": best.map(|(_, id)| id),
    })))
}

async fn info(State(st): State<Arc<AppState>>, AxPath(id): AxPath<String>) -> ApiResult<Json<Value>> {
    if let Ok(cache) = st.info_cache.lock() {
        if let Some((at, v)) = cache.get(&id) {
            if at.elapsed() < Duration::from_secs(6 * 3600) {
                return Ok(Json(v.clone()));
            }
        }
    }
    let lookup = id.clone();
    let _slot = st.ytdlp.acquire().await.map_err(|e| bad(e.to_string()))?;
    let value = tokio::task::spawn_blocking(move || video_info_sync(&lookup))
        .await
        .map_err(|e| bad(e.to_string()))?
        .map_err(bad)?;
    if let Ok(mut cache) = st.info_cache.lock() {
        cache.insert(id, (Instant::now(), value.clone()));
    }
    Ok(Json(value))
}

fn video_info_sync(id: &str) -> Result<Value, String> {
    let out = run_ytdlp(
        &[
            "-j".into(),
            "--no-warnings".into(),
            "--no-playlist".into(),
            format!("https://www.youtube.com/watch?v={id}"),
        ],
        Duration::from_secs(30),
    )?;
    let j: Value = out
        .lines()
        .next()
        .and_then(|l| serde_json::from_str(l).ok())
        .unwrap_or_else(|| json!({}));
    let chapters = j
        .get("chapters")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .take(100)
                .map(|c| {
                    json!({
                        "title": c.get("title").and_then(|v| v.as_str()).unwrap_or(""),
                        "start": c.get("start_time").and_then(|v| v.as_f64()).unwrap_or(0.0).round() as i64,
                        "end": c.get("end_time").and_then(|v| v.as_f64()).unwrap_or(0.0).round() as i64
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let description = j
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .chars()
        .take(6000)
        .collect::<String>();
    Ok(json!({
        "duration": j.get("duration").and_then(|v| v.as_f64()).unwrap_or(0.0).round() as i64,
        "description": description,
        "chapters": chapters,
        "title": j.get("title").and_then(|v| v.as_str()).unwrap_or(""),
        "author": j.get("uploader").and_then(|v| v.as_str()).unwrap_or("")
    }))
}

async fn related(AxPath(_id): AxPath<String>) -> Json<Value> {
    Json(json!([]))
}

async fn stream(
    State(st): State<Arc<AppState>>,
    AxPath(id): AxPath<String>,
) -> ApiResult<Response> {
    if let Ok(cache) = st.stream_cache.lock() {
        if let Some((at, url)) = cache.get(&id) {
            if at.elapsed() < Duration::from_secs(3 * 3600) {
                return Ok(Redirect::temporary(url).into_response());
            }
        }
    }
    let lookup = id.clone();
    let _slot = st.ytdlp.acquire().await.map_err(|e| bad(e.to_string()))?;
    let url = tokio::task::spawn_blocking(move || resolve_stream_sync(&lookup))
        .await
        .map_err(|e| bad(e.to_string()))?
        .map_err(bad)?;
    if let Ok(mut cache) = st.stream_cache.lock() {
        cache.insert(id, (Instant::now(), url.clone()));
    }
    Ok(Redirect::temporary(&url).into_response())
}

fn resolve_stream_sync(id: &str) -> Result<String, String> {
    let out = run_ytdlp(
        &[
            "-g".into(),
            "--no-warnings".into(),
            "--no-playlist".into(),
            "-f".into(),
            "bestaudio[ext=m4a]/bestaudio[acodec^=mp4a]/bestaudio".into(),
            format!("https://www.youtube.com/watch?v={id}"),
        ],
        Duration::from_secs(25),
    )?;
    out.lines()
        .find(|l| l.starts_with("http"))
        .map(|s| s.trim().to_string())
        .ok_or_else(|| "No audio stream available".to_string())
}

/// POST /api/log: the page's breadcrumbs and errors, appended to wavelength.log.
async fn client_log(State(st): State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> StatusCode {
    if crate::bridge::guard(&headers, true).is_err() {
        return StatusCode::FORBIDDEN;
    }
    crate::diag::log(&st.store.dir, &format!("page: {}", String::from_utf8_lossy(&body)));
    StatusCode::NO_CONTENT
}

async fn warm(State(st): State<Arc<AppState>>, AxPath(id): AxPath<String>) -> ApiResult<Json<Value>> {
    if let Ok(cache) = st.stream_cache.lock() {
        if cache
            .get(&id)
            .map(|(at, _)| at.elapsed() < Duration::from_secs(3 * 3600))
            .unwrap_or(false)
        {
            return Ok(Json(json!({ "ok": true, "cached": true })));
        }
    }
    let lookup = id.clone();
    let _slot = st.ytdlp.acquire().await.map_err(|e| bad(e.to_string()))?;
    let url = tokio::task::spawn_blocking(move || resolve_stream_sync(&lookup))
        .await
        .map_err(|e| bad(e.to_string()))?
        .map_err(bad)?;
    if let Ok(mut cache) = st.stream_cache.lock() {
        cache.insert(id, (Instant::now(), url));
    }
    Ok(Json(json!({ "ok": true })))
}

async fn cached(State(st): State<Arc<AppState>>, AxPath(id): AxPath<String>) -> Json<Value> {
    let hit = st.audio.cached(&track_id(&format!("/api/stream/{id}")));
    Json(json!({
        "cached": hit.is_some(),
        "path": hit.map(|p| p.to_string_lossy().to_string())
    }))
}

async fn local_list(State(st): State<Arc<AppState>>) -> Json<Value> {
    let dir = st.store.dir.join("local");
    let tracks = tokio::task::spawn_blocking(move || scan_local(&dir))
        .await
        .unwrap_or_default();
    Json(json!({ "tracks": tracks, "folder": "" }))
}

async fn upload_local(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let raw = headers
        .get("x-filename")
        .and_then(|v| v.to_str().ok())
        .map(|v| {
            percent_decode(v)
        })
        .unwrap_or_else(|| format!("upload-{}.mp3", short_hash(&format!("{:?}", Instant::now()))));
    let name = safe_name(&raw);
    if name.is_empty() {
        return Err(bad("Missing filename"));
    }
    let dir = st.store.dir.join("local");
    std::fs::create_dir_all(&dir).map_err(|e| bad(e.to_string()))?;
    let path = dir.join(&name);
    std::fs::write(&path, &body).map_err(|e| bad(e.to_string()))?;
    let track = tokio::task::spawn_blocking({
        let path = path.clone();
        let rel = name.clone();
        move || probe_local(&path, &rel)
    })
    .await
    .map_err(|e| bad(e.to_string()))?;
    Ok(Json(track))
}

async fn delete_local(
    State(st): State<Arc<AppState>>,
    AxPath(id): AxPath<String>,
) -> Json<Value> {
    let dir = st.store.dir.join("local");
    if let Some((path, _)) = find_local(&dir, &id) {
        let _ = std::fs::remove_file(path);
        return Json(json!({ "ok": true, "deleted": id }));
    }
    Json(json!({ "ok": false, "error": "Local file not found" }))
}

async fn local_stream(
    State(st): State<Arc<AppState>>,
    AxPath(id): AxPath<String>,
) -> Response {
    let dir = st.store.dir.join("local");
    let Some((path, _)) = find_local(&dir, &id) else {
        return (StatusCode::NOT_FOUND, Json(json!({ "error": "Local file not found" }))).into_response();
    };
    match tokio::fs::read(&path).await {
        Ok(bytes) => (
            [(header::CONTENT_TYPE, mime_for(&path))],
            bytes,
        )
            .into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response(),
    }
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(a), Some(b)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push(a * 16 + b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn safe_name(raw: &str) -> String {
    let base = raw.rsplit(['/', '\\']).next().unwrap_or(raw);
    let cleaned: String = base
        .chars()
        .map(|c| if c.is_control() || "<>:\"|?*".contains(c) { '_' } else { c })
        .collect();
    cleaned.trim().trim_start_matches('.').to_string()
}

fn short_hash(s: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    format!("{:016x}", h.finish())
}

fn hash_bytes(b: &[u8]) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    b.hash(&mut h);
    format!("{:016x}", h.finish())
}

fn collect_files(dir: &std::path::Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for entry in rd.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files(&path, out);
        } else if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            if matches!(
                ext.to_lowercase().as_str(),
                "mp3" | "m4a" | "aac" | "flac" | "wav" | "aiff" | "aif" | "ogg" | "oga" | "opus" | "webm" | "alac"
            ) {
                out.push(path);
            }
        }
    }
}

fn scan_local(dir: &std::path::Path) -> Vec<Value> {
    let mut files = Vec::new();
    collect_files(dir, &mut files);
    files.sort();
    files
        .iter()
        .filter_map(|path| {
            let rel = path
                .strip_prefix(dir)
                .unwrap_or(path)
                .to_string_lossy()
                .replace('\\', "/");
            Some(probe_local(path, &rel))
        })
        .collect()
}

fn find_local(dir: &std::path::Path, id: &str) -> Option<(PathBuf, String)> {
    let mut files = Vec::new();
    collect_files(dir, &mut files);
    for path in files {
        let rel = path
            .strip_prefix(dir)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        if short_hash(&rel) == id {
            return Some((path, rel));
        }
    }
    None
}

fn probe_local(path: &std::path::Path, rel: &str) -> Value {
    let local_id = short_hash(rel);
    let fallback_title = std::path::Path::new(rel)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "Untitled".into());
    let mut title = fallback_title.clone();
    let mut artist = "Unknown artist".to_string();
    let mut album = String::new();
    let mut duration = 0u64;
    let mut lossless = false;
    let mut codec = String::new();
    let mut bit_depth = 0u64;
    let mut sample_rate = 0u64;
    let mut bitrate = 0u64;

    if let Ok(out) = Command::new(ffprobe_bin())
        .args([
            "-v",
            "quiet",
            "-print_format",
            "json",
            "-show_format",
            "-show_streams",
        ])
        .arg(path)
        .output()
    {
        if out.status.success() {
            if let Ok(v) = serde_json::from_slice::<Value>(&out.stdout) {
                let format = &v["format"];
                let audio = v["streams"]
                    .as_array()
                    .and_then(|a| a.iter().find(|s| s["codec_type"] == "audio"));
                if let Some(tags) = format.get("tags") {
                    if let Some(t) = tags.get("title").and_then(|x| x.as_str()) {
                        if !t.trim().is_empty() {
                            title = t.to_string();
                        }
                    }
                    for key in ["artist", "album_artist", "creator"] {
                        if let Some(a) = tags.get(key).and_then(|x| x.as_str()) {
                            if !a.trim().is_empty() {
                                artist = a.to_string();
                                break;
                            }
                        }
                    }
                    if let Some(a) = tags.get("album").and_then(|x| x.as_str()) {
                        album = a.to_string();
                    }
                }
                duration = format
                    .get("duration")
                    .and_then(|x| x.as_str())
                    .and_then(|x| x.parse::<f64>().ok())
                    .map(|d| d.round() as u64)
                    .unwrap_or(0);
                bitrate = format
                    .get("bit_rate")
                    .and_then(|x| x.as_str())
                    .and_then(|x| x.parse::<u64>().ok())
                    .unwrap_or(0);
                if let Some(a) = audio {
                    codec = a
                        .get("codec_name")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_lowercase();
                    bit_depth = a
                        .get("bits_per_raw_sample")
                        .and_then(|x| x.as_str())
                        .and_then(|x| x.parse::<u64>().ok())
                        .unwrap_or(0);
                    if bit_depth == 0 {
                        bit_depth = a
                            .get("bits_per_sample")
                            .and_then(|x| x.as_u64())
                            .unwrap_or(0);
                    }
                    sample_rate = a
                        .get("sample_rate")
                        .and_then(|x| x.as_str())
                        .and_then(|x| x.parse::<u64>().ok())
                        .unwrap_or(0);
                    lossless = matches!(
                        codec.as_str(),
                        "flac" | "alac" | "wavpack" | "ape" | "truehd" | "mlp"
                    ) || codec.starts_with("pcm_");
                }
            }
        }
    }

    let ext = std::path::Path::new(rel)
        .extension()
        .map(|e| e.to_string_lossy().to_uppercase())
        .unwrap_or_default();
    let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    json!({
        "localId": local_id,
        "title": title,
        "artist": artist,
        "album": album,
        "duration": duration,
        "art": Value::Null,
        "lossless": lossless,
        "format": ext,
        "codec": codec,
        "bitDepth": bit_depth,
        "sampleRate": sample_rate,
        "bitrate": bitrate,
        "size": size,
        "file": rel,
    })
}

fn ffprobe_bin() -> String {
    std::env::var("WL_FFPROBE")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| find_bin("ffprobe"))
        .unwrap_or_else(|| "ffprobe".into())
}

fn mime_for(path: &std::path::Path) -> String {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase()
        .as_str()
    {
        "mp3" => "audio/mpeg",
        "m4a" | "aac" => "audio/mp4",
        "flac" => "audio/flac",
        "wav" => "audio/wav",
        "aiff" | "aif" => "audio/aiff",
        "ogg" | "oga" | "opus" => "audio/ogg",
        "webm" => "audio/webm",
        _ => "application/octet-stream",
    }
    .to_string()
}

async fn upload_cover(
    State(st): State<Arc<AppState>>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    if body.is_empty() {
        return Err(bad("Empty image"));
    }
    let ext = if body.starts_with(&[0x89, b'P', b'N', b'G']) {
        "png"
    } else if body.starts_with(&[0xFF, 0xD8]) {
        "jpg"
    } else if body.starts_with(b"GIF8") {
        "gif"
    } else if body.len() > 12 && &body[0..4] == b"RIFF".as_slice() && &body[8..12] == b"WEBP".as_slice() {
        "webp"
    } else if body.len() > 12 && &body[4..8] == b"ftyp".as_slice() {
        "avif"
    } else {
        return Err(bad("Unsupported image format"));
    };
    let dir = st.store.dir.join("covers");
    std::fs::create_dir_all(&dir).map_err(|e| bad(e.to_string()))?;
    let name = format!("{}.{}", hash_bytes(&body), ext);
    std::fs::write(dir.join(&name), &body).map_err(|e| bad(e.to_string()))?;
    Ok(Json(json!({ "url": format!("/covers/{name}") })))
}

async fn delete_cover(
    State(st): State<Arc<AppState>>,
    AxPath(name): AxPath<String>,
) -> Json<Value> {
    let name = safe_name(&name);
    let path = st.store.dir.join("covers").join(&name);
    let ok = std::fs::remove_file(&path).is_ok();
    Json(json!({ "ok": ok }))
}

async fn silence(
    State(st): State<Arc<AppState>>,
    AxPath(id): AxPath<String>,
) -> Json<Value> {
    let Some(path) = st.audio.cached(&id) else {
        return Json(json!({ "ready": false, "unavailable": false }));
    };
    match tokio::task::spawn_blocking(move || detect_silence(&path)).await {
        Ok(Ok(value)) => Json(json!({ "ready": true, "unavailable": false, "dur": value.0, "silences": value.1 })),
        _ => Json(json!({ "ready": false, "unavailable": true })),
    }
}

fn detect_silence(path: &PathBuf) -> Result<(f64, Vec<(f64, f64)>), String> {
    let out = Command::new(ffmpeg_bin())
        .args([
            "-hide_banner",
            "-nostats",
            "-i",
            path.to_str().unwrap_or(""),
            "-vn",
            "-af",
            "silencedetect=noise=-50dB:d=1.0",
            "-f",
            "null",
            "-",
        ])
        .output()
        .map_err(|e| e.to_string())?;
    let err = String::from_utf8_lossy(&out.stderr);
    let dur = err
        .split("Duration:")
        .nth(1)
        .and_then(|s| s.split(',').next())
        .map(parse_timestamp)
        .unwrap_or(0.0);
    let mut silences = Vec::new();
    let mut start: Option<f64> = None;
    for line in err.lines() {
        if let Some(v) = line.split("silence_start:").nth(1) {
            start = v.trim().parse::<f64>().ok();
        } else if let Some(v) = line.split("silence_end:").nth(1) {
            if let (Some(a), Some(b)) = (start.take(), v.trim().parse::<f64>().ok()) {
                if b - a >= 0.9 {
                    silences.push((round2(a), round2(b)));
                }
            }
        }
    }
    Ok((round2(dur), silences))
}

fn parse_timestamp(s: &str) -> f64 {
    let parts: Vec<&str> = s.trim().split(':').collect();
    let mut secs = 0.0;
    for p in parts {
        secs = secs * 60.0 + p.parse::<f64>().unwrap_or(0.0);
    }
    secs
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

/* ───────────────────────── downloads ───────────────────────── */

#[derive(Deserialize)]
struct DownloadBody {
    #[serde(rename = "ytId")]
    yt_id: Option<String>,
    title: Option<String>,
    artist: Option<String>,
}

async fn download(
    State(st): State<Arc<AppState>>,
    Json(body): Json<DownloadBody>,
) -> ApiResult<Json<Value>> {
    let Some(yt_id) = body.yt_id.filter(|s| !s.is_empty()) else {
        return Err(bad("No YouTube id"));
    };
    let title = body.title.unwrap_or_else(|| yt_id.clone());
    let id = format!("dl-{yt_id}");
    if let Ok(mut list) = st.downloads.lock() {
        if list
            .iter()
            .any(|d| d.get("id").and_then(|v| v.as_str()) == Some(&id))
        {
            return Ok(Json(json!({ "ok": true, "id": id })));
        }
        list.insert(
            0,
            json!({ "id": id, "ytId": yt_id, "name": title, "artist": body.artist, "status": "queued" }),
        );
    }
    let store = st.store.clone();
    spawn_download(yt_id, title, store, st.clone());
    Ok(Json(json!({ "ok": true })))
}

fn spawn_download(yt_id: String, title: String, store: Arc<Store>, st: Arc<AppState>) {
    std::thread::spawn(move || {
        let id = format!("dl-{yt_id}");
        let set_status = |status: &str, message: &str| {
            if let Ok(mut list) = st.downloads.lock() {
                if let Some(job) = list.iter_mut().find(|d| d.get("id").and_then(|v| v.as_str()) == Some(&id)) {
                    job["status"] = json!(status);
                    if !message.is_empty() {
                        job["error"] = json!(message);
                    }
                }
            }
        };
        set_status("downloading", "");
        let config = store.config();
        let dir = config
            .get("downloadDir")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let dir = if dir.is_empty() {
            crate::store::default_download_dir()
        } else {
            dir
        };
        let _ = std::fs::create_dir_all(&dir);
        let mut args: Vec<String> = vec![
            "-x".into(),
            "--audio-format".into(),
            "mp3".into(),
            "--no-playlist".into(),
            "--newline".into(),
            "-o".into(),
            format!("{dir}/%(title)s.%(ext)s"),
        ];
        if let Some(ffmpeg) = find_bin("ffmpeg") {
            args.push("--ffmpeg-location".into());
            args.push(ffmpeg);
        }
        args.push(format!("https://www.youtube.com/watch?v={yt_id}"));
        let out = Command::new(ytdlp_bin()).args(args).output();
        match out {
            Ok(o) if o.status.success() => {
                set_status("done", "");
                if let Ok(mut list) = st.downloads.lock() {
                    if let Some(job) = list.iter_mut().find(|d| d.get("id").and_then(|v| v.as_str()) == Some(&id)) {
                        job["name"] = json!(title);
                        job["file"] = json!(dir);
                    }
                }
            }
            Ok(o) => set_status("error", String::from_utf8_lossy(&o.stderr).lines().last().unwrap_or("download failed")),
            Err(e) => set_status("error", &e.to_string()),
        }
    });
}

async fn downloads(State(st): State<Arc<AppState>>) -> Json<Value> {
    Json(Value::Array(st.downloads.lock().map(|d| d.clone()).unwrap_or_default()))
}

async fn clear_downloads(State(st): State<Arc<AppState>>) -> Json<Value> {
    if let Ok(mut list) = st.downloads.lock() {
        list.retain(|d| {
            !matches!(
                d.get("status").and_then(|v| v.as_str()),
                Some("done") | Some("error")
            )
        });
    }
    Json(json!({ "ok": true }))
}

async fn clear_cache(State(st): State<Arc<AppState>>) -> Json<Value> {
    st.audio.clear_cache();
    Json(json!({ "ok": true }))
}

async fn discord() -> Json<Value> {
    Json(json!({ "connected": false }))
}

async fn spotify_stub() -> Json<Value> {
    Json(json!({ "ok": false, "error": "Spotify import is only available in the web build" }))
}

/* ───────────────────────── native audio ───────────────────────── */

#[derive(Deserialize)]
struct AudioBody {
    channel: Option<String>,
    src: Option<String>,
    position: Option<f64>,
    volume: Option<f32>,
    rate: Option<f32>,
}

async fn audio_status(State(st): State<Arc<AppState>>) -> Json<Value> {
    Json(json!(st.audio.status()))
}

async fn audio_spectrum(State(st): State<Arc<AppState>>) -> Response {
    let bins = st.audio.spectrum();
    ([(header::CONTENT_TYPE, "application/octet-stream")], bins).into_response()
}

async fn audio_action(
    State(st): State<Arc<AppState>>,
    AxPath(action): AxPath<String>,
    Json(body): Json<AudioBody>,
) -> ApiResult<Json<Value>> {
    let channel = body.channel.unwrap_or_else(|| "a".into());
    match action.as_str() {
        "play" => {
            let src = body.src.ok_or_else(|| bad("No source"))?;
            st.audio.play(&channel, &src);
        }
        "load" => {
            if let Some(src) = body.src {
                st.audio.load(&channel, &src);
            }
        }
        "pause" => st.audio.pause(&channel),
        "stop" => st.audio.stop(&channel),
        "seek" => st.audio.seek(&channel, body.position.unwrap_or(0.0)),
        "volume" => {
            if let Some(v) = body.volume {
                st.audio.set_volume(v);
            }
        }
        "rate" => {
            if let Some(v) = body.rate {
                st.audio.set_rate(v);
            }
        }
        _ => return Err(bad("Unknown audio action")),
    }
    Ok(Json(json!({ "ok": true, "status": st.audio.status() })))
}

/* ───────────────────────── sync ───────────────────────── */

async fn sync_status(State(st): State<Arc<AppState>>) -> Json<Value> {
    let mut meta = st.store.sync_meta();
    let hosting = meta.get("role").and_then(Value::as_str) == Some("host");
    let urls = if hosting { sync_urls() } else { Vec::new() };
    if let Some(first) = urls.first() {
        meta["hostUrl"] = json!(first);
    }
    meta["port"] = json!(LAN_PORT);
    meta["discoveryPort"] = json!(DISCOVERY_PORT);
    meta["urls"] = json!(urls);
    meta["device"] = json!(hostname());
    meta["listening"] = st
        .store
        .state()
        .get("listening")
        .cloned()
        .unwrap_or(Value::Null);
    Json(meta)
}

fn gen_code() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let seed = std::process::id() as u64 * 1_000_003 + nanos as u64;
    let mut x = seed;
    let mut s = String::with_capacity(8);
    for _ in 0..8 {
        x ^= x >> 33;
        x = x.wrapping_mul(0xff51afd7ed558ccd);
        x ^= x >> 33;
        s.push(char::from(b"0123456789ABCDEF"[(x & 0xf) as usize]));
    }
    s
}

/// Match the old server and the Android client: codes are uppercase letters
/// and digits only, so punctuation typed by hand never breaks pairing.
fn clean_code(value: &str) -> String {
    value
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(24)
        .collect::<String>()
        .to_ascii_uppercase()
}

async fn sync_host(State(st): State<Arc<AppState>>) -> ApiResult<Json<Value>> {
    start_lan_host(st.clone());
    let mut meta = st.store.sync_meta();
    meta["role"] = json!("host");
    meta["enabled"] = json!(true);
    if meta.get("code").and_then(|v| v.as_str()).unwrap_or("").is_empty() {
        meta["code"] = json!(gen_code());
    }
    meta["rev"] = json!(st.store.bump_rev());
    meta["hostUrl"] = json!(sync_urls()
        .into_iter()
        .next()
        .unwrap_or_else(|| format!("http://127.0.0.1:{LAN_PORT}")));
    meta["port"] = json!(LAN_PORT);
    st.store.set_sync_meta(meta);
    Ok(Json(sync_status(State(st)).await.0))
}

#[derive(Deserialize)]
struct JoinBody {
    url: Option<String>,
    code: Option<String>,
    #[allow(dead_code)]
    merge: Option<bool>,
}

async fn sync_join(
    State(st): State<Arc<AppState>>,
    Json(body): Json<JoinBody>,
) -> ApiResult<Json<Value>> {
    let base = normalize_base(body.url.unwrap_or_default());
    if base.is_empty() {
        return Err(bad("Enter the host address"));
    }
    let code = body.code.unwrap_or_default();
    let remote = tokio::task::spawn_blocking({
        let base = base.clone();
        let code = code.clone();
        move || remote_json(&base, "GET", "/sync/library", &code, None)
    })
    .await
    .map_err(|e| bad(e.to_string()))?
    .map_err(bad)?;
    let library = remote.get("library").cloned().unwrap_or(Value::Null);
    let local = st.store.state();
    let merged = merge_library_state(&local, &library);
    st.store.write_state(merged).map_err(bad)?;
    let mut meta = st.store.sync_meta();
    meta["role"] = json!("guest");
    meta["enabled"] = json!(true);
    meta["hostUrl"] = json!(base);
    meta["code"] = json!(code);
    meta["peer"] = remote.get("host").cloned().unwrap_or(json!("host"));
    meta["rev"] = remote.get("rev").cloned().unwrap_or(json!(0));
    meta["lastSync"] = json!(now_ms());
    st.store.set_sync_meta(meta.clone());
    // Send our library so the host sees this device too.
    let _ = tokio::task::spawn_blocking({
        let base = meta["hostUrl"].as_str().unwrap_or("").to_string();
        let code = meta["code"].as_str().unwrap_or("").to_string();
        let body = json!({ "library": st.store.state(), "host": hostname() });
        move || remote_json(&base, "PUT", "/sync/library", &code, Some(body))
    })
    .await;
    Ok(Json(meta))
}

async fn sync_stop(State(st): State<Arc<AppState>>) -> Json<Value> {
    let mut meta = st.store.sync_meta();
    meta["role"] = json!("");
    meta["enabled"] = json!(false);
    meta["hostUrl"] = json!("");
    meta["peer"] = json!("");
    st.store.set_sync_meta(meta.clone());
    Json(meta)
}

async fn sync_remote(
    State(st): State<Arc<AppState>>,
    body: Bytes,
) -> Json<Value> {
    let value: Value = serde_json::from_slice(&body).unwrap_or_else(|_| json!({}));
    let mut meta = st.store.sync_meta();
    if meta.get("role").and_then(|v| v.as_str()) == Some("guest") {
        if let Some(listening) = value.get("listening") {
            st.store.merge_listening_into_state(listening);
        }
    }
    meta["lastSync"] = json!(now_ms());
    // Host side: hand back the latest revision so paired devices can catch up.
    meta["rev"] = json!(st.store.bump_rev());
    st.store.set_sync_meta(meta.clone());
    Json(meta)
}

async fn sync_listening_get(State(st): State<Arc<AppState>>) -> Json<Value> {
    Json(json!({
        "listening": st.store.state().get("listening").cloned().unwrap_or(json!({ "v": 1, "devices": {} })),
        "rev": st.store.sync_meta().get("rev").cloned().unwrap_or(json!(0))
    }))
}

async fn sync_listening_put(
    State(st): State<Arc<AppState>>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let value: Value = serde_json::from_slice(&body).map_err(|e| bad(e.to_string()))?;
    let incoming = value.get("listening").cloned().unwrap_or(Value::Null);
    let merged = st.store.merge_listening_into_state(&incoming);
    let meta = st.store.sync_meta();
    if meta.get("role").and_then(|v| v.as_str()) == Some("guest") {
        let base = meta.get("hostUrl").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let code = meta.get("code").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if !base.is_empty() {
            let payload = json!({ "listening": merged, "device": hostname() });
            let remote = tokio::task::spawn_blocking(move || {
                remote_json(&base, "PUT", "/sync/listening", &code, Some(payload))
            })
            .await
            .map_err(|e| bad(e.to_string()))?;
            if let Ok(remote) = remote {
                if let Some(listening) = remote.get("listening") {
                    st.store.merge_listening_into_state(listening);
                }
            }
        }
    }
    Ok(Json(json!({
        "listening": st.store.state().get("listening").cloned().unwrap_or(Value::Null),
        "rev": st.store.bump_rev()
    })))
}

async fn sync_push(State(st): State<Arc<AppState>>) -> ApiResult<Json<Value>> {
    let meta = st.store.sync_meta();
    if meta.get("role").and_then(|v| v.as_str()) != Some("guest") {
        return Err(bad("This device is not connected to a host"));
    }
    let base = meta.get("hostUrl").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let code = meta.get("code").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let payload = json!({ "library": st.store.state(), "host": hostname() });
    let remote = tokio::task::spawn_blocking(move || {
        remote_json(&base, "PUT", "/sync/library", &code, Some(payload))
    })
    .await
    .map_err(|e| bad(e.to_string()))?
    .map_err(bad)?;
    let mut meta = st.store.sync_meta();
    meta["rev"] = remote.get("rev").cloned().unwrap_or(json!(0));
    meta["lastSync"] = json!(now_ms());
    st.store.set_sync_meta(meta.clone());
    Ok(Json(meta))
}

async fn sync_pull(State(st): State<Arc<AppState>>) -> ApiResult<Json<Value>> {
    let meta = st.store.sync_meta();
    if meta.get("role").and_then(|v| v.as_str()) != Some("guest") {
        return Err(bad("This device is not connected to a host"));
    }
    let base = meta.get("hostUrl").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let code = meta.get("code").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let remote = tokio::task::spawn_blocking(move || {
        remote_json(&base, "GET", "/sync/library", &code, None)
    })
    .await
    .map_err(|e| bad(e.to_string()))?
    .map_err(bad)?;
    if let Some(library) = remote.get("library") {
        let merged = merge_library_state(&st.store.state(), library);
        st.store.write_state(merged).map_err(bad)?;
    }
    let mut meta = st.store.sync_meta();
    meta["rev"] = remote.get("rev").cloned().unwrap_or(json!(0));
    meta["lastSync"] = json!(now_ms());
    st.store.set_sync_meta(meta.clone());
    Ok(Json(meta))
}

async fn sync_merge(State(st): State<Arc<AppState>>) -> ApiResult<Json<Value>> {
    sync_pull(State(st)).await
}

async fn sync_preview(State(st): State<Arc<AppState>>) -> Json<Value> {
    let state = st.store.state();
    let counts = |v: &Value, key: &str| v.get(key).and_then(|x| x.as_array()).map(|a| a.len()).unwrap_or(0);
    Json(json!({
        "ok": true,
        "add": {
            "playlists": counts(&state, "playlists"),
            "liked": counts(&state, "liked"),
            "history": counts(&state, "history")
        },
        "removed": { "playlists": 0, "liked": 0, "history": 0 },
        "sources": []
    }))
}

/* ───────────────────────── LAN host ───────────────────────── */

/// Answer UDP probes while hosting so "Find devices" works without typing an IP.
fn start_lan_discovery(st: Arc<AppState>) {
    {
        let mut started = match st.discovery_started.lock() {
            Ok(s) => s,
            Err(_) => return,
        };
        if *started {
            return;
        }
        *started = true;
    }
    std::thread::spawn(move || {
        let sock = match std::net::UdpSocket::bind((
            std::net::Ipv4Addr::UNSPECIFIED,
            DISCOVERY_PORT,
        )) {
            Ok(s) => s,
            Err(_) => return,
        };
        let _ = sock.set_broadcast(true);
        let mut buf = [0u8; 128];
        loop {
            let Ok((n, from)) = sock.recv_from(&mut buf) else {
                continue;
            };
            if n < DISCOVERY_PROBE.len() || n > 64 {
                continue;
            }
            if &buf[..DISCOVERY_PROBE.len()] != DISCOVERY_PROBE {
                continue;
            }
            let meta = st.store.sync_meta();
            if meta.get("role").and_then(Value::as_str) != Some("host") {
                continue;
            }
            if !meta.get("enabled").and_then(Value::as_bool).unwrap_or(false) {
                continue;
            }
            let reply = json!({
                "app": "wavelength",
                "name": hostname(),
                "version": VERSION,
                "port": LAN_PORT,
                "rev": meta.get("rev").and_then(Value::as_i64).unwrap_or(0),
            });
            let _ = sock.send_to(reply.to_string().as_bytes(), from);
        }
    });
}

/// Restart hosting (and discovery) when the app reopens after a crash/quit.
pub fn resume_sync_host(st: Arc<AppState>) {
    let meta = st.store.sync_meta();
    if meta.get("role").and_then(Value::as_str) == Some("host")
        && meta.get("enabled").and_then(Value::as_bool).unwrap_or(false)
    {
        start_lan_host(st);
    }
}

fn start_lan_host(st: Arc<AppState>) {
    start_lan_discovery(st.clone());
    let mut started = match st.lan_started.lock() {
        Ok(s) => s,
        Err(_) => return,
    };
    if *started {
        return;
    }
    *started = true;
    let state = st.clone();
    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(rt) => rt,
            Err(_) => return,
        };
        rt.block_on(async move {
            // The Android client and older desktop builds talk to `/api/sync/*`
            // with an `X-Wavelength-Sync` header; the native guest uses the
            // short `/sync/*` paths. Serve both so either can pair.
            let app = Router::new()
                .route("/sync/status", get(lan_status))
                .route("/api/sync/info", get(lan_status))
                .route("/sync/info", get(lan_status))
                .route("/sync/library", get(lan_library_get).put(lan_library_put))
                .route(
                    "/api/sync/library",
                    get(lan_library_get).put(lan_library_put),
                )
                .route("/sync/listening", get(lan_listening_get).put(lan_listening_put))
                .route(
                    "/api/sync/listening",
                    get(lan_listening_get).put(lan_listening_put),
                )
                .route("/api/sync/preview", axum::routing::put(lan_preview))
                .route("/sync/preview", axum::routing::put(lan_preview))
                .route("/api/sync/now", post(lan_now))
                .route("/sync/now", post(lan_now))
                .with_state(state.clone())
                .layer(DefaultBodyLimit::max(64 * 1024 * 1024))
                .layer(CorsLayer::permissive());
            let listener = match tokio::net::TcpListener::bind(("0.0.0.0", LAN_PORT)).await {
                Ok(l) => l,
                Err(_) => return,
            };
            let _ = axum::serve(listener, app).await;
        });
    });
}

fn authorized(st: &AppState, headers: &HeaderMap, query: &HashMap<String, String>) -> bool {
    let code = clean_code(
        st.store
            .sync_meta()
            .get("code")
            .and_then(|v| v.as_str())
            .unwrap_or(""),
    );
    if code.is_empty() {
        return true;
    }
    let header_code = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string())
    };
    let given = header_code("x-wl-code")
        .or_else(|| header_code("x-wavelength-sync"))
        .or_else(|| query.get("code").cloned())
        .unwrap_or_default();
    clean_code(&given) == code
}

async fn lan_status(State(st): State<Arc<AppState>>) -> Json<Value> {
    let meta = st.store.sync_meta();
    Json(json!({
        "ok": true,
        "app": "wavelength",
        "name": hostname(),
        "host": hostname(),
        "version": VERSION,
        "port": LAN_PORT,
        "rev": meta.get("rev").cloned().unwrap_or(json!(0)),
        "merge": true,
        "listening": true
    }))
}

async fn lan_library_get(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Json<Value>, ApiError> {
    if !authorized(&st, &headers, &q) {
        return Err(ApiError(StatusCode::UNAUTHORIZED, "Wrong sync code".into()));
    }
    let meta = st.store.sync_meta();
    Ok(Json(json!({
        "ok": true,
        "library": st.store.state(),
        "rev": meta.get("rev").cloned().unwrap_or(json!(0)),
        "host": hostname(),
        "updatedAt": now_ms()
    })))
}

async fn lan_library_put(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    if !authorized(&st, &headers, &q) {
        return Err(ApiError(StatusCode::UNAUTHORIZED, "Wrong sync code".into()));
    }
    let value: Value = serde_json::from_slice(&body).map_err(|e| bad(e.to_string()))?;
    let incoming = value.get("library").cloned().unwrap_or_else(|| value.clone());
    let mode = q
        .get("mode")
        .cloned()
        .or_else(|| {
            headers
                .get("x-wavelength-sync-mode")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string())
        })
        .unwrap_or_default();
    let mut next = if mode == "merge" {
        merge_library_state(&st.store.state(), &incoming)
    } else {
        // Plain PUT replaces the shared library, matching the old host and the
        // "Send changes" / "Pull" wording in the UI.
        let mut state = st.store.state();
        if !state.is_object() {
            state = json!({});
        }
        if let Some(obj) = state.as_object_mut() {
            for key in ["playlists", "liked", "history"] {
                if let Some(field) = incoming.get(key) {
                    obj.insert(key.to_string(), field.clone());
                }
            }
        }
        state
    };
    if let (Some(obj), Some(listening)) = (next.as_object_mut(), incoming.get("listening")) {
        if !listening.is_null() {
            let merged = merge_listening(obj.get("listening").unwrap_or(&Value::Null), listening);
            obj.insert("listening".into(), merged);
        }
    }
    st.store.write_state(next.clone()).map_err(bad)?;
    let rev = st.store.bump_rev();
    Ok(Json(json!({
        "ok": true,
        "rev": rev,
        "host": hostname(),
        "updatedAt": now_ms(),
        "library": next
    })))
}

async fn lan_preview(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    if !authorized(&st, &headers, &q) {
        return Err(ApiError(StatusCode::UNAUTHORIZED, "Wrong sync code".into()));
    }
    let value: Value = serde_json::from_slice(&body).unwrap_or_else(|_| json!({}));
    let incoming = value.get("library").cloned().unwrap_or_else(|| value.clone());
    let state = st.store.state();
    let count = |v: &Value, key: &str| {
        v.get(key)
            .and_then(|x| x.as_array())
            .map(|a| a.len())
            .unwrap_or(0)
    };
    let device = value
        .get("device")
        .and_then(Value::as_str)
        .unwrap_or("Device");
    // The Android client tolerates a host that cannot preview; without a
    // `selected` map it merges everything, which is the safe default.
    Ok(Json(json!({
        "ok": true,
        "role": "guest",
        "sources": [device],
        "add": {
            "playlists": count(&incoming, "playlists"),
            "liked": count(&incoming, "liked"),
            "history": count(&incoming, "history")
        },
        "removed": { "playlists": [], "songs": [], "liked": [] },
        "counts": {
            "playlists": count(&state, "playlists"),
            "liked": count(&state, "liked"),
            "history": count(&state, "history")
        }
    })))
}

async fn lan_now() -> Json<Value> {
    // Now-playing relay is a desktop-only nicety; answer harmlessly so paired
    // phones do not surface an error for it.
    Json(json!({ "ok": true, "devices": [], "cmds": [] }))
}

async fn lan_listening_get(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Json<Value>, ApiError> {
    if !authorized(&st, &headers, &q) {
        return Err(ApiError(StatusCode::UNAUTHORIZED, "Wrong sync code".into()));
    }
    Ok(Json(json!({
        "listening": st.store.state().get("listening").cloned().unwrap_or(json!({ "v": 1, "devices": {} })),
        "rev": st.store.sync_meta().get("rev").cloned().unwrap_or(json!(0))
    })))
}

async fn lan_listening_put(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    if !authorized(&st, &headers, &q) {
        return Err(ApiError(StatusCode::UNAUTHORIZED, "Wrong sync code".into()));
    }
    let value: Value = serde_json::from_slice(&body).map_err(|e| bad(e.to_string()))?;
    let incoming = value.get("listening").cloned().unwrap_or(Value::Null);
    let merged = st.store.merge_listening_into_state(&incoming);
    Ok(Json(json!({ "ok": true, "listening": merged, "rev": st.store.bump_rev() })))
}

/* ───────────────────────── helpers ───────────────────────── */

fn merge_library_state(local: &Value, remote: &Value) -> Value {
    let mut out = local.clone();
    if !out.is_object() {
        out = json!({});
    }
    let Some(obj) = out.as_object_mut() else {
        return out;
    };
    let key_of = |t: &Value| -> String {
        if let Some(v) = t.get("localId").and_then(|v| v.as_str()) {
            return format!("local:{v}");
        }
        if let Some(v) = t.get("spId").and_then(|v| v.as_str()) {
            return format!("sp:{v}");
        }
        if let Some(v) = t.get("ytId").and_then(|v| v.as_str()) {
            return format!("yt:{v}");
        }
        format!(
            "x:{}|{}",
            t.get("title").and_then(|v| v.as_str()).unwrap_or("").to_lowercase(),
            t.get("artist").and_then(|v| v.as_str()).unwrap_or("").to_lowercase()
        )
    };

    for key in ["liked", "history"] {
        let mut list = obj
            .get(key)
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let mut seen: Vec<String> = list.iter().map(key_of).collect();
        if let Some(arr) = remote.get(key).and_then(|v| v.as_array()) {
            for t in arr {
                let k = key_of(t);
                if !k.is_empty() && !seen.iter().any(|s| s == &k) {
                    seen.push(k);
                    list.push(t.clone());
                }
            }
        }
        if key == "history" {
            list.truncate(400);
        }
        obj.insert(key.into(), json!(list));
    }

    // Playlists merge by id; the local copy wins when both exist.
    let mut playlists = obj
        .get("playlists")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let mut ids: Vec<String> = playlists
        .iter()
        .map(|p| {
            p.get("id")
                .and_then(|v| v.as_str())
                .or_else(|| p.get("spId").and_then(|v| v.as_str()))
                .unwrap_or("")
                .to_string()
        })
        .collect();
    if let Some(arr) = remote.get("playlists").and_then(|v| v.as_array()) {
        for p in arr {
            let id = p
                .get("id")
                .and_then(|v| v.as_str())
                .or_else(|| p.get("spId").and_then(|v| v.as_str()))
                .unwrap_or("")
                .to_string();
            if id.is_empty() || ids.iter().any(|s| s == &id) {
                continue;
            }
            ids.push(id);
            playlists.push(p.clone());
        }
    }
    obj.insert("playlists".into(), json!(playlists));

    let merged_listening = merge_listening(
        local.get("listening").unwrap_or(&Value::Null),
        remote.get("listening").unwrap_or(&Value::Null),
    );
    obj.insert("listening".into(), merged_listening);
    out
}

fn normalize_base(raw: String) -> String {
    let raw = raw.trim().trim_end_matches('/');
    if raw.is_empty() {
        return String::new();
    }
    let with_scheme = if raw.starts_with("http://") || raw.starts_with("https://") {
        raw.to_string()
    } else {
        format!("http://{raw}")
    };
    let after_scheme = with_scheme.split("://").nth(1).unwrap_or("");
    if after_scheme.contains(':') {
        with_scheme
    } else {
        format!("{with_scheme}:{LAN_PORT}")
    }
}

fn remote_json(
    base: &str,
    method: &str,
    path: &str,
    code: &str,
    body: Option<Value>,
) -> Result<Value, String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| e.to_string())?;
    // The Tauri host serves short `/sync/*` paths; the old Node host and the
    // Android client use `/api/sync/*`. Try both so either can be a host.
    let mut paths = vec![path.to_string()];
    if path.starts_with("/sync/") {
        paths.push(format!("/api{path}"));
    }
    let mut last_error = String::from("Sync request failed");
    let mut last_status = None;
    for path in paths {
        let url = format!("{base}{path}");
        let mut req = match method {
            "PUT" => client.put(&url),
            "POST" => client.post(&url),
            _ => client.get(&url),
        };
        if !code.is_empty() {
            req = req
                .header("x-wl-code", code)
                .header("x-wavelength-sync", code);
        }
        if let Some(b) = &body {
            req = req.json(b);
        }
        let resp = match req.send() {
            Ok(resp) => resp,
            Err(e) => {
                last_error = e.to_string();
                continue;
            }
        };
        let status = resp.status();
        let value: Value = resp.json().unwrap_or_else(|_| json!({}));
        if status.is_success() {
            return Ok(value);
        }
        last_status = Some(status);
        last_error = value
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("Sync request failed")
            .to_string();
        if status != reqwest::StatusCode::NOT_FOUND {
            break;
        }
    }
    if last_status == Some(reqwest::StatusCode::NOT_FOUND) {
        return Err("The host does not answer the sync API".into());
    }
    Err(last_error)
}

fn hostname() -> String {
    Command::new("hostname")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Wavelength".into())
}

/// Every non-loopback IPv4 address on this machine, as a sync URL.
///
/// Phones cannot guess the computer's address, so the host UI shows all of
/// them and the UDP discovery reply points at the same port. Link-local
/// (169.254.x.x) addresses are useless for sync, so they are skipped.
fn sync_urls() -> Vec<String> {
    let mut urls: Vec<String> = Vec::new();
    if let Ok(interfaces) = if_addrs::get_if_addrs() {
        for iface in interfaces {
            let if_addrs::IfAddr::V4(v4) = iface.addr else {
                continue;
            };
            if v4.ip.is_loopback() || v4.ip.is_link_local() || v4.ip.is_unspecified() {
                continue;
            }
            let url = format!("http://{}:{}", v4.ip, LAN_PORT);
            if !urls.contains(&url) {
                urls.push(url);
            }
        }
    }
    urls
}

/// Broadcast addresses to send a discovery probe to, including the global one.
fn broadcast_addrs() -> Vec<std::net::Ipv4Addr> {
    let mut out = vec![std::net::Ipv4Addr::new(255, 255, 255, 255)];
    if let Ok(interfaces) = if_addrs::get_if_addrs() {
        for iface in interfaces {
            let if_addrs::IfAddr::V4(v4) = iface.addr else {
                continue;
            };
            if v4.ip.is_loopback() || v4.ip.is_link_local() {
                continue;
            }
            let broadcast = v4.broadcast.or_else(|| {
                let ip = u32::from(v4.ip);
                let mask = u32::from(v4.netmask);
                Some(std::net::Ipv4Addr::from((ip & mask) | !mask))
            });
            if let Some(broadcast) = broadcast {
                if !out.contains(&broadcast) {
                    out.push(broadcast);
                }
            }
        }
    }
    out
}

/// Probe the local network for hosts answering the discovery broadcast.
fn probe_sync_hosts(timeout: Duration) -> Vec<Value> {
    let Ok(sock) = std::net::UdpSocket::bind((std::net::Ipv4Addr::UNSPECIFIED, 0)) else {
        return Vec::new();
    };
    let _ = sock.set_broadcast(true);
    let _ = sock.set_read_timeout(Some(Duration::from_millis(100)));
    for addr in broadcast_addrs() {
        let _ = sock.send_to(DISCOVERY_PROBE, (addr, DISCOVERY_PORT));
    }
    // A host on this same machine is not reached by broadcast on some systems.
    let _ = sock.send_to(DISCOVERY_PROBE, (std::net::Ipv4Addr::LOCALHOST, DISCOVERY_PORT));

    let start = Instant::now();
    let mut found: Vec<Value> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut buf = [0u8; 2048];
    while start.elapsed() < timeout {
        let (n, from) = match sock.recv_from(&mut buf) {
            Ok(v) => v,
            Err(ref e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue
            }
            Err(_) => continue,
        };
        let Ok(value) = serde_json::from_slice::<Value>(&buf[..n]) else {
            continue;
        };
        if value.get("app").and_then(Value::as_str) != Some("wavelength") {
            continue;
        }
        let port = match value.get("port").and_then(Value::as_u64) {
            Some(p) if p > 0 && p <= u16::MAX as u64 => p as u16,
            _ => continue,
        };
        if !seen.insert(format!("{}:{}", from.ip(), port)) {
            continue;
        }
        let fallback = from.ip().to_string();
        found.push(json!({
            "url": format!("http://{}:{}", from.ip(), port),
            "name": value
                .get("name")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or(&fallback),
            "version": value.get("version").and_then(Value::as_str).unwrap_or(""),
        }));
    }
    found
}

async fn sync_find() -> Json<Value> {
    let hosts = tokio::task::spawn_blocking(|| probe_sync_hosts(Duration::from_millis(1500)))
        .await
        .unwrap_or_default();
    Json(json!({
        "hosts": hosts,
        "discoveryPort": DISCOVERY_PORT,
        "port": LAN_PORT
    }))
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn dir_size(path: &std::path::Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    let mut total = 0;
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
            total += dir_size(&p);
        } else {
            total += entry.metadata().map(|m| m.len()).unwrap_or(0);
        }
    }
    total
}
