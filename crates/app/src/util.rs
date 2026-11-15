//! Small helpers: local time formatting, opening folders, geometry.

use chrono::{DateTime, Local, NaiveDate, TimeZone};
use eframe::egui::{self, Rect};
use std::path::Path;
use std::time::{Duration, SystemTime};
use tapestry_core::clips::Clip;

/// Local-time file namer for recordings: ("2026-11-20", "14-05-33").
pub fn local_namer() -> tapestry_core::recorder::Namer {
    std::sync::Arc::new(|| {
        let now = Local::now();
        (now.format("%Y-%m-%d").to_string(), now.format("%H-%M-%S").to_string())
    })
}

pub fn today() -> String {
    Local::now().format("%Y-%m-%d").to_string()
}

pub fn to_local(t: SystemTime) -> DateTime<Local> {
    DateTime::<Local>::from(t)
}

pub fn unix_to_local(secs: i64) -> Option<DateTime<Local>> {
    Local.timestamp_opt(secs, 0).single()
}

/// When a clip started, from its folder/file name (local time).
pub fn clip_start(clip: &Clip) -> Option<DateTime<Local>> {
    let date = NaiveDate::parse_from_str(&clip.day, "%Y-%m-%d").ok()?;
    let (h, m, s) = clip.hms()?;
    let naive = date.and_hms_opt(h, m, s.min(59))?;
    Local.from_local_datetime(&naive).earliest()
}

/// Approximate clip length: last write minus start (no need to probe).
pub fn clip_len(clip: &Clip) -> Option<Duration> {
    let start = clip_start(clip)?;
    let end = to_local(clip.modified);
    let secs = (end - start).num_milliseconds();
    (secs > 0 && secs < 6 * 3600 * 1000).then(|| Duration::from_millis(secs as u64))
}

/// Seconds since local midnight of `day` for a local time.
pub fn secs_into_day(day: &str, t: &DateTime<Local>) -> Option<f32> {
    let date = NaiveDate::parse_from_str(day, "%Y-%m-%d").ok()?;
    let midnight = Local.from_local_datetime(&date.and_hms_opt(0, 0, 0)?).earliest()?;
    Some((*t - midnight).num_milliseconds() as f32 / 1000.0)
}

pub fn day_label(day: &str) -> String {
    let Ok(date) = NaiveDate::parse_from_str(day, "%Y-%m-%d") else { return day.to_string() };
    let today = Local::now().date_naive();
    match (today - date).num_days() {
        0 => "Today".into(),
        1 => "Yesterday".into(),
        2..=6 => date.format("%A").to_string(),
        _ => date.format("%a %d %b").to_string(),
    }
}

pub fn hhmm(t: SystemTime) -> String {
    to_local(t).format("%H:%M").to_string()
}

pub fn hhmmss(t: SystemTime) -> String {
    to_local(t).format("%H:%M:%S").to_string()
}

pub fn ago(t: SystemTime) -> String {
    let secs = SystemTime::now().duration_since(t).map(|d| d.as_secs()).unwrap_or(0);
    match secs {
        0..=9 => "just now".into(),
        10..=59 => format!("{secs}s ago"),
        60..=3599 => format!("{} min ago", secs / 60),
        3600..=86_399 => format!("{}h {}m ago", secs / 3600, (secs % 3600) / 60),
        _ => format!("{} days ago", secs / 86_400),
    }
}

pub fn dur(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

pub fn bytes(b: u64) -> String {
    let b = b as f64;
    if b >= 1e9 {
        format!("{:.1} GB", b / 1e9)
    } else if b >= 1e6 {
        format!("{:.0} MB", b / 1e6)
    } else {
        format!("{:.0} KB", b / 1e3)
    }
}

/// Largest rect of the given aspect ratio centred in `outer`.
pub fn fit_aspect(outer: Rect, aspect: f32) -> Rect {
    let (w, h) = (outer.width(), outer.height());
    let size = if w / h > aspect { egui::vec2(h * aspect, h) } else { egui::vec2(w, w / aspect) };
    Rect::from_center_size(outer.center(), size)
}

pub fn open_path(p: &Path) {
    #[cfg(windows)]
    let _ = std::process::Command::new("explorer").arg(p).spawn();
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(p).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let _ = std::process::Command::new("xdg-open").arg(p).spawn();
}

pub fn show_in_folder(p: &Path) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let _ = std::process::Command::new("explorer").raw_arg(format!("/select,\"{}\"", p.display())).spawn();
    }
    #[cfg(not(windows))]
    if let Some(dir) = p.parent() {
        open_path(dir);
    }
}
