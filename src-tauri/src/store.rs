//! SQLite-backed application state.
//!
//! The Electron build kept `state.json` on disk. The Rust core keeps the same
//! JSON document in SQLite (and mirrors it to `state.json` for compatibility)
//! so the existing UI needs no changes. Listening stats are merged on every
//! write, which is what makes the recap correct across devices.

use rusqlite::{params, Connection};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const MAX_STAMP: f64 = 864e13;

pub struct Store {
    pub dir: PathBuf,
    conn: Mutex<Connection>,
}

pub fn stamp(v: Option<&Value>) -> i64 {
    let n = match v {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
        Some(Value::String(s)) => s.parse::<f64>().unwrap_or(0.0),
        _ => 0.0,
    };
    n.clamp(0.0, MAX_STAMP).round() as i64
}

/// Older builds clamped timestamps to 1e12, so snapshots from different
/// devices can tie. In that case keep the snapshot that contains more
/// listening time instead of dropping the phone's history.
fn listening_weight(d: &Value) -> f64 {
    let mut n = 0.0;
    if let Some(days) = d.get("days").and_then(|v| v.as_object()) {
        for v in days.values() {
            n += v.as_f64().unwrap_or(0.0);
        }
    }
    if let Some(years) = d.get("years").and_then(|v| v.as_object()) {
        for b in years.values() {
            n += b.get("seconds").and_then(|v| v.as_f64()).unwrap_or(0.0) * 2.0;
        }
    }
    n
}

pub fn merge_listening(current: &Value, incoming: &Value) -> Value {
    let mut devices = Map::new();
    for src in [current, incoming] {
        let Some(map) = src.get("devices").and_then(|v| v.as_object()) else {
            continue;
        };
        for (id, d) in map {
            if !d.is_object() {
                continue;
            }
            let mut d = d.clone();
            let at = stamp(d.get("updatedAt"));
            if let Some(obj) = d.as_object_mut() {
                obj.insert("updatedAt".into(), json!(at));
            }
            match devices.get(id) {
                None => {
                    devices.insert(id.clone(), d);
                }
                Some(have) => {
                    let old = stamp(have.get("updatedAt"));
                    if at > old || (at == old && listening_weight(&d) >= listening_weight(have)) {
                        devices.insert(id.clone(), d);
                    }
                }
            }
        }
    }
    json!({ "v": 1, "devices": devices })
}

impl Store {
    pub fn open(dir: &Path) -> Result<Self, String> {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        let conn = Connection::open(dir.join("wavelength.db")).map_err(|e| e.to_string())?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             -- A saved playlist must survive a power cut or a forced close, so
             -- wait for the write to reach the disk instead of batching it.
             PRAGMA synchronous=FULL;
             CREATE TABLE IF NOT EXISTS kv (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
        )
        .map_err(|e| e.to_string())?;
        // If the database lost its state (an interrupted write, a copied data
        // folder) the JSON mirror written next to it still has the library.
        let has_state: Option<String> = conn
            .query_row("SELECT value FROM kv WHERE key='state'", [], |r| r.get(0))
            .ok();
        if has_state.is_none() {
            if let Ok(raw) = std::fs::read(dir.join("state.json")) {
                if let Ok(value) = serde_json::from_slice::<Value>(&raw) {
                    if let Ok(raw) = serde_json::to_string(&value) {
                        let _ = conn.execute(
                            "INSERT OR REPLACE INTO kv (key, value) VALUES ('state', ?1)",
                            params![raw],
                        );
                    }
                }
            }
        }
        Ok(Self {
            dir: dir.to_path_buf(),
            conn: Mutex::new(conn),
        })
    }

    pub fn kv_get(&self, key: &str) -> Option<Value> {
        let conn = self.conn.lock().ok()?;
        let mut stmt = conn.prepare("SELECT value FROM kv WHERE key = ?1").ok()?;
        let raw: String = stmt.query_row(params![key], |r| r.get(0)).ok()?;
        serde_json::from_str(&raw).ok()
    }

    pub fn kv_set(&self, key: &str, value: &Value) -> Result<(), String> {
        let raw = serde_json::to_string(value).map_err(|e| e.to_string())?;
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        conn.execute(
            "INSERT INTO kv (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, raw],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn state(&self) -> Value {
        self.kv_get("state").unwrap_or_else(|| json!({}))
    }

    /// Save a full UI state blob, merging listening data this copy may not
    /// have seen yet (a delayed save must never erase a phone's stats).
    pub fn write_state(&self, incoming: Value) -> Result<Value, String> {
        let mut next = incoming;
        let prev = self.state();
        if next.get("listening").is_some() {
            let merged = merge_listening(
                prev.get("listening").unwrap_or(&Value::Null),
                next.get("listening").unwrap_or(&Value::Null),
            );
            if let Some(obj) = next.as_object_mut() {
                obj.insert("listening".into(), merged);
            }
        } else if let Some(l) = prev.get("listening") {
            if let Some(obj) = next.as_object_mut() {
                obj.insert("listening".into(), l.clone());
            }
        }
        self.kv_set("state", &next)?;
        // Write through a temp file so a crash mid-write cannot leave a
        // half-written mirror behind; the rename is atomic.
        let raw = serde_json::to_vec_pretty(&next).unwrap_or_default();
        let path = self.dir.join("state.json");
        let tmp = self.dir.join("state.json.tmp");
        if std::fs::write(&tmp, raw).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
        Ok(next)
    }

    /// Bring back a library left behind by a different install layout.
    ///
    /// The Flatpak and the deb/AppImage builds keep their data in different
    /// folders, and an old build may have used another name still. When this
    /// install starts with no playlists at all, look for the newest other
    /// folder that has some and adopt it (with its covers). Only an empty
    /// library is replaced, so this never brings back a playlist the user
    /// deleted while another install was running.
    pub fn recover_library(&self) -> Option<PathBuf> {
        let has_playlists = self
            .state()
            .get("playlists")
            .and_then(|v| v.as_array())
            .map(|a| !a.is_empty())
            .unwrap_or(false);
        if has_playlists {
            return None;
        }
        let mut best: Option<(std::time::SystemTime, PathBuf, Value)> = None;
        for dir in legacy_data_dirs() {
            if dir == self.dir {
                continue;
            }
            let Some(state) = state_from_dir(&dir) else { continue };
            let count = state
                .get("playlists")
                .and_then(|v| v.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            if count == 0 {
                continue;
            }
            let at = modified(&dir);
            if best.as_ref().map(|(b, _, _)| at > *b).unwrap_or(true) {
                best = Some((at, dir, state));
            }
        }
        let (_, from, state) = best?;
        copy_covers(&from.join("covers"), &self.dir.join("covers"));
        let _ = self.write_state(state);
        Some(from)
    }

    pub fn merge_listening_into_state(&self, incoming: &Value) -> Value {
        let mut state = self.state();
        if !state.is_object() {
            state = json!({});
        }
        let merged = merge_listening(
            state.get("listening").unwrap_or(&Value::Null),
            incoming,
        );
        if let Some(obj) = state.as_object_mut() {
            obj.insert("listening".into(), merged.clone());
        }
        let _ = self.write_state(state);
        merged
    }

    pub fn config(&self) -> Value {
        self.kv_get("config").unwrap_or_else(|| {
            json!({
                "downloadDir": default_download_dir(),
                "theme": "dark"
            })
        })
    }

    pub fn sync_meta(&self) -> Value {
        self.kv_get("sync").unwrap_or_else(|| {
            json!({
                "role": "", "enabled": false, "code": "", "hostUrl": "",
                "peer": "", "rev": 0, "lastSync": 0, "remoteUpdatedAt": 0
            })
        })
    }

    pub fn set_sync_meta(&self, value: Value) {
        let _ = self.kv_set("sync", &value);
    }

    pub(crate) fn bump_rev(&self) -> i64 {
        let conn = match self.conn.lock() {
            Ok(c) => c,
            Err(_) => return 0,
        };
        let next: i64 = {
            let mut stmt = match conn.prepare("SELECT value FROM meta WHERE key='rev'") {
                Ok(s) => s,
                Err(_) => return 0,
            };
            stmt.query_row([], |r| r.get::<_, String>(0))
                .ok()
                .and_then(|v| v.parse::<i64>().ok())
                .unwrap_or(0)
                + 1
        };
        let _ = conn.execute(
            "INSERT INTO meta (key, value) VALUES ('rev', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![next.to_string()],
        );
        next
    }
}

pub fn default_download_dir() -> String {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".into());
    format!("{home}/Music/Wavelength")
}

pub fn default_data_dir() -> PathBuf {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".into());
    let home = PathBuf::from(home);
    if cfg!(target_os = "windows") {
        let base = std::env::var("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|_| home.join("AppData/Roaming"));
        return base.join("app.wavelength.music.tauri");
    }
    if cfg!(target_os = "macos") {
        return home.join("Library/Application Support/app.wavelength.music.tauri");
    }
    // Linux: the same XDG folder Tauri gives the deb and AppImage builds, so a
    // headless run and the window share one library.
    let base = std::env::var("XDG_DATA_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/share"));
    base.join("app.wavelength.music.tauri")
}

/// Data folders this app has used before, on any packaging format.
pub fn legacy_data_dirs() -> Vec<PathBuf> {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let home = PathBuf::from(home);
    if cfg!(target_os = "windows") {
        let base = std::env::var("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|_| home.join("AppData/Roaming"));
        return [base.join("Wavelength"), base.join("app.wavelength.music.tauri")].to_vec();
    }
    if cfg!(target_os = "macos") {
        let base = home.join("Library/Application Support");
        return [base.join("Wavelength"), base.join("app.wavelength.music.tauri")].to_vec();
    }
    let base = std::env::var("XDG_DATA_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/share"));
    linux_legacy_dirs(&home, &base)
}

/// The user's real home, even inside a Flatpak sandbox where `$HOME` points at
/// `~/.var/app/<app-id>` instead.
fn real_home(home: &Path) -> &Path {
    let mut parts = home.components().rev();
    if let (Some(_id), Some(app), Some(var)) = (parts.next(), parts.next(), parts.next()) {
        if app.as_os_str() == "app" && var.as_os_str() == ".var" {
            if let Some(rest) = home.ancestors().nth(3) {
                return rest;
            }
        }
    }
    home
}

/// Linux data folders, from every packaging format.
///
/// A Flatpak has its own home and XDG data folder, and the deb/AppImage build
/// keeps its library in the real home. The Flatpak manifest grants that folder
/// read-only, so it can be adopted when the sandbox starts with no playlists;
/// the other direction (a deb reading the Flatpak's folder) needs no grant.
fn linux_legacy_dirs(home: &Path, xdg_home: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let real = real_home(home);
    let mut bases = vec![xdg_home.to_path_buf()];
    let real_data = real.join(".local/share");
    if real_data != xdg_home {
        bases.push(real_data);
    }
    for base in &bases {
        for name in ["Wavelength", "app.wavelength.music.tauri"] {
            dirs.push(base.join(name));
        }
    }
    for id in ["app.wavelength.music", "app.wavelength.music.tauri"] {
        let data = real.join(".var/app").join(id).join("data");
        for name in ["Wavelength", "app.wavelength.music.tauri", id] {
            dirs.push(data.join(name));
        }
    }
    dirs.sort();
    dirs.dedup();
    dirs
}

/// Read the saved state from another data folder, preferring the database
/// (authoritative) over the JSON mirror.
fn state_from_dir(dir: &Path) -> Option<Value> {
    let db = dir.join("wavelength.db");
    if db.is_file() {
        if let Ok(conn) = Connection::open_with_flags(
            &db,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        ) {
            let raw: Option<String> = conn
                .query_row("SELECT value FROM kv WHERE key='state'", [], |r| r.get(0))
                .ok();
            if let Some(raw) = raw {
                if let Ok(value) = serde_json::from_str::<Value>(&raw) {
                    return Some(value);
                }
            }
        }
    }
    let raw = std::fs::read(dir.join("state.json")).ok()?;
    serde_json::from_slice(&raw).ok()
}

fn modified(dir: &Path) -> std::time::SystemTime {
    let mut newest = std::time::UNIX_EPOCH;
    for name in ["wavelength.db", "state.json"] {
        if let Ok(m) = std::fs::metadata(dir.join(name)).and_then(|m| m.modified()) {
            if m > newest {
                newest = m;
            }
        }
    }
    newest
}

fn copy_covers(from: &Path, to: &Path) {
    let Ok(rd) = std::fs::read_dir(from) else { return };
    let _ = std::fs::create_dir_all(to);
    for entry in rd.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let dest = to.join(entry.file_name());
        if dest.exists() {
            continue;
        }
        let _ = std::fs::copy(&path, &dest);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_newest_legacy_library_only() {
        let root = std::env::temp_dir().join(format!("wl-recover-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let live = root.join("live");
        let old = root.join("old");
        let empty = root.join("empty");
        for dir in [&live, &old, &empty] {
            std::fs::create_dir_all(dir.join("covers")).unwrap();
        }
        std::fs::write(old.join("state.json"), r#"{"playlists":[{"id":"p1","tracks":[]}]}"#).unwrap();
        std::fs::write(old.join("covers/p1.jpg"), b"cover").unwrap();
        std::fs::write(empty.join("state.json"), r#"{"playlists":[]}"#).unwrap();

        // Feed the recovery walk directly: the test must not depend on where
        // this machine keeps its real library.
        let candidates = [&old, &empty];
        let mut best: Option<(std::time::SystemTime, &Path)> = None;
        for dir in candidates {
            let Some(state) = state_from_dir(dir) else { continue };
            let count = state.get("playlists").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0);
            if count == 0 {
                continue;
            }
            let at = modified(dir);
            if best.map(|(b, _)| at >= b).unwrap_or(true) {
                best = Some((at, dir));
            }
        }
        let chosen = best.map(|(_, d)| d).unwrap();
        assert_eq!(chosen, old.as_path());
        copy_covers(&chosen.join("covers"), &live.join("covers"));
        assert_eq!(std::fs::read(live.join("covers/p1.jpg")).unwrap(), b"cover");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn finds_the_real_home_from_inside_a_flatpak() {
        let home = Path::new("/home/u/.var/app/app.wavelength.music");
        let xdg = home.join("data");
        let dirs = linux_legacy_dirs(home, &xdg);
        let has = |p: &str| dirs.iter().any(|d| d == Path::new(p));

        // The sandbox's own library.
        assert!(has(
            "/home/u/.var/app/app.wavelength.music/data/app.wavelength.music.tauri"
        ));
        // The deb/AppImage library, at the path the manifest grants.
        assert!(has("/home/u/.local/share/app.wavelength.music.tauri"));
        // Never a doubly nested sandbox path.
        assert!(!dirs
            .iter()
            .any(|d| d.starts_with("/home/u/.var/app/app.wavelength.music/.var")));
    }

    #[test]
    fn a_deb_install_finds_the_flatpak_library() {
        let dirs = linux_legacy_dirs(Path::new("/home/u"), Path::new("/home/u/.local/share"));
        assert!(dirs.iter().any(|d| d
            == Path::new(
                "/home/u/.var/app/app.wavelength.music/data/app.wavelength.music.tauri"
            )));
        assert!(dirs
            .iter()
            .any(|d| d == Path::new("/home/u/.local/share/app.wavelength.music.tauri")));
        // A normal home is not mistaken for a sandbox.
        assert_eq!(real_home(Path::new("/home/u")), Path::new("/home/u"));
    }
}
