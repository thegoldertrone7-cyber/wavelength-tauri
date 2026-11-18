//! The last resort of the lyric service: listen to the song and write the lyrics down.
//!
//! When LRCLIB, NetEase, KuGou and lyrics.ovh all come back empty, the core can transcribe the song on this
//! computer with Whisper (speech-to-text, run by the `whisper-rs` crate) and keep the result as time-synced
//! lyrics in the same cache the databases use. Nothing leaves the computer except the one-off download of the
//! model file from Hugging Face.
//!
//! The job runs on its own thread. `/api/lyrics` reports `{ pending, stage, progress }` while it runs, and the
//! page and the Minecraft mod poll until the lines arrive.
//!
//! Speech recognition is compiled in only with `--features whisper` (it needs cmake and a C++ compiler). Without
//! it everything here still builds; `Generator::compiled()` is false and a job fails at once with a clear message.

use crate::lyrics::Line;
use serde::Serialize;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const MODEL_BASE_URL: &str = "https://huggingface.co/ggerganov/whisper.cpp/resolve/main";
/// Whisper works on 16 kHz mono audio.
const WHISPER_RATE: f64 = 16_000.0;
/// A failed job is not retried for this long, so a poll loop cannot restart a transcription that keeps failing.
const RETRY_AFTER: Duration = Duration::from_secs(300);
/// Songs longer than this are not transcribed (a mix or a podcast would take far too long).
const MAX_SECONDS: f64 = 12.0 * 60.0;

/// (id, file, size in MB, what it is good for)
pub const MODELS: [(&str, &str, u64, &str); 3] = [
    ("tiny", "ggml-tiny.bin", 75, "Fastest, roughest"),
    ("base", "ggml-base.bin", 142, "Balanced (recommended)"),
    ("small", "ggml-small.bin", 466, "Most accurate, slower"),
];

/* ───────────────────────── status ───────────────────────── */

#[derive(Clone, Debug, Serialize)]
pub struct JobStatus {
    /// "model" | "audio" | "decode" | "listen" | "done" | "failed"
    pub stage: String,
    /// 0..1 inside the current stage; `overall` rolls the stages up into one bar.
    pub progress: f64,
    pub error: Option<String>,
}

impl JobStatus {
    fn new(stage: &str, progress: f64) -> Self {
        Self { stage: stage.into(), progress, error: None }
    }

    fn failed(msg: impl Into<String>) -> Self {
        Self { stage: "failed".into(), progress: 1.0, error: Some(msg.into()) }
    }

    pub fn finished(&self) -> bool {
        self.stage == "done" || self.stage == "failed"
    }

    pub fn failed_now(&self) -> bool {
        self.stage == "failed"
    }

    /// One bar for the whole job.
    pub fn overall(&self) -> f64 {
        let (from, span) = match self.stage.as_str() {
            "model" => (0.0, 0.25),
            "audio" => (0.25, 0.10),
            "decode" => (0.35, 0.05),
            "listen" => (0.40, 0.58),
            "done" => (1.0, 0.0),
            _ => (0.0, 0.0),
        };
        (from + span * self.progress.clamp(0.0, 1.0)).clamp(0.0, 1.0)
    }

    pub fn label(&self) -> &'static str {
        match self.stage.as_str() {
            "model" => "Downloading the speech model (once)",
            "audio" => "Fetching the song",
            "decode" => "Preparing the audio",
            "listen" => "Listening to the song",
            "done" => "Done",
            _ => "Could not generate lyrics",
        }
    }
}

struct Job {
    status: JobStatus,
    at: Instant,
}

/// What to transcribe.
#[derive(Clone, Debug, PartialEq)]
pub enum Source {
    /// A YouTube video id: fetched through the core's own `/api/stream/{id}`.
    Youtube(String),
    /// A local file, fetched through `/api/local/{id}/stream`.
    Local(String),
}

impl Source {
    /// The page and the mod identify a track as "yt:ID", "local:ID" or "sp:ID" (a song with no source yet).
    pub fn from_track_id(id: &str) -> Option<Self> {
        let (kind, rest) = id.split_once(':')?;
        let rest = rest.trim();
        if rest.is_empty() || rest.len() > 200 || rest.contains('/') || rest.contains('\\') || rest.contains('?') {
            return None;
        }
        match kind {
            "yt" => Some(Self::Youtube(rest.to_string())),
            "local" => Some(Self::Local(rest.to_string())),
            _ => None,
        }
    }

    fn stream_path(&self) -> String {
        match self {
            Self::Youtube(id) => format!("/api/stream/{id}"),
            Self::Local(id) => format!("/api/local/{id}/stream"),
        }
    }
}

/// Everything a job needs, gathered by the route.
pub struct Spec {
    pub key: String,
    pub source: Source,
    pub title: String,
    pub artist: String,
    pub duration: f64,
    /// The song's audio when the engine already has it on disk (no download needed).
    pub cached: Option<PathBuf>,
    pub port: u16,
}

/// Called with the finished lines and a short description of where they came from.
pub type Done = Box<dyn FnOnce(Vec<Line>, String) + Send + 'static>;

/* ───────────────────────── the service ───────────────────────── */

pub struct Generator {
    dir: PathBuf,
    /// The setting the page owns and the mod follows: generate lyrics when no database has them.
    enabled: AtomicBool,
    model: Mutex<String>,
    jobs: Mutex<HashMap<String, Job>>,
    /// While a model downloads, its progress is shown here for the settings page.
    model_progress: Mutex<Option<(String, f64)>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Generator {
    pub fn new(dir: PathBuf) -> Self {
        let g = Self {
            dir,
            enabled: AtomicBool::new(false),
            model: Mutex::new("base".into()),
            jobs: Mutex::new(HashMap::new()),
            model_progress: Mutex::new(None),
        };
        g.load_config();
        g
    }

    /// Whether this build can transcribe at all.
    pub fn compiled() -> bool {
        cfg!(feature = "whisper")
    }

    fn config_file(&self) -> PathBuf {
        self.dir.join("config.json")
    }

    fn load_config(&self) {
        let Ok(text) = std::fs::read_to_string(self.config_file()) else { return };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else { return };
        self.enabled.store(v.get("generate").and_then(|x| x.as_bool()).unwrap_or(false), Ordering::Relaxed);
        if let Some(m) = v.get("model").and_then(|x| x.as_str()) {
            if MODELS.iter().any(|(id, ..)| *id == m) {
                *lock(&self.model) = m.to_string();
            }
        }
    }

    pub fn configure(&self, generate: Option<bool>, model: Option<&str>) {
        if let Some(on) = generate {
            self.enabled.store(on, Ordering::Relaxed);
        }
        if let Some(m) = model.filter(|m| MODELS.iter().any(|(id, ..)| id == m)) {
            *lock(&self.model) = m.to_string();
        }
        let body = serde_json::json!({ "generate": self.enabled(), "model": self.model_id() });
        if std::fs::create_dir_all(&self.dir).is_ok() {
            let _ = std::fs::write(self.config_file(), body.to_string());
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    pub fn model_id(&self) -> String {
        lock(&self.model).clone()
    }

    fn model_path(&self, id: &str) -> Option<PathBuf> {
        MODELS.iter().find(|(m, ..)| *m == id).map(|(_, file, ..)| self.dir.join("models").join(file))
    }

    pub fn model_ready(&self, id: &str) -> bool {
        let Some((_, _, mb, _)) = MODELS.iter().find(|(m, ..)| *m == id) else { return false };
        // A file far smaller than the model is a broken download.
        self.model_path(id)
            .and_then(|p| std::fs::metadata(p).ok())
            .map_or(false, |m| m.len() > mb * 1_000_000 / 2)
    }

    /// For Settings: what is installed, what is chosen, and what is downloading.
    pub fn describe(&self) -> serde_json::Value {
        let downloading = lock(&self.model_progress).clone();
        serde_json::json!({
            "ok": true,
            "compiled": Self::compiled(),
            "generate": self.enabled(),
            "model": self.model_id(),
            "downloading": downloading.map(|(id, p)| serde_json::json!({ "id": id, "progress": p })),
            "models": MODELS.iter().map(|(id, _, mb, note)| serde_json::json!({
                "id": id, "mb": mb, "note": note, "ready": self.model_ready(id),
            })).collect::<Vec<_>>(),
        })
    }

    /// Where this key stands. Does not start anything.
    pub fn status(&self, key: &str) -> Option<JobStatus> {
        lock(&self.jobs).get(key).map(|j| j.status.clone())
    }

    fn set(&self, key: &str, status: JobStatus) {
        if let Some(j) = lock(&self.jobs).get_mut(key) {
            j.status = status;
            j.at = Instant::now();
        }
    }

    /// The state of the job for this lyric key, starting one when there is none.
    /// `None` when the song cannot be transcribed (too long).
    pub fn ensure(self: &Arc<Self>, spec: Spec, done: Done) -> Option<JobStatus> {
        if spec.duration > MAX_SECONDS {
            return None;
        }
        {
            let mut jobs = lock(&self.jobs);
            if let Some(j) = jobs.get(&spec.key) {
                // A running job reports its progress; a finished one is reported, and a failed one is not restarted for a while.
                if !j.status.finished() || j.at.elapsed() < RETRY_AFTER {
                    return Some(j.status.clone());
                }
            }
            if jobs.values().any(|j| !j.status.finished()) {
                // One transcription at a time: it uses every core.
                return Some(JobStatus::new("audio", 0.0));
            }
            let first = if self.model_ready(&self.model_id()) { "audio" } else { "model" };
            jobs.insert(spec.key.clone(), Job { status: JobStatus::new(first, 0.0), at: Instant::now() });
        }
        let start = lock(&self.jobs).get(&spec.key).map(|j| j.status.clone());
        let me = Arc::clone(self);
        let spawned = std::thread::Builder::new().name("wavelength-lyric-generator".into()).spawn(move || {
            let model = me.model_id();
            match me.run(&spec, &model) {
                Ok(lines) => {
                    let lines = clean_generated(lines, spec.duration);
                    if lines.is_empty() {
                        me.set(&spec.key, JobStatus::failed("No singing or speech was found in the song."));
                    } else {
                        done(lines, format!("Generated on this device ({model})"));
                        me.set(&spec.key, JobStatus::new("done", 1.0));
                    }
                }
                Err(e) => {
                    crate::diag::log(&me.dir, &format!("lyric generator: {} - {}: {e}", spec.title, spec.artist));
                    me.set(&spec.key, JobStatus::failed(e));
                }
            }
        });
        if spawned.is_err() {
            return Some(JobStatus::failed("Could not start the generator."));
        }
        start
    }

    fn run(self: &Arc<Self>, spec: &Spec, model_id: &str) -> Result<Vec<Line>, String> {
        if !Self::compiled() {
            return Err("This build of Wavelength has no speech recognition. Rebuild with `--features whisper`.".into());
        }
        let key = spec.key.as_str();
        let model = self.ensure_model(key, model_id)?;

        self.set(key, JobStatus::new("audio", 0.0));
        let (file, temp) = match &spec.cached {
            Some(p) if p.is_file() => (p.clone(), false),
            _ => (self.fetch_audio(spec)?, true),
        };

        self.set(key, JobStatus::new("decode", 0.0));
        let samples = decode_16k_mono(&file, MAX_SECONDS);
        if temp {
            let _ = std::fs::remove_file(&file);
        }
        let samples = samples?;
        if samples.len() < (WHISPER_RATE as usize) * 3 {
            return Err("The song is too short to transcribe.".into());
        }

        self.set(key, JobStatus::new("listen", 0.0));
        let (me, k) = (Arc::clone(self), key.to_string());
        transcribe(&model, &samples, Arc::new(move |p| me.set(&k, JobStatus::new("listen", p))))
    }

    /// Starts downloading a model in the background (Settings > Download), unless one is already downloading.
    pub fn prefetch_model(self: &Arc<Self>, id: &str) {
        if !MODELS.iter().any(|(m, ..)| *m == id) || self.model_ready(id) || lock(&self.model_progress).is_some() {
            return;
        }
        *lock(&self.model_progress) = Some((id.to_string(), 0.0));
        let (me, id) = (Arc::clone(self), id.to_string());
        let spawned = std::thread::Builder::new().name("wavelength-model-download".into()).spawn(move || {
            if let Err(e) = me.ensure_model("", &id) {
                crate::diag::log(&me.dir, &format!("speech model download: {e}"));
            }
            *lock(&me.model_progress) = None;
        });
        if spawned.is_err() {
            *lock(&self.model_progress) = None;
        }
    }

    /// Makes sure the model file is on disk, downloading it once.
    fn ensure_model(&self, key: &str, id: &str) -> Result<PathBuf, String> {
        let path = self.model_path(id).ok_or("Unknown speech model.")?;
        if self.model_ready(id) {
            return Ok(path);
        }
        let file = MODELS.iter().find(|(m, ..)| *m == id).map(|(_, f, ..)| *f).ok_or("Unknown speech model.")?;
        std::fs::create_dir_all(path.parent().unwrap_or(Path::new("."))).map_err(|e| e.to_string())?;
        let part = path.with_extension("part");
        let client = reqwest::blocking::Client::builder()
            .user_agent("Wavelength")
            .connect_timeout(Duration::from_secs(20))
            .timeout(Duration::from_secs(60 * 30))
            .build()
            .map_err(|e| e.to_string())?;
        let mut resp = client
            .get(format!("{MODEL_BASE_URL}/{file}"))
            .send()
            .map_err(|e| format!("Could not download the speech model: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("The speech model download failed (HTTP {}).", resp.status().as_u16()));
        }
        let total = resp.content_length().unwrap_or(0) as f64;
        let mut out = std::fs::File::create(&part).map_err(|e| e.to_string())?;
        let (mut got, mut buf, mut last) = (0u64, vec![0u8; 256 * 1024], Instant::now());
        loop {
            let n = resp.read(&mut buf).map_err(|e| format!("The speech model download was interrupted: {e}"))?;
            if n == 0 {
                break;
            }
            out.write_all(&buf[..n]).map_err(|e| e.to_string())?;
            got += n as u64;
            if last.elapsed() > Duration::from_millis(250) {
                last = Instant::now();
                let p = if total > 0.0 { got as f64 / total } else { 0.0 };
                *lock(&self.model_progress) = Some((id.to_string(), p));
                self.set(key, JobStatus::new("model", p));
            }
        }
        out.flush().map_err(|e| e.to_string())?;
        drop(out);
        *lock(&self.model_progress) = None;
        if total > 0.0 && (got as f64) < total {
            let _ = std::fs::remove_file(&part);
            return Err("The speech model download was cut short.".into());
        }
        std::fs::rename(&part, &path).map_err(|e| e.to_string())?;
        if !self.model_ready(id) {
            let _ = std::fs::remove_file(&path);
            return Err("The speech model file is damaged. Try again.".into());
        }
        Ok(path)
    }

    /// Downloads the song through the core's own stream route (it resolves YouTube and serves local files).
    fn fetch_audio(&self, spec: &Spec) -> Result<PathBuf, String> {
        let tmp = self.dir.join("tmp");
        std::fs::create_dir_all(&tmp).map_err(|e| e.to_string())?;
        let path = tmp.join(format!("{:016x}.audio", fnv(&spec.key)));
        let client = reqwest::blocking::Client::builder()
            .user_agent("Wavelength")
            .connect_timeout(Duration::from_secs(20))
            .timeout(Duration::from_secs(300))
            .build()
            .map_err(|e| e.to_string())?;
        let url = format!("http://127.0.0.1:{}{}", spec.port, spec.source.stream_path());
        let mut resp = client.get(url).send().map_err(|e| format!("Could not fetch the song: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("Could not fetch the song (HTTP {}).", resp.status().as_u16()));
        }
        let mut out = std::fs::File::create(&path).map_err(|e| e.to_string())?;
        std::io::copy(&mut resp, &mut out).map_err(|e| format!("Could not fetch the song: {e}"))?;
        Ok(path)
    }
}

fn fnv(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/* ───────────────────────── audio ───────────────────────── */

/// Interleaved samples at any rate -> mono at 16 kHz (linear interpolation; plenty for speech recognition).
pub fn to_mono_16k(samples: impl Iterator<Item = f32>, rate: u32, channels: usize, max_out: usize) -> Vec<f32> {
    let channels = channels.max(1);
    let step = rate.max(1) as f64 / WHISPER_RATE;
    let mut out: Vec<f32> = Vec::new();
    let (mut frame, mut filled) = (0.0f32, 0usize);
    let (mut idx, mut prev, mut next_out) = (0u64, 0.0f32, 0.0f64);
    for s in samples {
        frame += s;
        filled += 1;
        if filled < channels {
            continue;
        }
        let x = frame / channels as f32;
        frame = 0.0;
        filled = 0;
        if idx == 0 {
            prev = x;
        }
        while next_out <= idx as f64 {
            let frac = (next_out - (idx as f64 - 1.0)).clamp(0.0, 1.0) as f32;
            out.push(prev + (x - prev) * frac);
            next_out += step;
        }
        prev = x;
        idx += 1;
        if out.len() >= max_out {
            break;
        }
    }
    out
}

fn decode_16k_mono(path: &Path, max_seconds: f64) -> Result<Vec<f32>, String> {
    use rodio::Source;
    let file = std::fs::File::open(path).map_err(|e| format!("Could not open the song: {e}"))?;
    let dec = rodio::Decoder::try_from(file).map_err(|e| format!("Unsupported audio format: {e}"))?;
    let rate = u32::from(dec.sample_rate());
    let channels = usize::from(u16::from(dec.channels()));
    Ok(to_mono_16k(dec, rate, channels, (max_seconds * WHISPER_RATE) as usize))
}

/* ───────────────────────── speech recognition ───────────────────────── */

#[cfg(feature = "whisper")]
fn transcribe(model: &Path, samples: &[f32], progress: Arc<dyn Fn(f64) + Send + Sync>) -> Result<Vec<Line>, String> {
    use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};
    let path = model.to_str().ok_or("The speech model path is not valid text.")?;
    let ctx = WhisperContext::new_with_params(path, WhisperContextParameters::default())
        .map_err(|e| format!("Could not load the speech model: {e}"))?;
    let mut state = ctx.create_state().map_err(|e| e.to_string())?;
    let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(1, 8);
    params.set_n_threads(threads as i32);
    params.set_language(Some("auto"));
    params.set_translate(false);
    params.set_print_special(false);
    params.set_print_progress(false);
    params.set_print_realtime(false);
    params.set_print_timestamps(false);
    // Short segments (about one sung line each) instead of whole sentences.
    params.set_token_timestamps(true);
    params.set_max_len(46);
    params.set_split_on_word(true);
    // Music makes Whisper repeat itself; carrying text between windows makes it worse.
    params.set_no_context(true);
    params.set_suppress_blank(true);
    let report = Arc::clone(&progress);
    params.set_progress_callback_safe(move |p: i32| report(p as f64 / 100.0));
    progress(0.0);
    state.full(params, samples).map_err(|e| format!("Speech recognition failed: {e}"))?;
    progress(1.0);
    let n = state.full_n_segments().map_err(|e| e.to_string())?;
    let mut lines = Vec::new();
    for i in 0..n {
        let text = state.full_get_segment_text(i).map_err(|e| e.to_string())?;
        let t0 = state.full_get_segment_t0(i).map_err(|e| e.to_string())?; // centiseconds
        lines.push(Line { t: t0 as f64 / 100.0, text: text.trim().to_string(), tr: None });
    }
    Ok(lines)
}

#[cfg(not(feature = "whisper"))]
fn transcribe(_model: &Path, _samples: &[f32], _progress: Arc<dyn Fn(f64) + Send + Sync>) -> Result<Vec<Line>, String> {
    Err("This build of Wavelength has no speech recognition.".into())
}

/* ───────────────────────── cleaning ───────────────────────── */

fn is_noise(text: &str) -> bool {
    let t = text.trim();
    if t.is_empty() {
        return true;
    }
    // Whisper writes sounds in brackets: [Music], (applause), ♪ ♪
    let bracketed = (t.starts_with('[') && t.ends_with(']')) || (t.starts_with('(') && t.ends_with(')')) || (t.starts_with('*') && t.ends_with('*'));
    if bracketed {
        return true;
    }
    if t.chars().all(|c| !c.is_alphanumeric()) {
        return true;
    }
    let l = t.to_lowercase();
    ["thanks for watching", "thank you for watching", "subtitles by", "amara.org", "subscribe to", "like and subscribe", "transcribed by"]
        .iter()
        .any(|k| l.contains(k))
}

/// Removes what Whisper invents over music: bracketed sounds, sign-offs, and the same line over and over.
pub fn clean_generated(lines: Vec<Line>, duration: f64) -> Vec<Line> {
    let mut out: Vec<Line> = Vec::new();
    let mut run = 0usize;
    let mut last_text = String::new();
    let mut lines = lines;
    lines.sort_by(|a, b| a.t.partial_cmp(&b.t).unwrap_or(std::cmp::Ordering::Equal));
    for mut l in lines {
        l.text = l.text.split_whitespace().collect::<Vec<_>>().join(" ");
        if is_noise(&l.text) || !l.t.is_finite() || l.t < 0.0 {
            continue;
        }
        if duration > 0.0 && l.t > duration + 5.0 {
            continue;
        }
        let same = l.text.to_lowercase() == last_text;
        run = if same { run + 1 } else { 1 };
        last_text = l.text.to_lowercase();
        // A chorus repeats a line a few times; a loop repeats it until the song ends.
        if run > 3 {
            continue;
        }
        out.push(l);
    }
    // Most of the song being one line (even with the loops cut) means nothing real was recognised.
    if out.len() >= 8 {
        let mut counts: HashMap<String, usize> = HashMap::new();
        for l in &out {
            *counts.entry(l.text.to_lowercase()).or_insert(0) += 1;
        }
        if counts.values().copied().max().unwrap_or(0) * 10 > out.len() * 6 {
            return Vec::new();
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(t: f64, s: &str) -> Line {
        Line { t, text: s.into(), tr: None }
    }

    #[test]
    fn track_ids() {
        assert_eq!(Source::from_track_id("yt:abc123"), Some(Source::Youtube("abc123".into())));
        assert_eq!(Source::from_track_id("local:9f"), Some(Source::Local("9f".into())));
        assert_eq!(Source::from_track_id("sp:xyz"), None);
        assert_eq!(Source::from_track_id("yt:../etc"), None, "a path is never an id");
        assert_eq!(Source::from_track_id("yt:"), None);
        assert_eq!(Source::from_track_id("nothing"), None);
    }

    #[test]
    fn cleaning_drops_invented_text() {
        let raw = vec![
            line(1.0, "[Music]"),
            line(2.0, "  Hello   darling "),
            line(3.0, "♪"),
            line(4.0, "Thanks for watching!"),
            line(5.0, "Hello darling"),
            line(99.0, "after the end"),
        ];
        let out = clean_generated(raw, 60.0);
        let texts: Vec<_> = out.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(texts, vec!["Hello darling", "Hello darling"]);
    }

    #[test]
    fn a_loop_is_cut_and_a_chorus_is_kept() {
        let looped: Vec<Line> = (0..10).map(|i| line(i as f64 * 3.0, "la la la")).collect();
        assert_eq!(clean_generated(looped, 0.0).len(), 3, "a line may repeat three times in a row, not ten");
        let chorus = vec![line(1.0, "stay"), line(3.0, "stay"), line(5.0, "stay"), line(7.0, "go")];
        assert_eq!(clean_generated(chorus, 0.0).len(), 4);
    }

    #[test]
    fn one_line_everywhere_means_nothing_was_heard() {
        let mut raw = Vec::new();
        for i in 0..3 {
            for j in 0..3 {
                raw.push(line((i * 4 + j) as f64, "uh"));
            }
            raw.push(line((i * 4 + 3) as f64, &format!("other {i}")));
        }
        assert!(clean_generated(raw, 0.0).is_empty());
    }

    #[test]
    fn resampling_keeps_length_and_downmixes() {
        // one second of stereo at 48 kHz: left 1.0, right 0.0 -> mono 0.5, 16000 samples
        let stereo = std::iter::repeat([1.0f32, 0.0]).take(48_000).flatten();
        let out = to_mono_16k(stereo, 48_000, 2, usize::MAX);
        assert!((out.len() as i64 - 16_000).abs() <= 2, "got {}", out.len());
        assert!(out.iter().all(|v| (*v - 0.5).abs() < 1e-5));
        // the cap stops early
        let capped = to_mono_16k(std::iter::repeat(0.1f32).take(48_000), 48_000, 1, 100);
        assert_eq!(capped.len(), 100);
    }

    #[test]
    fn overall_progress_only_goes_forward() {
        let order = ["model", "audio", "decode", "listen", "done"];
        let mut last = -1.0;
        for st in order {
            for p in [0.0, 0.5, 1.0] {
                let v = JobStatus::new(st, p).overall();
                assert!(v >= last - 1e-9, "{st} {p}: {v} < {last}");
                last = v;
            }
        }
        assert_eq!(JobStatus::new("done", 1.0).overall(), 1.0);
    }

    #[test]
    fn config_round_trips_and_ignores_unknown_models() {
        let dir = std::env::temp_dir().join(format!("wl-gen-{}", std::process::id()));
        let g = Generator::new(dir.clone());
        assert!(!g.enabled());
        g.configure(Some(true), Some("small"));
        g.configure(None, Some("gigantic"));
        let again = Generator::new(dir.clone());
        assert!(again.enabled());
        assert_eq!(again.model_id(), "small");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
