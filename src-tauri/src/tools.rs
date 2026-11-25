//! Helper programs (yt-dlp, ffmpeg, ffprobe).
//!
//! macOS and Linux keep using whatever is installed (Homebrew, a package manager, the Flatpak).
//! Windows has no package manager that every PC has, so Wavelength looks after the tools itself: on first start it
//! downloads `yt-dlp.exe`, `ffmpeg.exe` and `ffprobe.exe` into `<data folder>\tools` and keeps yt-dlp up to date
//! (YouTube changes often and an old yt-dlp stops working). Nothing needs to be installed by hand and nothing
//! outside the Wavelength data folder is touched. Only built-in Windows parts are used: PowerShell, which ships
//! with Windows 10 and 11, unpacks the ffmpeg zip.

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime};

const YTDLP_URL: &str = "https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp.exe";
/// yt-dlp's own ffmpeg builds (the ones its documentation points to). The zip holds `<folder>/bin/ffmpeg.exe`
/// and `<folder>/bin/ffprobe.exe`.
const FFMPEG_URL: &str =
    "https://github.com/yt-dlp/FFmpeg-Builds/releases/latest/download/ffmpeg-master-latest-win64-gpl.zip";
/// Anything smaller than this is an error page, not the program.
const YTDLP_MIN_BYTES: u64 = 5 * 1024 * 1024;
const FFMPEG_MIN_BYTES: u64 = 20 * 1024 * 1024;
/// How often a downloaded yt-dlp asks itself for an update.
const UPDATE_EVERY: Duration = Duration::from_secs(24 * 60 * 60);

static TOOLS_DIR: OnceLock<PathBuf> = OnceLock::new();
static RUNNING: AtomicBool = AtomicBool::new(false);
static STATUS: Mutex<Status> = Mutex::new(Status::new());

struct Status {
    state: &'static str,
    message: String,
}

impl Status {
    const fn new() -> Self {
        Status { state: "idle", message: String::new() }
    }
}

fn set_status(state: &'static str, message: impl Into<String>) {
    let mut s = STATUS.lock().unwrap_or_else(|e| e.into_inner());
    s.state = state;
    s.message = message.into();
}

/// `{ "state": "idle" | "downloading" | "ready" | "error", "message": "…" }` for the health endpoint.
pub fn status_json() -> Value {
    let s = STATUS.lock().unwrap_or_else(|e| e.into_inner());
    json!({ "state": s.state, "message": s.message })
}

/// Remembers where the tools live. Called once from `boot` with the data folder.
pub fn init(data_dir: &Path) {
    let _ = TOOLS_DIR.set(data_dir.join("tools"));
}

/// `yt-dlp` becomes `yt-dlp.exe` on Windows.
pub fn exe_name(name: &str) -> String {
    if cfg!(windows) && !name.to_ascii_lowercase().ends_with(".exe") {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

/// The copy Wavelength manages itself, if it has one.
pub fn managed_path(name: &str) -> Option<PathBuf> {
    let p = TOOLS_DIR.get()?.join(exe_name(name));
    p.is_file().then_some(p)
}

/// A `Command` that never flashes a console window when the app is a GUI program on Windows.
pub fn command(program: impl AsRef<std::ffi::OsStr>) -> Command {
    #[allow(unused_mut)]
    let mut c = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        c.creation_flags(CREATE_NO_WINDOW);
    }
    c
}

/// Starts the one-time setup in the background. Safe to call again (a second call while one is running does nothing).
/// Does nothing except on Windows.
pub fn ensure_async() {
    if !cfg!(windows) {
        return;
    }
    if RUNNING.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(|| {
        ensure();
        RUNNING.store(false, Ordering::SeqCst);
    });
}

fn have(name: &str) -> bool {
    crate::api::find_bin(name).is_some()
}

fn ensure() {
    let Some(dir) = TOOLS_DIR.get() else { return };
    if let Err(e) = std::fs::create_dir_all(dir) {
        set_status("error", format!("Could not create {}: {e}", dir.display()));
        return;
    }
    // Leftovers from an interrupted download.
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if matches!(p.extension().and_then(|x| x.to_str()), Some("part") | Some("new")) {
                let _ = std::fs::remove_file(p);
            }
        }
    }

    let mut failure: Option<String> = None;

    if !have("yt-dlp") {
        set_status("downloading", "Downloading yt-dlp (one time)…");
        let dest = dir.join(exe_name("yt-dlp"));
        if let Err(e) = download(YTDLP_URL, &dest, YTDLP_MIN_BYTES) {
            failure = Some(format!("yt-dlp: {e}"));
        }
    }

    if !have("ffmpeg") || !have("ffprobe") {
        set_status("downloading", "Downloading ffmpeg (one time, about 150 MB)…");
        if let Err(e) = install_ffmpeg(dir) {
            // Playback works without ffmpeg, so this is reported but does not hide a working yt-dlp.
            failure = failure.or(Some(format!("ffmpeg: {e}")));
        }
    }

    match failure {
        Some(msg) => set_status("error", msg),
        None => set_status("ready", ""),
    }

    update_ytdlp(dir);
}

fn download(url: &str, dest: &Path, min_bytes: u64) -> Result<(), String> {
    let client = reqwest::blocking::Client::builder()
        .user_agent("Wavelength")
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(1200))
        .build()
        .map_err(|e| e.to_string())?;
    let mut resp = client
        .get(url)
        .send()
        .map_err(|e| format!("could not download ({e}). Check the internet connection"))?;
    if !resp.status().is_success() {
        return Err(format!("could not download (HTTP {})", resp.status().as_u16()));
    }
    let part = dest.with_extension("part");
    {
        let mut f = std::fs::File::create(&part).map_err(|e| e.to_string())?;
        std::io::copy(&mut resp, &mut f).map_err(|e| format!("download was interrupted ({e})"))?;
    }
    let size = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
    if size < min_bytes {
        let _ = std::fs::remove_file(&part);
        return Err("the download was incomplete".into());
    }
    std::fs::rename(&part, dest).map_err(|e| e.to_string())
}

fn install_ffmpeg(dir: &Path) -> Result<(), String> {
    let zip = dir.join("ffmpeg.zip");
    download(FFMPEG_URL, &zip, FFMPEG_MIN_BYTES)?;
    let result = extract_ffmpeg(&zip, dir);
    let _ = std::fs::remove_file(&zip);
    result?;
    for name in ["ffmpeg", "ffprobe"] {
        let exe = dir.join(exe_name(name));
        let new = dir.join(format!("{}.new", exe_name(name)));
        if !new.is_file() {
            return Err(format!("{name} was not in the download"));
        }
        std::fs::rename(&new, &exe).map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[cfg(windows)]
fn extract_ffmpeg(zip: &Path, dir: &Path) -> Result<(), String> {
    // Pulls just the two programs out of the zip. Paths travel in environment variables so no quoting can go wrong.
    let script = "Add-Type -AssemblyName System.IO.Compression.FileSystem; \
        $z = [System.IO.Compression.ZipFile]::OpenRead($env:WL_ZIP); \
        try { foreach ($e in $z.Entries) { \
        if ($e.FullName -match '/bin/(ffmpeg|ffprobe)\\.exe$') { \
        [System.IO.Compression.ZipFileExtensions]::ExtractToFile($e, (Join-Path $env:WL_DEST ($e.Name + '.new')), $true) } } } \
        finally { $z.Dispose() }";
    let root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into());
    let full = PathBuf::from(root).join("System32\\WindowsPowerShell\\v1.0\\powershell.exe");
    let ps = if full.is_file() { full } else { PathBuf::from("powershell") };
    let out = command(ps)
        .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", script])
        .env("WL_ZIP", zip)
        .env("WL_DEST", dir)
        .output()
        .map_err(|e| format!("could not unpack the download ({e})"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!(
            "could not unpack the download ({})",
            err.lines().find(|l| !l.trim().is_empty()).unwrap_or("PowerShell failed").trim()
        ));
    }
    Ok(())
}

#[cfg(not(windows))]
fn extract_ffmpeg(_zip: &Path, _dir: &Path) -> Result<(), String> {
    Err("automatic ffmpeg setup is only used on Windows".into())
}

/// yt-dlp stops working when YouTube changes; the standalone exe can update itself in place. Checked once a day.
fn update_ytdlp(dir: &Path) {
    let Some(bin) = managed_path("yt-dlp") else { return };
    let stamp = dir.join("yt-dlp.checked");
    let fresh = std::fs::metadata(&stamp)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .map(|age| age < UPDATE_EVERY)
        .unwrap_or(false);
    if fresh {
        return;
    }
    let child = command(&bin)
        .arg("-U")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    if let Ok(mut child) = child {
        let start = std::time::Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if start.elapsed() < Duration::from_secs(90) => {
                    std::thread::sleep(Duration::from_millis(250));
                }
                _ => {
                    let _ = child.kill();
                    break;
                }
            }
        }
    }
    let _ = std::fs::write(&stamp, b"");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exe_name_adds_suffix_only_on_windows() {
        if cfg!(windows) {
            assert_eq!(exe_name("yt-dlp"), "yt-dlp.exe");
            assert_eq!(exe_name("ffmpeg.exe"), "ffmpeg.exe");
        } else {
            assert_eq!(exe_name("yt-dlp"), "yt-dlp");
        }
    }

    #[test]
    fn status_reports_state_and_message() {
        set_status("downloading", "Downloading yt-dlp");
        let v = status_json();
        assert_eq!(v["state"], "downloading");
        assert_eq!(v["message"], "Downloading yt-dlp");
        set_status("idle", "");
    }
}
