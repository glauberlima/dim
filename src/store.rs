//! Tiny persistence in %APPDATA%\dim: recent images and window geometry.
use std::{
    fs,
    path::{Path, PathBuf},
};

pub const MAX_RECENT: usize = 10;

fn dir() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|a| PathBuf::from(a).join("dim"))
}

pub fn load_recent() -> Vec<PathBuf> {
    let text = dir().and_then(|d| fs::read_to_string(d.join("recent.txt")).ok()).unwrap_or_default();
    text.lines().filter(|l| !l.is_empty()).take(MAX_RECENT).map(PathBuf::from).collect()
}

pub fn save_recent(list: &[PathBuf]) {
    if let Some(d) = dir() {
        let _ = fs::create_dir_all(&d);
        let text: Vec<String> = list.iter().map(|p| p.to_string_lossy().into_owned()).collect();
        let _ = fs::write(d.join("recent.txt"), text.join("\n"));
    }
}

/// Case-insensitive identity of a path (Windows paths compare that way).
pub fn path_key(p: &Path) -> String {
    p.to_string_lossy().to_lowercase()
}

/// Moves `p` to the front (case-insensitive dedupe) and keeps at most MAX_RECENT entries.
pub fn push_recent(list: &mut Vec<PathBuf>, p: PathBuf) {
    let k = path_key(&p);
    list.retain(|x| path_key(x) != k);
    list.insert(0, p);
    list.truncate(MAX_RECENT);
}

/// [x, y, width, height]; width/height normalized to 96 DPI.
pub fn load_window() -> Option<[i32; 4]> {
    let text = fs::read_to_string(dir()?.join("window.txt")).ok()?;
    let v: Vec<i32> = text.split_whitespace().filter_map(|s| s.parse().ok()).collect();
    v.try_into().ok()
}

pub fn save_window(r: [i32; 4]) {
    if let Some(d) = dir() {
        let _ = fs::create_dir_all(&d);
        let _ = fs::write(d.join("window.txt"), format!("{} {} {} {}", r[0], r[1], r[2], r[3]));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recent_dedupes_ignoring_case_and_caps_at_ten() {
        let mut l = vec![];
        for i in 0..12 {
            push_recent(&mut l, PathBuf::from(format!("C:\\img{i}.iso")));
        }
        assert_eq!(l.len(), MAX_RECENT);
        assert_eq!(l[0], PathBuf::from("C:\\img11.iso"));
        push_recent(&mut l, PathBuf::from("c:\\IMG5.ISO"));
        assert_eq!(l.len(), MAX_RECENT);
        assert_eq!(l[0], PathBuf::from("c:\\IMG5.ISO"));
        assert_eq!(l.iter().filter(|p| p.to_string_lossy().eq_ignore_ascii_case("c:\\img5.iso")).count(), 1);
    }
}
