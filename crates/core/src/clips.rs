//! The recordings folder: listing clips, the motion log, and cleanup.
//!
//! Layout (everything Tapestry writes, nothing else is ever touched):
//! ```text
//! <recordings_dir>/<camera-slug>/<YYYY-MM-DD>/<HH-MM-SS>.mp4
//! <recordings_dir>/<camera-slug>/snapshots/<YYYY-MM-DD_HH-MM-SS>.jpg
//! <recordings_dir>/<camera-slug>/motion.csv
//! ```
//! Cleanup only deletes files matching the clip pattern above, so pointing
//! `recordings_dir` at a folder with other videos in it is safe.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, PartialEq)]
pub struct Clip {
    pub camera_slug: String,
    /// `YYYY-MM-DD` (local date when recorded)
    pub day: String,
    /// `HH-MM-SS` or `HH-MM-SS-2` (local time when recorded)
    pub stem: String,
    pub path: PathBuf,
    pub bytes: u64,
    pub modified: SystemTime,
}

impl Clip {
    /// (hour, minute, second) parsed from the file name.
    pub fn hms(&self) -> Option<(u32, u32, u32)> {
        parse_stem(&self.stem)
    }
}

fn all_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// `YYYY-MM-DD`
pub fn is_day_name(s: &str) -> bool {
    let p: Vec<&str> = s.split('-').collect();
    p.len() == 3 && p[0].len() == 4 && p[1].len() == 2 && p[2].len() == 2 && p.iter().all(|x| all_digits(x))
}

/// `HH-MM-SS` with optional `-N` suffix.
pub fn parse_stem(s: &str) -> Option<(u32, u32, u32)> {
    let p: Vec<&str> = s.split('-').collect();
    if !(p.len() == 3 || p.len() == 4) || !p.iter().all(|x| all_digits(x)) {
        return None;
    }
    if p[0].len() != 2 || p[1].len() != 2 || p[2].len() != 2 {
        return None;
    }
    let (h, m, sec) = (p[0].parse().ok()?, p[1].parse().ok()?, p[2].parse().ok()?);
    (h < 24 && m < 60 && sec < 61).then_some((h, m, sec))
}

fn is_clip_file(path: &Path) -> bool {
    path.extension().map_or(false, |e| e.eq_ignore_ascii_case("mp4"))
        && path.file_stem().and_then(|s| s.to_str()).map_or(false, |s| parse_stem(s).is_some())
}

/// All clips under `root`, newest first.
pub fn scan(root: &Path) -> Vec<Clip> {
    let mut out = Vec::new();
    let Ok(cams) = fs::read_dir(root) else { return out };
    for cam in cams.flatten() {
        if !cam.path().is_dir() {
            continue;
        }
        let slug = cam.file_name().to_string_lossy().into_owned();
        let Ok(days) = fs::read_dir(cam.path()) else { continue };
        for day in days.flatten() {
            let day_name = day.file_name().to_string_lossy().into_owned();
            if !is_day_name(&day_name) || !day.path().is_dir() {
                continue;
            }
            let Ok(files) = fs::read_dir(day.path()) else { continue };
            for f in files.flatten() {
                let path = f.path();
                if !is_clip_file(&path) {
                    continue;
                }
                let Ok(meta) = f.metadata() else { continue };
                out.push(Clip {
                    camera_slug: slug.clone(),
                    day: day_name.clone(),
                    stem: path.file_stem().unwrap().to_string_lossy().into_owned(),
                    bytes: meta.len(),
                    modified: meta.modified().unwrap_or(UNIX_EPOCH),
                    path,
                });
            }
        }
    }
    out.sort_by(|a, b| (&b.day, &b.stem, &b.camera_slug).cmp(&(&a.day, &a.stem, &a.camera_slug)));
    out
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct CleanupReport {
    pub deleted: usize,
    pub freed_bytes: u64,
    pub kept_bytes: u64,
}

/// Delete clips older than `keep_days` (0 = keep forever), then the oldest
/// clips until the total is under `max_bytes` (0 = no cap). Files in
/// `protect` (currently being written) are never touched.
pub fn cleanup(root: &Path, keep_days: u32, max_bytes: u64, protect: &[PathBuf], now: SystemTime) -> CleanupReport {
    let mut report = CleanupReport::default();
    let mut clips = scan(root);
    clips.retain(|c| !protect.iter().any(|p| p == &c.path));
    // Oldest first.
    clips.sort_by_key(|c| c.modified);

    let cutoff = (keep_days > 0).then(|| now.checked_sub(Duration::from_secs(keep_days as u64 * 86400))).flatten();
    let mut remaining: Vec<Clip> = Vec::new();
    for c in clips {
        if cutoff.map_or(false, |cut| c.modified < cut) && fs::remove_file(&c.path).is_ok() {
            report.deleted += 1;
            report.freed_bytes += c.bytes;
        } else {
            remaining.push(c);
        }
    }

    let protected_bytes: u64 = protect.iter().filter_map(|p| fs::metadata(p).ok()).map(|m| m.len()).sum();
    let mut total: u64 = remaining.iter().map(|c| c.bytes).sum::<u64>() + protected_bytes;
    if max_bytes > 0 {
        let mut i = 0;
        while total > max_bytes && i < remaining.len() {
            let c = &remaining[i];
            if fs::remove_file(&c.path).is_ok() {
                report.deleted += 1;
                report.freed_bytes += c.bytes;
                total -= c.bytes;
            }
            i += 1;
        }
    }
    report.kept_bytes = total;

    // Tidy now-empty day folders (only ones matching our naming).
    if let Ok(cams) = fs::read_dir(root) {
        for cam in cams.flatten() {
            if let Ok(days) = fs::read_dir(cam.path()) {
                for day in days.flatten() {
                    if is_day_name(&day.file_name().to_string_lossy()) {
                        let _ = fs::remove_dir(day.path()); // fails harmlessly if not empty
                    }
                }
            }
        }
    }
    report
}

/// Total bytes of clips under `root`.
pub fn usage_bytes(root: &Path) -> u64 {
    scan(root).iter().map(|c| c.bytes).sum()
}

// ----- motion log -----------------------------------------------------------

/// A motion interval in Unix seconds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MotionSpan {
    pub start: i64,
    pub end: i64,
}

pub fn motion_log_path(root: &Path, slug: &str) -> PathBuf {
    root.join(slug).join("motion.csv")
}

pub fn append_motion(path: &Path, span: MotionSpan) -> std::io::Result<()> {
    if let Some(d) = path.parent() {
        fs::create_dir_all(d)?;
    }
    let mut f = OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(f, "{},{}", span.start, span.end)
}

pub fn read_motion(path: &Path) -> Vec<MotionSpan> {
    let Ok(text) = fs::read_to_string(path) else { return Vec::new() };
    text.lines()
        .filter_map(|l| {
            let (a, b) = l.trim().split_once(',')?;
            Some(MotionSpan { start: a.trim().parse().ok()?, end: b.trim().parse().ok()? })
        })
        .filter(|s| s.end >= s.start)
        .collect()
}

/// Drop motion entries older than `before` (Unix seconds).
pub fn prune_motion(path: &Path, before: i64) -> std::io::Result<()> {
    let spans = read_motion(path);
    if spans.iter().all(|s| s.end >= before) {
        return Ok(());
    }
    let keep: String = spans.iter().filter(|s| s.end >= before).map(|s| format!("{},{}\n", s.start, s.end)).collect();
    let tmp = path.with_extension("csv.tmp");
    fs::write(&tmp, keep)?;
    fs::rename(&tmp, path)
}

pub fn system_time_to_unix(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(e) => -(e.duration().as_secs() as i64),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("tapestry-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn write(p: &Path, bytes: usize) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, vec![0u8; bytes]).unwrap();
    }

    #[test]
    fn names() {
        assert!(is_day_name("2026-11-20"));
        assert!(!is_day_name("2026-1-20"));
        assert_eq!(parse_stem("14-05-33"), Some((14, 5, 33)));
        assert_eq!(parse_stem("14-05-33-2"), Some((14, 5, 33)));
        assert_eq!(parse_stem("holiday"), None);
        assert_eq!(parse_stem("25-00-00"), None);
    }

    #[test]
    fn scan_ignores_foreign_files() {
        let d = tmpdir("scan");
        write(&d.join("garden/2026-11-20/14-05-33.mp4"), 10);
        write(&d.join("garden/2026-11-20/14-09-00.mp4"), 10);
        write(&d.join("gate/2026-11-19/23-59-59.mp4"), 10);
        write(&d.join("gate/2026-11-19/my wedding.mp4"), 10);
        write(&d.join("gate/holidays/01-02-03.mp4"), 10);
        write(&d.join("gate/snapshots/2026-11-19_10-00-00.jpg"), 10);
        let clips = scan(&d);
        let names: Vec<_> = clips.iter().map(|c| format!("{}/{}/{}", c.camera_slug, c.day, c.stem)).collect();
        assert_eq!(names, vec!["garden/2026-11-20/14-09-00", "garden/2026-11-20/14-05-33", "gate/2026-11-19/23-59-59"]);
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn cleanup_by_size_respects_protection_and_foreign_files() {
        let d = tmpdir("cleanup");
        let a = d.join("cam/2026-11-18/10-00-00.mp4");
        let b = d.join("cam/2026-11-19/10-00-00.mp4");
        let c = d.join("cam/2026-11-20/10-00-00.mp4");
        let foreign = d.join("cam/2026-11-18/keep me.mp4");
        write(&a, 1000);
        std::thread::sleep(Duration::from_millis(20));
        write(&b, 1000);
        std::thread::sleep(Duration::from_millis(20));
        write(&c, 1000);
        write(&foreign, 5000);
        // Cap at 1500 bytes, protect the oldest (pretend it's recording).
        let r = cleanup(&d, 0, 1500, &[a.clone()], SystemTime::now());
        assert!(a.exists(), "protected file kept");
        assert!(!b.exists(), "oldest unprotected deleted");
        assert!(!c.exists(), "still over cap, next deleted");
        assert!(foreign.exists(), "never touch files we didn't name");
        assert_eq!(r.deleted, 2);
        assert!(!d.join("cam/2026-11-19").exists(), "empty day folder removed");
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn cleanup_by_age() {
        let d = tmpdir("age");
        let a = d.join("cam/2026-11-01/10-00-00.mp4");
        write(&a, 100);
        let future = SystemTime::now() + Duration::from_secs(15 * 86400);
        assert_eq!(cleanup(&d, 30, 0, &[], future).deleted, 0);
        assert_eq!(cleanup(&d, 14, 0, &[], future).deleted, 1);
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn motion_log_roundtrip_and_prune() {
        let d = tmpdir("motion");
        let p = motion_log_path(&d, "cam");
        append_motion(&p, MotionSpan { start: 100, end: 130 }).unwrap();
        append_motion(&p, MotionSpan { start: 500, end: 520 }).unwrap();
        assert_eq!(read_motion(&p).len(), 2);
        prune_motion(&p, 200).unwrap();
        assert_eq!(read_motion(&p), vec![MotionSpan { start: 500, end: 520 }]);
        fs::remove_dir_all(&d).ok();
    }
}
