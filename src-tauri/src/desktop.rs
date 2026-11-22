//! Desktop shell integration the page cannot do on its own.
//!
//! * The window frame follows the app's light/dark mode (on Windows 11 this is the title bar).
//! * System media controls: media keys, the Windows volume/lock-screen media card, MPRIS on Linux. The audio engine
//!   lives in this process, so the webview has no playing media of its own and the browser's Media Session never
//!   reaches the system. The page sends what is playing to `media_update`, and the system's button presses come back
//!   through `media_drain`, which the page polls and turns into its own play/pause/next/previous.

use serde::Deserialize;
use std::sync::{Mutex, OnceLock};

static SHELL: OnceLock<tauri::AppHandle> = OnceLock::new();
static EVENTS: Mutex<Vec<String>> = Mutex::new(Vec::new());
static MEDIA: OnceLock<Mutex<std::sync::mpsc::Sender<MediaState>>> = OnceLock::new();

#[derive(Deserialize, Clone, Default, Debug)]
#[serde(default)]
pub struct MediaState {
    pub has: bool,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub cover: String,
    pub duration: f64,
    pub position: f64,
    pub playing: bool,
}

pub fn push_event(e: String) {
    if let Ok(mut q) = EVENTS.lock() {
        if q.len() < 32 {
            q.push(e);
        }
    }
}

/// Button presses since the last call.
pub fn media_drain() -> Vec<String> {
    EVENTS.lock().map(|mut q| std::mem::take(&mut *q)).unwrap_or_default()
}

pub fn media_update(state: MediaState) {
    if let Some(tx) = MEDIA.get() {
        if let Ok(tx) = tx.lock() {
            let _ = tx.send(state);
        }
    }
}

/// Called once after the window exists. `hwnd` is the window handle on Windows (the system media card needs it).
pub fn init(handle: tauri::AppHandle, hwnd: Option<*mut std::ffi::c_void>) {
    let _ = SHELL.set(handle);
    #[cfg(any(windows, target_os = "linux"))]
    {
        if let Some(tx) = sys::start(hwnd) {
            let _ = MEDIA.set(Mutex::new(tx));
        }
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        let _ = hwnd;
    }
}

pub fn set_theme(dark: bool) {
    use tauri::Manager;
    if let Some(h) = SHELL.get() {
        if let Some(w) = h.get_webview_window("main") {
            let _ = w.set_theme(Some(if dark { tauri::Theme::Dark } else { tauri::Theme::Light }));
        }
    }
}

#[cfg(any(windows, target_os = "linux"))]
mod sys {
    use super::*;
    use souvlaki::{MediaControlEvent, MediaControls, MediaMetadata, MediaPlayback, MediaPosition, PlatformConfig, SeekDirection};
    use std::time::Duration;

    pub fn start(hwnd: Option<*mut std::ffi::c_void>) -> Option<std::sync::mpsc::Sender<MediaState>> {
        let (tx, rx) = std::sync::mpsc::channel::<MediaState>();
        // Raw pointers are not Send; the thread rebuilds it from the number.
        let hwnd_bits = hwnd.map(|p| p as usize);
        std::thread::Builder::new()
            .name("wavelength-media".into())
            .spawn(move || {
                let cfg = PlatformConfig {
                    dbus_name: "wavelength",
                    display_name: "Wavelength",
                    hwnd: hwnd_bits.map(|b| b as *mut std::ffi::c_void),
                };
                let mut controls = match MediaControls::new(cfg) {
                    Ok(c) => c,
                    Err(e) => {
                        eprintln!("wavelength: system media controls unavailable: {e:?}");
                        return;
                    }
                };
                let attached = controls.attach(|ev: MediaControlEvent| {
                    let dir = |d: SeekDirection| if matches!(d, SeekDirection::Forward) { "+" } else { "-" };
                    let name = match ev {
                        MediaControlEvent::Play => "play".to_string(),
                        MediaControlEvent::Pause => "pause".to_string(),
                        MediaControlEvent::Toggle => "toggle".to_string(),
                        MediaControlEvent::Next => "next".to_string(),
                        MediaControlEvent::Previous => "prev".to_string(),
                        MediaControlEvent::Stop => "stop".to_string(),
                        MediaControlEvent::Seek(d) => format!("seek:{}10", dir(d)),
                        MediaControlEvent::SeekBy(d, by) => format!("seek:{}{}", dir(d), by.as_secs_f64().round()),
                        MediaControlEvent::SetPosition(MediaPosition(p)) => format!("pos:{}", p.as_secs_f64()),
                        _ => return,
                    };
                    push_event(name);
                });
                if let Err(e) = attached {
                    eprintln!("wavelength: system media controls could not attach: {e:?}");
                    return;
                }
                let mut shown = String::new();
                for st in rx {
                    let dur = |s: f64| Duration::from_secs_f64(if s.is_finite() && s > 0.0 { s } else { 0.0 });
                    if !st.has {
                        shown.clear();
                        let _ = controls.set_playback(MediaPlayback::Stopped);
                        continue;
                    }
                    let key = format!("{}|{}|{}|{}", st.title, st.artist, st.cover, st.duration as u64);
                    if key != shown {
                        shown = key;
                        let _ = controls.set_metadata(MediaMetadata {
                            title: Some(&st.title),
                            artist: Some(&st.artist),
                            album: Some(&st.album),
                            cover_url: if st.cover.starts_with("http") { Some(&st.cover) } else { None },
                            duration: if st.duration > 0.0 { Some(dur(st.duration)) } else { None },
                        });
                    }
                    let progress = Some(MediaPosition(dur(st.position)));
                    let _ = controls.set_playback(if st.playing {
                        MediaPlayback::Playing { progress }
                    } else {
                        MediaPlayback::Paused { progress }
                    });
                }
            })
            .ok()?;
        Some(tx)
    }
}
