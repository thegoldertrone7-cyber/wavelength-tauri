//! Native audio engine: HTTP/Range input -> symphonia decode (via rodio) ->
//! CoreAudio/ALSA/WASAPI output, with a sample tap for the real visualizer.
//!
//! The web UI still talks to `<audio>`-like objects. `public/js/native-audio.js`
//! forwards those calls here, so no UI code had to change.

use rodio::mixer::Mixer;
use rodio::{Decoder, DeviceSinkBuilder, MixerDeviceSink, Player, Source};
use serde::Serialize;
use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone, Serialize)]
pub struct AudioStatus {
    pub channel: String,
    pub src: String,
    pub track_id: String,
    pub playing: bool,
    pub paused: bool,
    pub loading: bool,
    pub ended: bool,
    pub position: f64,
    pub duration: f64,
    pub volume: f32,
    pub rate: f32,
    pub error: String,
}

impl Default for AudioStatus {
    fn default() -> Self {
        Self {
            channel: String::new(),
            src: String::new(),
            track_id: String::new(),
            playing: false,
            paused: true,
            loading: false,
            ended: false,
            position: 0.0,
            duration: 0.0,
            volume: 1.0,
            rate: 1.0,
            error: String::new(),
        }
    }
}

enum Cmd {
    Play { channel: String, src: String },
    Load { channel: String, src: String },
    Pause { channel: String },
    Seek { channel: String, position: f64 },
    Stop { channel: String },
    Volume(f32),
    Rate(f32),
    Ready { src: String, path: Option<PathBuf>, error: String },
}

pub struct AudioEngine {
    tx: Sender<Cmd>,
    status: Arc<Mutex<AudioStatus>>,
    tap: Arc<Mutex<VecDeque<f32>>>,
    cache: PathBuf,
    loaded: Arc<Mutex<HashMap<String, PathBuf>>>,
}

impl AudioEngine {
    pub fn new(cache: PathBuf) -> Self {
        let _ = std::fs::create_dir_all(&cache);
        let (tx, rx) = mpsc::channel();
        let status = Arc::new(Mutex::new(AudioStatus::default()));
        let tap = Arc::new(Mutex::new(VecDeque::with_capacity(8192)));
        let loaded = Arc::new(Mutex::new(HashMap::new()));
        let engine = Self {
            tx: tx.clone(),
            status: status.clone(),
            tap: tap.clone(),
            cache: cache.clone(),
            loaded: loaded.clone(),
        };
        std::thread::Builder::new()
            .name("wl-audio".into())
            .spawn(move || audio_thread(rx, tx, status, tap, cache, loaded))
            .ok();
        engine
    }

    pub fn status(&self) -> AudioStatus {
        self.status.lock().map(|s| s.clone()).unwrap_or_default()
    }

    pub fn play(&self, channel: &str, src: &str) {
        let _ = self.tx.send(Cmd::Play {
            channel: channel.into(),
            src: src.into(),
        });
    }

    pub fn load(&self, channel: &str, src: &str) {
        let _ = self.tx.send(Cmd::Load {
            channel: channel.into(),
            src: src.into(),
        });
    }

    pub fn pause(&self, channel: &str) {
        let _ = self.tx.send(Cmd::Pause {
            channel: channel.into(),
        });
    }

    pub fn seek(&self, channel: &str, position: f64) {
        let _ = self.tx.send(Cmd::Seek {
            channel: channel.into(),
            position,
        });
    }

    pub fn stop(&self, channel: &str) {
        let _ = self.tx.send(Cmd::Stop {
            channel: channel.into(),
        });
    }

    pub fn set_volume(&self, volume: f32) {
        let _ = self.tx.send(Cmd::Volume(volume.clamp(0.0, 1.0)));
    }

    pub fn set_rate(&self, rate: f32) {
        let _ = self.tx.send(Cmd::Rate(rate.clamp(0.25, 4.0)));
    }

    pub fn cached(&self, id: &str) -> Option<PathBuf> {
        self.loaded.lock().ok().and_then(|m| m.get(id).cloned()).or_else(|| {
            find_cached(&self.cache, id)
        })
    }

    pub fn cached_count(&self) -> usize {
        std::fs::read_dir(&self.cache)
            .map(|d| d.flatten().filter(|e| e.path().is_file()).count())
            .unwrap_or(0)
    }

    pub fn clear_cache(&self) {
        if let Ok(entries) = std::fs::read_dir(&self.cache) {
            for entry in entries.flatten() {
                let _ = std::fs::remove_file(entry.path());
            }
        }
        if let Ok(mut m) = self.loaded.lock() {
            m.clear();
        }
    }

    /// 256 log-spaced magnitudes (0..255) from the recent output samples.
    pub fn spectrum(&self) -> Vec<u8> {
        let samples: Vec<f32> = match self.tap.lock() {
            Ok(b) => b.iter().copied().collect(),
            Err(_) => Vec::new(),
        };
        const BINS: usize = 256;
        let mut out = vec![0u8; BINS];
        if samples.len() < 512 {
            return out;
        }
        let n = 2048usize;
        let start = samples.len().saturating_sub(n);
        let chunk = &samples[start..];
        let mut buf: Vec<rustfft::num_complex::Complex<f32>> = chunk
            .iter()
            .take(n)
            .enumerate()
            .map(|(i, v)| {
                let w = 0.5
                    - 0.5 * ((2.0 * std::f32::consts::PI * i as f32) / (n as f32 - 1.0)).cos();
                rustfft::num_complex::Complex::new(v * w, 0.0)
            })
            .collect();
        while buf.len() < n {
            buf.push(rustfft::num_complex::Complex::new(0.0, 0.0));
        }
        let mut planner = rustfft::FftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(n);
        fft.process(&mut buf);
        // Log frequency mapping so bass does not own the whole display.
        for (i, slot) in out.iter_mut().enumerate() {
            let lo = ((i as f32 / BINS as f32).powf(2.2) * (n as f32 / 2.0)) as usize;
            let hi = (((i + 1) as f32 / BINS as f32).powf(2.2) * (n as f32 / 2.0)) as usize;
            let hi = hi.max(lo + 1).min(n / 2);
            let mut mag = 0.0f32;
            for b in lo..hi {
                mag = mag.max(buf[b].norm());
            }
            let scaled = (mag * 22.0).sqrt() * 42.0;
            *slot = scaled.clamp(0.0, 255.0) as u8;
        }
        out
    }
}

fn audio_thread(
    rx: Receiver<Cmd>,
    tx: Sender<Cmd>,
    status: Arc<Mutex<AudioStatus>>,
    tap: Arc<Mutex<VecDeque<f32>>>,
    cache: PathBuf,
    loaded: Arc<Mutex<HashMap<String, PathBuf>>>,
) {
    let device: Option<MixerDeviceSink> = DeviceSinkBuilder::open_default_sink().ok();
    let handle = device.as_ref().map(|d| d.mixer().clone());
    let mut sink: Option<Player> = None;
    let mut started = false;
    let mut want_play = false;
    let mut volume = 1.0f32;
    let mut rate = 1.0f32;
    let mut prepared_paths: HashMap<String, PathBuf> = HashMap::new();
    let mut inflight: std::collections::HashSet<String> = std::collections::HashSet::new();

    if handle.is_none() {
        if let Ok(mut s) = status.lock() {
            s.error = "No audio output device".into();
        }
    }

    loop {
        match rx.recv_timeout(Duration::from_millis(120)) {
            Ok(cmd) => match cmd {
                Cmd::Play { channel, src } => {
                    let same = status
                        .lock()
                        .map(|s| s.src == src && s.channel == channel && !s.ended)
                        .unwrap_or(false);
                    if same {
                        if let Some(s) = &sink {
                            s.play();
                            want_play = true;
                            if let Ok(mut st) = status.lock() {
                                st.playing = true;
                                st.paused = false;
                                st.loading = false;
                            }
                            continue;
                        }
                    }
                    if let Ok(mut s) = status.lock() {
                        s.channel = channel.clone();
                        s.src = src.clone();
                        s.track_id = track_id(&src);
                        s.loading = true;
                        s.error.clear();
                        s.ended = false;
                        s.playing = false;
                        s.paused = false;
                        s.position = 0.0;
                        s.duration = 0.0;
                    }
                    want_play = true;
                    if let Some(path) = prepared_paths.get(&src).cloned() {
                        start_sink(&handle, &mut sink, &mut started, &path, &tap, &status, &mut volume, &mut rate);
                    } else if inflight.insert(src.clone()) {
                        spawn_fetch(tx.clone(), cache.clone(), src, loaded.clone());
                    }
                }
                Cmd::Load { channel: _channel, src } => {
                    if let Some(path) = prepared_paths.get(&src).cloned() {
                        if let Ok(mut m) = loaded.lock() {
                            m.insert(track_id(&src), path.clone());
                        }
                    } else if inflight.insert(src.clone()) {
                        spawn_fetch(tx.clone(), cache.clone(), src, loaded.clone());
                    }
                }
                Cmd::Pause { channel } => {
                    if let Ok(mut s) = status.lock() {
                        if !s.channel.is_empty() && s.channel != channel {
                            continue;
                        }
                        s.paused = true;
                        s.playing = false;
                    }
                    want_play = false;
                    if let Some(s) = &sink {
                        s.pause();
                    }
                }
                Cmd::Seek { channel, position } => {
                    if let Some(s) = &sink {
                        let active = status
                            .lock()
                            .map(|st| st.channel == channel || st.channel.is_empty())
                            .unwrap_or(false);
                        if active {
                            let _ = s.try_seek(Duration::from_secs_f64(position.max(0.0)));
                            if let Ok(mut st) = status.lock() {
                                st.position = position.max(0.0);
                            }
                        }
                    }
                }
                Cmd::Stop { channel } => {
                    let active = status
                        .lock()
                        .map(|st| st.channel.is_empty() || st.channel == channel)
                        .unwrap_or(true);
                    if active {
                        sink = None;
                        started = false;
                        want_play = false;
                        if let Ok(mut s) = status.lock() {
                            s.playing = false;
                            s.paused = true;
                            s.position = 0.0;
                        }
                    }
                }
                Cmd::Volume(v) => {
                    volume = v;
                    if let Some(s) = &sink {
                        s.set_volume(v);
                    }
                    if let Ok(mut st) = status.lock() {
                        st.volume = v;
                    }
                }
                Cmd::Rate(r) => {
                    rate = r;
                    if let Some(s) = &sink {
                        s.set_speed(r);
                    }
                    if let Ok(mut st) = status.lock() {
                        st.rate = r;
                    }
                }
                Cmd::Ready { src, path, error } => {
                    inflight.remove(&src);
                    if !error.is_empty() {
                        let active = status
                            .lock()
                            .map(|st| st.src == src)
                            .unwrap_or(false);
                        if active {
                            if let Ok(mut s) = status.lock() {
                                s.loading = false;
                                s.playing = false;
                                s.paused = true;
                                s.error = error;
                            }
                            want_play = false;
                        }
                        continue;
                    }
                    let Some(path) = path else { continue };
                    prepared_paths.insert(src.clone(), path.clone());
                    if let Ok(mut m) = loaded.lock() {
                        m.insert(track_id(&src), path.clone());
                    }
                    let active = status
                        .lock()
                        .map(|st| st.src == src)
                        .unwrap_or(false);
                    if active && want_play {
                        start_sink(&handle, &mut sink, &mut started, &path, &tap, &status, &mut volume, &mut rate);
                    }
                }
            },
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }

        if let Some(s) = &sink {
            let paused = s.is_paused();
            let empty = s.empty();
            let pos = s.get_pos().as_secs_f64();
            let finished = started && empty && !paused && want_play;
            if let Ok(mut st) = status.lock() {
                if st.channel.is_empty() {
                    continue;
                }
                if pos > st.position || !paused {
                    st.position = pos;
                }
                st.paused = paused;
                st.playing = !paused && !empty;
                if finished {
                    st.playing = false;
                    st.ended = true;
                    st.paused = true;
                    st.position = st.duration.max(pos);
                }
            }
            if finished {
                started = false;
                want_play = false;
            }
        }
    }
    drop(device);
}

fn start_sink(
    handle: &Option<Mixer>,
    sink: &mut Option<Player>,
    started: &mut bool,
    path: &Path,
    tap: &Arc<Mutex<VecDeque<f32>>>,
    status: &Arc<Mutex<AudioStatus>>,
    volume: &mut f32,
    rate: &mut f32,
) {
    let Some(handle) = handle else { return };
    *sink = None;
    let Ok(file) = File::open(path) else {
        if let Ok(mut s) = status.lock() {
            s.loading = false;
            s.error = "Could not open audio file".into();
        }
        return;
    };
    let Ok(decoder) = Decoder::try_from(file) else {
        if let Ok(mut s) = status.lock() {
            s.loading = false;
            s.error = "Unsupported audio format".into();
        }
        return;
    };
    let duration = decoder
        .total_duration()
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let player = Player::connect_new(handle);
    player.set_volume(*volume);
    player.set_speed(*rate);
    player.append(Tap::new(decoder, tap.clone()));
    *sink = Some(player);
    *started = true;
    if let Ok(mut s) = status.lock() {
        s.loading = false;
        s.playing = true;
        s.paused = false;
        s.ended = false;
        s.duration = duration;
        s.position = 0.0;
    }
}

fn spawn_fetch(
    tx: Sender<Cmd>,
    cache: PathBuf,
    src: String,
    loaded: Arc<Mutex<HashMap<String, PathBuf>>>,
) {
    std::thread::spawn(move || {
        let id = track_id(&src);
        let result = if let Some(hit) = find_cached(&cache, &id) {
            Ok(hit)
        } else {
            fetch_to_cache(&cache, &src, &id)
        };
        match result {
            Ok(path) => {
                if let Ok(mut m) = loaded.lock() {
                    m.insert(id.clone(), path.clone());
                }
                let _ = tx.send(Cmd::Ready {
                    src,
                    path: Some(path),
                    error: String::new(),
                });
            }
            Err(e) => {
                let _ = tx.send(Cmd::Ready {
                    src,
                    path: None,
                    error: e,
                });
            }
        }
    });
}

fn fetch_to_cache(cache: &Path, src: &str, id: &str) -> Result<PathBuf, String> {
    // YouTube links are fetched with yt-dlp itself: direct googlevideo URLs
    // are bound to the client/UA that requested them and 403 otherwise.
    if let Some(rest) = src.split("/api/stream/").nth(1) {
        let yt_id = sanitize(rest.split('?').next().unwrap_or(""));
        if !yt_id.is_empty() {
            if let Some(hit) = find_cached(cache, &yt_id) {
                return Ok(hit);
            }
            let template = cache.join(format!("{yt_id}.%(ext)s"));
            let out = std::process::Command::new(crate::api::ytdlp_bin())
                .args([
                    "-f",
                    "bestaudio[ext=m4a]/bestaudio[acodec^=mp4a]/bestaudio",
                    "--no-playlist",
                    "--no-warnings",
                    "--socket-timeout",
                    "20",
                    "-o",
                ])
                .arg(&template)
                .arg(format!("https://www.youtube.com/watch?v={yt_id}"))
                .output()
                .map_err(|e| format!("yt-dlp: {e}"))?;
            if !out.status.success() {
                return Err(String::from_utf8_lossy(&out.stderr)
                    .lines()
                    .last()
                    .unwrap_or("yt-dlp could not download the audio")
                    .trim_start_matches("ERROR: ")
                    .to_string());
            }
            return find_cached(cache, &yt_id)
                .ok_or_else(|| "yt-dlp finished without an audio file".to_string());
        }
    }

    let url = if src.starts_with("http://") || src.starts_with("https://") {
        src.to_string()
    } else {
        format!("{}{}", crate::api::BASE_URL, src)
    };
    let client = reqwest::blocking::Client::builder()
        .user_agent("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Safari/605.1.15")
        .redirect(reqwest::redirect::Policy::limited(10))
        .timeout(Duration::from_secs(90))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client
        .get(&url)
        .header(reqwest::header::REFERER, "https://www.youtube.com/")
        .send()
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("audio HTTP {}", resp.status()));
    }
    let ctype = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let ext = if ctype.contains("mp4") || ctype.contains("m4a") || ctype.contains("aac") {
        "m4a"
    } else if ctype.contains("mpeg") || ctype.contains("mp3") {
        "mp3"
    } else if ctype.contains("webm") {
        "webm"
    } else if ctype.contains("ogg") || ctype.contains("opus") {
        "ogg"
    } else if ctype.contains("flac") {
        "flac"
    } else {
        "audio"
    };
    let final_path = cache.join(format!("{id}.{ext}"));
    let part = cache.join(format!("{id}.{ext}.part"));
    let mut file = File::create(&part).map_err(|e| e.to_string())?;
    let mut resp = resp;
    std::io::copy(&mut resp, &mut file).map_err(|e| e.to_string())?;
    drop(file);
    std::fs::rename(&part, &final_path).map_err(|e| e.to_string())?;
    Ok(final_path)
}

pub fn track_id(src: &str) -> String {
    let path = src.split('?').next().unwrap_or(src);
    if let Some(rest) = path.split("/api/stream/").nth(1) {
        return sanitize(rest);
    }
    let mut h = std::collections::hash_map::DefaultHasher::new();
    src.hash(&mut h);
    format!("{:x}", h.finish())
}

fn sanitize(v: &str) -> String {
    v.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(64)
        .collect()
}

fn find_cached(cache: &Path, id: &str) -> Option<PathBuf> {
    let entries = std::fs::read_dir(cache).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().map(|e| e == "part").unwrap_or(false) {
            continue;
        }
        let name = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        if name == id {
            return Some(path);
        }
    }
    None
}

struct Tap<S> {
    inner: S,
    buf: Arc<Mutex<VecDeque<f32>>>,
}

impl<S> Tap<S> {
    fn new(inner: S, buf: Arc<Mutex<VecDeque<f32>>>) -> Self {
        Self { inner, buf }
    }
}

impl<S: Source> Iterator for Tap<S> {
    type Item = f32;
    fn next(&mut self) -> Option<f32> {
        let sample = self.inner.next();
        if let Some(v) = sample {
            if let Ok(mut b) = self.buf.lock() {
                b.push_back(v);
                while b.len() > 8192 {
                    b.pop_front();
                }
            }
        }
        sample
    }
}

impl<S: Source> Source for Tap<S> {
    fn current_span_len(&self) -> Option<usize> {
        self.inner.current_span_len()
    }
    fn channels(&self) -> rodio::ChannelCount {
        self.inner.channels()
    }
    fn sample_rate(&self) -> rodio::SampleRate {
        self.inner.sample_rate()
    }
    fn total_duration(&self) -> Option<Duration> {
        self.inner.total_duration()
    }
}
