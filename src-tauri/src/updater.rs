//! In-app updates from GitHub Releases.
//!
//! Release builds are signed in CI and published with a `latest.json` manifest
//! (see `.github/workflows/release.yml`). The Tauri updater plugin reads that
//! manifest from the endpoint in `tauri.conf.json`, verifies the signature
//! against the public key in the same file, and installs the platform bundle.
//!
//! The Settings → Updates page was written for the old web-layer updater and
//! calls four routes. They are kept, and answered here, so the UI needed no
//! changes:
//!
//!   GET  /api/update/status    current state, no network
//!   POST /api/update/check     ask GitHub whether a newer release exists
//!   POST /api/update/install   download and verify it ("Version X is ready")
//!   POST /api/update/restart   install it and relaunch
//!
//! If `tauri.conf.json` has no public key (a local build) the plugin is not
//! registered at all and these routes report that updates are not set up,
//! rather than the app failing to start.

use crate::api::{ApiError, AppState};
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tauri_plugin_updater::{Update, UpdaterExt};

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Where a person can always get the newest installer by hand.
const RELEASES: &str = "https://github.com/thegoldertrone7-cyber/wavelength-tauri/releases/latest";

/// How this copy of the app was installed decides whether it can replace itself.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Desktop,
    AppImage,
    Package,
    Flatpak,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::Desktop => "desktop",
            Kind::AppImage => "appimage",
            Kind::Package => "package",
            Kind::Flatpak => "flatpak",
        }
    }
    /// A Flatpak is read-only and updated by Flatpak, so the app must not try to overwrite itself.
    fn can_install(self) -> bool {
        self != Kind::Flatpak
    }
}

#[cfg(target_os = "linux")]
fn install_kind() -> Kind {
    if std::env::var_os("FLATPAK_ID").is_some() || std::path::Path::new("/.flatpak-info").exists() {
        Kind::Flatpak
    } else if std::env::var_os("APPIMAGE").is_some() {
        Kind::AppImage
    } else {
        Kind::Package
    }
}

#[cfg(not(target_os = "linux"))]
fn install_kind() -> Kind {
    Kind::Desktop
}

fn fail(code: StatusCode, msg: impl Into<String>) -> ApiError {
    ApiError(code, msg.into())
}

/// True when `tauri.conf.json` carries a real updater public key.
pub fn configured(cfg: &tauri::Config) -> bool {
    cfg.plugins
        .0
        .get("updater")
        .and_then(|u| u.get("pubkey"))
        .and_then(|k| k.as_str())
        .map_or(false, |k| {
            let k = k.trim();
            k.len() > 40 && !k.contains("REPLACE")
        })
}

pub struct Updates {
    handle: Mutex<Option<tauri::AppHandle>>,
    configured: AtomicBool,
    /// The newest release found by the last check, not yet downloaded.
    found: Mutex<Option<Update>>,
    /// A downloaded and signature-verified bundle waiting for a restart, with the release it belongs to.
    staged: Mutex<Option<(Update, Vec<u8>)>>,
    /// Download progress in bytes, for the Settings page.
    done: AtomicU64,
    total: AtomicU64,
    checked_at: AtomicI64,
    error: Mutex<String>,
    busy: AtomicBool,
}

impl Default for Updates {
    fn default() -> Self {
        Self::new()
    }
}

impl Updates {
    pub fn new() -> Self {
        Self {
            handle: Mutex::new(None),
            configured: AtomicBool::new(false),
            found: Mutex::new(None),
            staged: Mutex::new(None),
            done: AtomicU64::new(0),
            total: AtomicU64::new(0),
            checked_at: AtomicI64::new(0),
            error: Mutex::new(String::new()),
            busy: AtomicBool::new(false),
        }
    }

    /// Called once the desktop shell is up. Headless runs never call this.
    pub fn attach(&self, handle: tauri::AppHandle, configured: bool) {
        *lock(&self.handle) = Some(handle);
        self.configured.store(configured, Ordering::SeqCst);
    }

    fn set_error(&self, msg: &str) {
        *lock(&self.error) = msg.to_string();
    }

    /// The JSON the Settings page reads.
    pub fn view(&self) -> Value {
        let found = lock(&self.found).clone();
        let staged = lock(&self.staged)
            .as_ref()
            .map(|(u, _)| u.version.clone())
            .unwrap_or_default();
        let error = lock(&self.error).clone();
        let kind = install_kind();
        let busy = self.busy.load(Ordering::SeqCst);
        json!({
            "ok": true,
            "native": true,
            "running": env!("CARGO_PKG_VERSION"),
            "checkedAt": self.checked_at.load(Ordering::SeqCst),
            "staged": staged,
            "error": error,
            "rolledBack": false,
            "how": kind.name(),
            "canInstall": kind.can_install(),
            "releaseUrl": RELEASES,
            "progress": { "busy": busy, "done": self.done.load(Ordering::SeqCst), "total": self.total.load(Ordering::SeqCst) },
            "available": found.map(|u| json!({
                "version": u.version,
                "notes": u.body.clone().unwrap_or_default(),
                "needsInstaller": !kind.can_install(),
            })),
        })
    }

    pub async fn check(&self) {
        let handle = lock(&self.handle).clone();
        let Some(handle) = handle else {
            self.set_error("Updates are only available in the desktop app");
            return;
        };
        if !self.configured.load(Ordering::SeqCst) {
            self.set_error("Updates are not set up in this build");
            return;
        }
        let updater = match handle.updater_builder().timeout(Duration::from_secs(30)).build() {
            Ok(u) => u,
            Err(e) => {
                self.set_error(&format!("Updater unavailable: {e}"));
                return;
            }
        };
        match updater.check().await {
            Ok(found) => {
                // A newer check can only replace what was found; a bundle already staged for the old find stays usable.
                *lock(&self.found) = found;
                self.set_error("");
            }
            Err(e) => self.set_error(&format!("Could not check for updates: {e}")),
        }
        self.checked_at.store(now_ms(), Ordering::SeqCst);
    }

    /// Downloads the update found by the last check and verifies its signature.
    pub async fn download(&self) -> Result<(), ApiError> {
        if !install_kind().can_install() {
            return Err(fail(
                StatusCode::BAD_REQUEST,
                "This Flatpak install is updated by downloading the new Flatpak from the releases page",
            ));
        }
        let mut found = lock(&self.found).clone();
        if found.is_none() {
            self.check().await;
            found = lock(&self.found).clone();
        }
        let Some(update) = found else {
            return Err(fail(StatusCode::BAD_REQUEST, "You are already on the newest version"));
        };
        if lock(&self.staged).as_ref().map_or(false, |(u, _)| u.version == update.version) {
            return Ok(());
        }
        if self.busy.swap(true, Ordering::SeqCst) {
            return Err(fail(StatusCode::CONFLICT, "An update is already downloading"));
        }
        self.done.store(0, Ordering::SeqCst);
        self.total.store(0, Ordering::SeqCst);
        let result = update
            .download(
                |chunk, total| {
                    self.done.fetch_add(chunk as u64, Ordering::SeqCst);
                    if let Some(t) = total {
                        self.total.store(t, Ordering::SeqCst);
                    }
                },
                || {},
            )
            .await;
        self.busy.store(false, Ordering::SeqCst);
        match result {
            Ok(bytes) => {
                *lock(&self.staged) = Some((update, bytes));
                self.set_error("");
                Ok(())
            }
            Err(e) => {
                let msg = format!("Download failed: {e}");
                self.set_error(&msg);
                Err(fail(StatusCode::BAD_GATEWAY, msg))
            }
        }
    }

    /// Installs the staged bundle and relaunches. The install runs to the end before the page is answered, so a
    /// failure is reported (and the bundle kept for another try) instead of vanishing. On Windows the installer ends
    /// this process itself, so no reply is sent and the page treats the dropped connection as "installing".
    pub async fn restart(&self) -> Value {
        let handle = lock(&self.handle).clone();
        let staged = lock(&self.staged).take();
        let (Some((update, bytes)), Some(handle)) = (staged, handle) else {
            return json!({ "ok": false, "error": "Download the update first", "releaseUrl": RELEASES });
        };
        let ran = tokio::task::spawn_blocking(move || {
            let r = update.install(&bytes).map_err(|e| e.to_string());
            (r, update, bytes)
        })
        .await;
        match ran {
            Ok((Ok(()), _, _)) => {
                std::thread::spawn(move || {
                    // Let the reply reach the page before the app goes away.
                    std::thread::sleep(Duration::from_millis(600));
                    handle.restart();
                });
                json!({ "ok": true, "restarting": true })
            }
            Ok((Err(e), update, bytes)) => {
                *lock(&self.staged) = Some((update, bytes));
                let msg = if install_kind() == Kind::Package {
                    format!("This install could not replace itself ({e}). Download the newest installer instead.")
                } else {
                    format!("The update could not be installed: {e}")
                };
                self.set_error(&msg);
                json!({ "ok": false, "error": msg, "releaseUrl": RELEASES })
            }
            Err(e) => {
                let msg = format!("The update could not be installed: {e}");
                self.set_error(&msg);
                json!({ "ok": false, "error": msg, "releaseUrl": RELEASES })
            }
        }
    }
}

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/update/status", get(status))
        .route("/api/update/check", post(check))
        .route("/api/update/install", post(install))
        .route("/api/update/restart", post(restart))
}

async fn status(State(st): State<Arc<AppState>>) -> Json<Value> {
    Json(st.updater.view())
}

async fn check(State(st): State<Arc<AppState>>) -> Json<Value> {
    st.updater.check().await;
    Json(st.updater.view())
}

async fn install(State(st): State<Arc<AppState>>) -> Result<Json<Value>, ApiError> {
    st.updater.download().await?;
    Ok(Json(st.updater.view()))
}

async fn restart(State(st): State<Arc<AppState>>) -> Json<Value> {
    Json(st.updater.restart().await)
}
