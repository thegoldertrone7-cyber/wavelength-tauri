//! A small log file (`wavelength.log` in the data folder) that survives a crash.
//!
//! The page writes breadcrumbs ("open playlist (240 songs)", "drawn playlist", errors) through `/api/log`, and a
//! panic hook records any panic in the core, so when the app dies the last lines say what it was doing.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_BYTES: u64 = 512 * 1024;

pub fn log(dir: &Path, line: &str) {
    let path = dir.join("wavelength.log");
    if std::fs::metadata(&path).map_or(false, |m| m.len() > MAX_BYTES) {
        let _ = std::fs::rename(&path, dir.join("wavelength.log.1"));
    }
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let clean: String = line.chars().filter(|c| !c.is_control()).take(600).collect();
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{secs} {clean}");
    }
}

pub fn install_panic_hook(dir: PathBuf) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        log(&dir, &format!("PANIC {info}"));
        previous(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_and_rotates() {
        let dir = std::env::temp_dir().join(format!("wl-diag-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        log(&dir, "open playlist (3 songs)\nsecond line");
        let text = std::fs::read_to_string(dir.join("wavelength.log")).unwrap();
        assert!(text.contains("open playlist (3 songs)second line"), "control characters are dropped: {text}");
        std::fs::write(dir.join("wavelength.log"), vec![b'x'; (MAX_BYTES + 1) as usize]).unwrap();
        log(&dir, "after rotation");
        assert!(dir.join("wavelength.log.1").exists());
        assert!(std::fs::read_to_string(dir.join("wavelength.log")).unwrap().contains("after rotation"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
