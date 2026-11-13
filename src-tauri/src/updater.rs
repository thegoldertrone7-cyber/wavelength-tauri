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
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
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
    /// A downloaded and signature-verified bundle waiting for a restart.
    staged: Mutex<Option<(String, Vec<u8>)>>,
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
            .map(|(v, _)| v.clone())
            .unwrap_or_default();
        let error = lock(&self.error).clone();
        json!({
            "ok": true,
            "native": true,
            "running": env!("CARGO_PKG_VERSION"),
            "checkedAt": self.checked_at.load(Ordering::SeqCst),
            "staged": staged,
            "error": error,
            "rolledBack": false,
            "available": found.map(|u| json!({
                "version": u.version,
                "notes": u.body.clone().unwrap_or_default(),
                "needsInstaller": false,
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
        let updater = match handle.updater() {
            Ok(u) => u,
            Err(e) => {
                self.set_error(&format!("Updater unavailable: {e}"));
                return;
            }
        };
        match updater.check().await {
            Ok(found) => {
                *lock(&self.found) = found;
                self.set_error("");
            }
            Err(e) => self.set_error(&format!("Could not check for updates: {e}")),
        }
        self.checked_at.store(now_ms(), Ordering::SeqCst);
    }

    /// Downloads the update found by the last check and verifies its signature.
    pub async fn download(&self) -> Result<(), ApiError> {
        let found = lock(&self.found).clone();
        let Some(update) = found else {
            return Err(fail(StatusCode::BAD_REQUEST, "Check for updates first"));
        };
        if self.busy.swap(true, Ordering::SeqCst) {
            return Err(fail(StatusCode::CONFLICT, "An update is already downloading"));
        }
        let result = update.download(|_chunk, _total| {}, || {}).await;
        self.busy.store(false, Ordering::SeqCst);
        match result {
            Ok(bytes) => {
                *lock(&self.staged) = Some((update.version.clone(), bytes));
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

    /// Installs the staged bundle and relaunches. On Windows the installer
    /// ends the process itself, so the relaunch is done by the installer.
    pub fn restart(&self) -> Value {
        let found = lock(&self.found).clone();
        let handle = lock(&self.handle).clone();
        let staged = lock(&self.staged).take();
        let (Some((_, bytes)), Some(update), Some(handle)) = (staged, found, handle) else {
            return json!({ "ok": false, "manual": true });
        };
        std::thread::spawn(move || {
            // Let the HTTP reply reach the page before the app goes away.
            std::thread::sleep(Duration::from_millis(400));
            match update.install(&bytes) {
                Ok(()) => handle.restart(),
                Err(e) => eprintln!("wavelength update: install failed: {e}"),
            }
        });
        json!({ "ok": true })
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
    Json(st.updater.restart())
}
