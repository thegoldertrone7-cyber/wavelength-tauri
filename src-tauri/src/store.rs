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
             PRAGMA synchronous=NORMAL;
             CREATE TABLE IF NOT EXISTS kv (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
        )
        .map_err(|e| e.to_string())?;
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
        let _ = std::fs::write(
            self.dir.join("state.json"),
            serde_json::to_vec_pretty(&next).unwrap_or_default(),
        );
        Ok(next)
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
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    format!("{home}/Music/Wavelength")
}

pub fn default_data_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join("Library/Application Support/Wavelength")
}
