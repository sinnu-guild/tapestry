//! Locating and launching FFmpeg.
//!
//! Tapestry leans on FFmpeg for all video work (RTSP, H.264/H.265 decoding,
//! recording, snapshots, playback). Running it as a child process keeps the
//! Rust build simple — no C libraries to link on Windows.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

/// Find ffmpeg: explicit setting → next to our exe → PATH.
pub fn locate(configured: &str) -> Option<PathBuf> {
    let configured = configured.trim();
    if !configured.is_empty() {
        let p = PathBuf::from(configured);
        return if p.is_file() { Some(p) } else { which(configured) };
    }
    let exe_name = if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" };
    if let Ok(me) = std::env::current_exe() {
        if let Some(dir) = me.parent() {
            for cand in [dir.join(exe_name), dir.join("ffmpeg").join("bin").join(exe_name)] {
                if cand.is_file() {
                    return Some(cand);
                }
            }
        }
    }
    which(exe_name).or_else(winget_fallback)
}

/// `winget install Gyan.FFmpeg` puts ffmpeg on PATH, but only for programs
/// started *after* the install. Look in winget's folders directly too.
fn winget_fallback() -> Option<PathBuf> {
    let local = PathBuf::from(std::env::var_os("LOCALAPPDATA")?);
    let link = local.join("Microsoft").join("WinGet").join("Links").join("ffmpeg.exe");
    if link.is_file() {
        return Some(link);
    }
    let packages = local.join("Microsoft").join("WinGet").join("Packages");
    for pkg in std::fs::read_dir(packages).ok()?.flatten() {
        if !pkg.file_name().to_string_lossy().starts_with("Gyan.FFmpeg") {
            continue;
        }
        for build in std::fs::read_dir(pkg.path()).ok()?.flatten() {
            let exe = build.path().join("bin").join("ffmpeg.exe");
            if exe.is_file() {
                return Some(exe);
            }
        }
    }
    None
}

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let cand = dir.join(name);
        if cand.is_file() {
            return Some(cand);
        }
        if cfg!(windows) && !name.to_ascii_lowercase().ends_with(".exe") {
            let cand = dir.join(format!("{name}.exe"));
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    None
}

/// A `Command` that won't flash a console window on Windows.
pub fn command(ffmpeg: &Path) -> Command {
    let mut cmd = Command::new(ffmpeg);
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

/// Common flags in front of an RTSP input: TCP transport (reliable over Wi-Fi),
/// low-latency demuxing, and a socket timeout so a dead camera is noticed.
pub fn rtsp_input_args(url: &str, low_latency: bool) -> Vec<String> {
    let mut a: Vec<String> = vec!["-hide_banner".into(), "-nostdin".into(), "-loglevel".into(), "error".into()];
    if low_latency {
        a.extend(["-fflags", "nobuffer", "-flags", "low_delay"].map(String::from));
        // Tapo cameras' RTP timestamps jump ~400 ms forward then back about once
        // a second, which makes the fps filter emit bursts of duplicates and then
        // drop real frames. Timestamp frames by arrival instead.
        a.extend(["-use_wallclock_as_timestamps", "1"].map(String::from));
    }
    a.extend(["-rtsp_transport", "tcp", "-timeout", "8000000"].map(String::from));
    a.push("-i".into());
    a.push(url.into());
    a
}

/// Tie a long-running FFmpeg to Tapestry's lifetime: on Windows, children
/// are added to a job object that the OS kills when Tapestry exits — even if
/// it crashes — so no stray FFmpeg keeps streaming in the background.
pub fn adopt(child: &std::process::Child) {
    job::adopt(child);
}

#[cfg(windows)]
mod job {
    use std::os::windows::io::AsRawHandle;
    use std::sync::OnceLock;

    type Handle = *mut core::ffi::c_void;

    #[repr(C)]
    #[derive(Default)]
    struct BasicLimit {
        per_process_user_time_limit: i64,
        per_job_user_time_limit: i64,
        limit_flags: u32,
        minimum_working_set_size: usize,
        maximum_working_set_size: usize,
        active_process_limit: u32,
        affinity: usize,
        priority_class: u32,
        scheduling_class: u32,
    }

    #[repr(C)]
    #[derive(Default)]
    struct IoCounters([u64; 6]);

    #[repr(C)]
    #[derive(Default)]
    struct ExtendedLimit {
        basic: BasicLimit,
        io: IoCounters,
        process_memory_limit: usize,
        job_memory_limit: usize,
        peak_process_memory_used: usize,
        peak_job_memory_used: usize,
    }

    const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x2000;
    const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION: i32 = 9;

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateJobObjectW(attrs: *mut core::ffi::c_void, name: *const u16) -> Handle;
        fn SetInformationJobObject(job: Handle, class: i32, info: *mut core::ffi::c_void, len: u32) -> i32;
        fn AssignProcessToJobObject(job: Handle, process: Handle) -> i32;
    }

    struct Job(Handle);
    // The handle is only ever passed to thread-safe kernel calls.
    unsafe impl Send for Job {}
    unsafe impl Sync for Job {}

    static JOB: OnceLock<Option<Job>> = OnceLock::new();

    fn job() -> Option<&'static Job> {
        JOB.get_or_init(|| unsafe {
            let h = CreateJobObjectW(std::ptr::null_mut(), std::ptr::null());
            if h.is_null() {
                return None;
            }
            let mut info = ExtendedLimit::default();
            info.basic.limit_flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let ok = SetInformationJobObject(
                h,
                JOB_OBJECT_EXTENDED_LIMIT_INFORMATION,
                &mut info as *mut ExtendedLimit as *mut core::ffi::c_void,
                std::mem::size_of::<ExtendedLimit>() as u32,
            );
            // The handle is intentionally never closed: the OS closes it when
            // Tapestry exits, which is what kills the children.
            (ok != 0).then_some(Job(h))
        })
        .as_ref()
    }

    pub fn adopt(child: &std::process::Child) {
        if let Some(j) = job() {
            unsafe {
                AssignProcessToJobObject(j.0, child.as_raw_handle() as Handle);
            }
        }
    }
}

#[cfg(not(windows))]
mod job {
    pub fn adopt(_child: &std::process::Child) {}
}

/// `ffmpeg -version` first line, to confirm the binary works.
pub fn version(ffmpeg: &Path) -> Option<String> {
    let out = command(ffmpeg).arg("-version").stdout(Stdio::piped()).output().ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    s.lines().next().map(|l| l.trim().to_string())
}

/// Duration of a media file, by asking ffmpeg to open it and reading the
/// "Duration:" line it prints. Works on fragmented MP4s still being written
/// only approximately, which is fine for a scrub bar.
pub fn probe_duration(ffmpeg: &Path, file: &Path) -> Option<Duration> {
    let out = command(ffmpeg).args(["-hide_banner", "-nostdin", "-i"]).arg(file).stderr(Stdio::piped()).output().ok()?;
    parse_duration_line(&String::from_utf8_lossy(&out.stderr))
}

pub fn parse_duration_line(stderr: &str) -> Option<Duration> {
    let line = stderr.lines().find(|l| l.trim_start().starts_with("Duration:"))?;
    let v = line.trim_start().strip_prefix("Duration:")?.trim();
    let v = v.split(',').next()?.trim();
    parse_hms(v)
}

/// `01:02:03.45` → Duration.
pub fn parse_hms(v: &str) -> Option<Duration> {
    let mut parts = v.split(':');
    let h: f64 = parts.next()?.parse().ok()?;
    let m: f64 = parts.next()?.parse().ok()?;
    let s: f64 = parts.next()?.parse().ok()?;
    let total = h * 3600.0 + m * 60.0 + s;
    if total.is_finite() && total >= 0.0 {
        Some(Duration::from_secs_f64(total))
    } else {
        None
    }
}

/// Grab one full-resolution frame from a camera into a JPEG.
pub fn snapshot(ffmpeg: &Path, rtsp_url: &str, out: &Path) -> Result<(), String> {
    if let Some(dir) = out.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let mut cmd = command(ffmpeg);
    cmd.args(rtsp_input_args(rtsp_url, false))
        .args(["-frames:v", "1", "-q:v", "2", "-update", "1", "-y"])
        .arg(out)
        .stderr(Stdio::piped());
    let o = cmd.output().map_err(|e| format!("couldn't start ffmpeg: {e}"))?;
    if o.status.success() && out.is_file() {
        Ok(())
    } else {
        Err(last_line(&String::from_utf8_lossy(&o.stderr)).unwrap_or_else(|| "snapshot failed".into()))
    }
}

pub fn last_line(s: &str) -> Option<String> {
    s.lines().rev().map(str::trim).find(|l| !l.is_empty()).map(String::from)
}

/// Turn ffmpeg's terse errors into something a person can act on.
pub fn friendly_error(raw: &str) -> String {
    let l = raw.to_ascii_lowercase();
    if l.contains("401") || l.contains("unauthorized") {
        "Wrong camera username/password (use the Camera Account from the Tapo app)".into()
    } else if l.contains("connection refused") {
        "Camera refused the video connection — is the Camera Account set up?".into()
    } else if l.contains("timed out") || l.contains("timeout") || l.contains("no route") || l.contains("unreachable") {
        "Camera not reachable — check it's on and on the same network".into()
    } else if l.contains("453") || l.contains("not enough bandwidth") || l.contains("503") {
        "Camera is busy (too many viewers?)".into()
    } else if l.contains("does not contain any stream") {
        "Camera isn't sending sound (is its microphone turned off in the Tapo app?)".into()
    } else if l.is_empty() {
        "Stream ended".into()
    } else {
        raw.trim().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        let s = "Input #0, mov,mp4\n  Duration: 00:01:02.50, start: 0.000000, bitrate: 900 kb/s\n";
        assert_eq!(parse_duration_line(s), Some(Duration::from_millis(62_500)));
        assert_eq!(parse_duration_line("  Duration: N/A, bitrate: N/A"), None);
        assert_eq!(parse_hms("1:00:00"), Some(Duration::from_secs(3600)));
    }

    #[test]
    fn live_input_uses_arrival_timestamps() {
        let pos = |a: &[String], s: &str| a.iter().position(|x| x == s);
        let live = rtsp_input_args("rtsp://cam/stream2", true);
        let flag = pos(&live, "-use_wallclock_as_timestamps").expect("live args set wallclock timestamps");
        assert_eq!(live[flag + 1], "1");
        assert!(flag < pos(&live, "-i").unwrap(), "must be an input option");
        let snap = rtsp_input_args("rtsp://cam/stream1", false);
        assert!(pos(&snap, "-use_wallclock_as_timestamps").is_none());
    }

    #[test]
    fn friendly() {
        assert!(friendly_error("[rtsp @ 0x1] method DESCRIBE failed: 401 Unauthorized").contains("password"));
        assert!(friendly_error("Connection timed out").contains("reachable"));
    }
}
