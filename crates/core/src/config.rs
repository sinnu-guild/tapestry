//! Settings, stored as a small human-editable INI file:
//! `%APPDATA%\Tapestry\tapestry.ini` on Windows.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordMode {
    Off,
    Motion,
    Continuous,
}

impl RecordMode {
    pub fn as_str(self) -> &'static str {
        match self {
            RecordMode::Off => "off",
            RecordMode::Motion => "motion",
            RecordMode::Continuous => "continuous",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" | "none" => Some(RecordMode::Off),
            "motion" => Some(RecordMode::Motion),
            "continuous" | "always" | "24/7" => Some(RecordMode::Continuous),
            _ => None,
        }
    }
    pub const ALL: [RecordMode; 3] = [RecordMode::Off, RecordMode::Motion, RecordMode::Continuous];
}

#[derive(Debug, Clone, PartialEq)]
pub struct Camera {
    pub name: String,
    pub host: String,
    pub username: String,
    pub password: String,
    pub onvif_port: u16,
    pub rtsp_port: u16,
    /// RTSP path of the high-quality stream (Tapo: `stream1`).
    pub main_path: String,
    /// RTSP path of the low-quality stream (Tapo: `stream2`).
    pub sub_path: String,
    /// Flip pan/tilt directions (for cameras mounted upside down).
    pub invert_ptz: bool,
    /// Per-camera recording switch (the global mode still applies).
    pub record: bool,
}

impl Default for Camera {
    fn default() -> Self {
        Camera {
            name: String::new(),
            host: String::new(),
            username: String::new(),
            password: String::new(),
            onvif_port: 2020,
            rtsp_port: 554,
            main_path: "stream1".into(),
            sub_path: "stream2".into(),
            invert_ptz: false,
            record: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Quality {
    Main,
    Sub,
}

impl Camera {
    /// rtsp://user:pass@host:port/path with credentials percent-encoded.
    pub fn rtsp_url(&self, q: Quality) -> String {
        let path = match q {
            Quality::Main => &self.main_path,
            Quality::Sub => &self.sub_path,
        };
        let path = path.trim_start_matches('/');
        let host =
            if self.host.contains(':') && !self.host.starts_with('[') { format!("[{}]", self.host) } else { self.host.clone() };
        if self.username.is_empty() {
            format!("rtsp://{host}:{}/{path}", self.rtsp_port)
        } else {
            format!(
                "rtsp://{}:{}@{host}:{}/{path}",
                percent_encode(&self.username),
                percent_encode(&self.password),
                self.rtsp_port
            )
        }
    }

    /// Same URL with the password masked, for logs and UI.
    pub fn rtsp_url_redacted(&self, q: Quality) -> String {
        let mut c = self.clone();
        if !c.password.is_empty() {
            c.password = "****".into();
        }
        c.rtsp_url(q).replace("%2A%2A%2A%2A", "****")
    }

    /// A filesystem-safe folder name for this camera's recordings.
    pub fn slug(&self) -> String {
        slugify(&self.name)
    }
}

pub fn slugify(name: &str) -> String {
    let mut s = String::new();
    let mut dash = false;
    for c in name.trim().chars() {
        if c.is_alphanumeric() {
            s.extend(c.to_lowercase());
            dash = false;
        } else if !dash && !s.is_empty() {
            s.push('-');
            dash = true;
        }
    }
    while s.ends_with('-') {
        s.pop();
    }
    if s.is_empty() {
        "camera".into()
    } else {
        s
    }
}

/// Encode everything outside RFC 3986 "unreserved".
pub fn percent_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    /// Path to ffmpeg.exe; empty = look next to Tapestry, then on PATH.
    pub ffmpeg: String,
    pub recordings_dir: PathBuf,
    pub record_mode: RecordMode,
    /// Keep recording this long after motion stops.
    pub post_roll_secs: u32,
    /// Split long motion clips / continuous recordings into files this long.
    pub max_clip_secs: u32,
    /// Delete recordings older than this (0 = never).
    pub keep_days: u32,
    /// Delete oldest recordings when the folder grows beyond this (0 = no cap).
    pub max_storage_gb: u32,
    /// Flash the taskbar button when motion happens while Tapestry isn't focused.
    pub flash_taskbar: bool,
    pub always_on_top: bool,
    /// Frame rate for the grid tiles (lower = less CPU).
    pub grid_fps: u32,
    /// Click on the focused video to aim the camera there.
    pub click_to_aim: bool,
    /// How far one click-to-aim moves, relative units (tuned per camera).
    pub aim_gain: f32,
    pub cameras: Vec<Camera>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            ffmpeg: String::new(),
            recordings_dir: default_recordings_dir(),
            record_mode: RecordMode::Motion,
            post_roll_secs: 20,
            max_clip_secs: 300,
            keep_days: 14,
            max_storage_gb: 100,
            flash_taskbar: true,
            always_on_top: false,
            grid_fps: 25,
            click_to_aim: true,
            aim_gain: 0.35,
            cameras: Vec::new(),
        }
    }
}

pub fn config_dir() -> PathBuf {
    if let Some(appdata) = std::env::var_os("APPDATA") {
        return PathBuf::from(appdata).join("Tapestry");
    }
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        return PathBuf::from(xdg).join("tapestry");
    }
    home_dir().join(".config").join("tapestry")
}

pub fn config_path() -> PathBuf {
    config_dir().join("tapestry.ini")
}

fn home_dir() -> PathBuf {
    std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."))
}

pub fn default_recordings_dir() -> PathBuf {
    home_dir().join("Videos").join("Tapestry")
}

fn parse_bool(v: &str) -> Option<bool> {
    match v.trim().to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Some(true),
        "false" | "no" | "off" | "0" => Some(false),
        _ => None,
    }
}

/// Values may be wrapped in double quotes to keep leading/trailing spaces.
fn unquote(v: &str) -> String {
    let t = v.trim();
    if t.len() >= 2 && t.starts_with('"') && t.ends_with('"') {
        t[1..t.len() - 1].replace("\\\"", "\"").replace("\\\\", "\\")
    } else {
        t.to_string()
    }
}

fn quote_if_needed(v: &str) -> String {
    if v != v.trim() || v.starts_with('"') {
        format!("\"{}\"", v.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        v.to_string()
    }
}

impl Config {
    /// Parse INI text. Unknown keys are ignored; problems are returned as warnings.
    pub fn parse(text: &str) -> (Config, Vec<String>) {
        let mut cfg = Config { cameras: Vec::new(), ..Config::default() };
        let mut warnings = Vec::new();
        let mut section = String::new();

        for (lineno, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                continue;
            }
            if line.starts_with('[') && line.ends_with(']') {
                section = line[1..line.len() - 1].trim().to_ascii_lowercase();
                if section == "camera" {
                    cfg.cameras.push(Camera::default());
                }
                continue;
            }
            let Some((k, v)) = line.split_once('=') else {
                warnings.push(format!("line {}: expected key = value", lineno + 1));
                continue;
            };
            let key = k.trim().to_ascii_lowercase();
            let val = unquote(v);
            let mut bad = || warnings.push(format!("line {}: bad value for {key}: {val:?}", lineno + 1));

            match section.as_str() {
                "general" | "" => match key.as_str() {
                    "ffmpeg" => cfg.ffmpeg = val.clone(),
                    "recordings_dir" => {
                        if !val.is_empty() {
                            cfg.recordings_dir = PathBuf::from(&val)
                        }
                    }
                    "record_mode" => match RecordMode::parse(&val) {
                        Some(m) => cfg.record_mode = m,
                        None => bad(),
                    },
                    "post_roll_secs" => val.parse().map(|v| cfg.post_roll_secs = v).unwrap_or_else(|_| bad()),
                    "max_clip_secs" => val.parse().map(|v: u32| cfg.max_clip_secs = v.max(10)).unwrap_or_else(|_| bad()),
                    "keep_days" => val.parse().map(|v| cfg.keep_days = v).unwrap_or_else(|_| bad()),
                    "max_storage_gb" => val.parse().map(|v| cfg.max_storage_gb = v).unwrap_or_else(|_| bad()),
                    "grid_fps" => val.parse().map(|v: u32| cfg.grid_fps = v.clamp(1, 30)).unwrap_or_else(|_| bad()),
                    "aim_gain" => val.parse().map(|v: f32| cfg.aim_gain = v.clamp(0.01, 2.0)).unwrap_or_else(|_| bad()),
                    "flash_taskbar" => parse_bool(&val).map(|v| cfg.flash_taskbar = v).unwrap_or_else(bad),
                    "always_on_top" => parse_bool(&val).map(|v| cfg.always_on_top = v).unwrap_or_else(bad),
                    "click_to_aim" => parse_bool(&val).map(|v| cfg.click_to_aim = v).unwrap_or_else(bad),
                    _ => {}
                },
                "camera" => {
                    let cam = cfg.cameras.last_mut().unwrap();
                    match key.as_str() {
                        "name" => cam.name = val.clone(),
                        "host" | "ip" => cam.host = val.clone(),
                        "username" | "user" => cam.username = val.clone(),
                        "password" | "pass" => cam.password = val.clone(),
                        "onvif_port" => val.parse().map(|v| cam.onvif_port = v).unwrap_or_else(|_| bad()),
                        "rtsp_port" => val.parse().map(|v| cam.rtsp_port = v).unwrap_or_else(|_| bad()),
                        "main_path" => cam.main_path = val.clone(),
                        "sub_path" => cam.sub_path = val.clone(),
                        "invert_ptz" => parse_bool(&val).map(|v| cam.invert_ptz = v).unwrap_or_else(bad),
                        "record" => parse_bool(&val).map(|v| cam.record = v).unwrap_or_else(bad),
                        _ => {}
                    }
                }
                _ => {}
            }
        }

        cfg.cameras.retain(|c| !c.host.is_empty());
        for (i, c) in cfg.cameras.iter_mut().enumerate() {
            if c.name.trim().is_empty() {
                c.name = format!("Camera {}", i + 1);
            }
        }
        (cfg, warnings)
    }

    pub fn to_ini(&self) -> String {
        let mut s = String::new();
        let _ = writeln!(s, "# Tapestry settings. Safe to edit by hand while Tapestry is closed.");
        let _ = writeln!(s, "# The camera username/password is the *Camera Account* from the Tapo app");
        let _ = writeln!(s, "# (Camera Settings > Advanced Settings > Camera Account), not your TP-Link login.");
        let _ = writeln!(s);
        let _ = writeln!(s, "[general]");
        let _ = writeln!(s, "ffmpeg = {}", quote_if_needed(&self.ffmpeg));
        let _ = writeln!(s, "recordings_dir = {}", quote_if_needed(&self.recordings_dir.to_string_lossy()));
        let _ = writeln!(s, "# off | motion | continuous");
        let _ = writeln!(s, "record_mode = {}", self.record_mode.as_str());
        let _ = writeln!(s, "post_roll_secs = {}", self.post_roll_secs);
        let _ = writeln!(s, "max_clip_secs = {}", self.max_clip_secs);
        let _ = writeln!(s, "keep_days = {}", self.keep_days);
        let _ = writeln!(s, "max_storage_gb = {}", self.max_storage_gb);
        let _ = writeln!(s, "flash_taskbar = {}", self.flash_taskbar);
        let _ = writeln!(s, "always_on_top = {}", self.always_on_top);
        let _ = writeln!(s, "grid_fps = {}", self.grid_fps);
        let _ = writeln!(s, "click_to_aim = {}", self.click_to_aim);
        let _ = writeln!(s, "aim_gain = {}", self.aim_gain);
        for c in &self.cameras {
            let _ = writeln!(s);
            let _ = writeln!(s, "[camera]");
            let _ = writeln!(s, "name = {}", quote_if_needed(&c.name));
            let _ = writeln!(s, "host = {}", c.host.trim());
            let _ = writeln!(s, "username = {}", quote_if_needed(&c.username));
            let _ = writeln!(s, "password = {}", quote_if_needed(&c.password));
            let _ = writeln!(s, "onvif_port = {}", c.onvif_port);
            let _ = writeln!(s, "rtsp_port = {}", c.rtsp_port);
            let _ = writeln!(s, "main_path = {}", c.main_path);
            let _ = writeln!(s, "sub_path = {}", c.sub_path);
            let _ = writeln!(s, "invert_ptz = {}", c.invert_ptz);
            let _ = writeln!(s, "record = {}", c.record);
        }
        s
    }

    pub fn load_from(path: &Path) -> std::io::Result<(Config, Vec<String>)> {
        let text = std::fs::read_to_string(path)?;
        Ok(Config::parse(&text))
    }

    /// Load from the default location; a missing file gives defaults.
    pub fn load() -> (Config, Vec<String>) {
        match Config::load_from(&config_path()) {
            Ok(r) => r,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Config::default(), Vec::new()),
            Err(e) => (Config::default(), vec![format!("couldn't read settings: {e}")]),
        }
    }

    pub fn save_to(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        // Write-then-rename so a crash never leaves a half-written file.
        let tmp = path.with_extension("ini.tmp");
        std::fs::write(&tmp, self.to_ini())?;
        std::fs::rename(&tmp, path)
    }

    pub fn save(&self) -> std::io::Result<()> {
        self.save_to(&config_path())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let mut cfg = Config::default();
        cfg.record_mode = RecordMode::Continuous;
        cfg.keep_days = 30;
        cfg.cameras.push(Camera {
            name: "Front Gate".into(),
            host: "192.168.1.21".into(),
            username: "tapestry".into(),
            password: " p@ss#w0rd\"x ".into(),
            invert_ptz: true,
            ..Camera::default()
        });
        cfg.cameras.push(Camera { name: "Garden".into(), host: "192.168.1.22".into(), ..Camera::default() });
        let (back, warnings) = Config::parse(&cfg.to_ini());
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(back, cfg);
    }

    #[test]
    fn tolerant_parse() {
        let (cfg, warnings) =
            Config::parse("record_mode = sometimes\n[camera]\nip = 10.0.0.2\n[camera]\nname = no host\n[weird]\nx=1\nnonsense\n");
        assert_eq!(cfg.cameras.len(), 1);
        assert_eq!(cfg.cameras[0].name, "Camera 1");
        assert_eq!(cfg.record_mode, RecordMode::Motion);
        assert_eq!(warnings.len(), 2);
    }

    #[test]
    fn rtsp_urls() {
        let c = Camera { host: "192.168.1.5".into(), username: "me".into(), password: "a:b@c/d".into(), ..Camera::default() };
        assert_eq!(c.rtsp_url(Quality::Main), "rtsp://me:a%3Ab%40c%2Fd@192.168.1.5:554/stream1");
        assert_eq!(c.rtsp_url(Quality::Sub), "rtsp://me:a%3Ab%40c%2Fd@192.168.1.5:554/stream2");
        assert_eq!(c.rtsp_url_redacted(Quality::Sub), "rtsp://me:****@192.168.1.5:554/stream2");
    }

    #[test]
    fn slugs() {
        assert_eq!(slugify("  Front Gate (C530WS) "), "front-gate-c530ws");
        assert_eq!(slugify("***"), "camera");
    }
}
