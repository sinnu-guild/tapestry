//! Live (or file) video → NV12 frames, via an FFmpeg child process.
//!
//! Each `VideoStream` owns a supervisor thread that starts FFmpeg, reads
//! fixed-size raw frames from its stdout, notices stalls, and reconnects with
//! back-off. The UI only ever asks "what's the latest frame?".
//!
//! Frames stay in the decoder's own YUV (NV12) form and the GPU turns them
//! into RGB while drawing: converting 3K video to RGBA in FFmpeg cost more CPU
//! than decoding it, and RGBA is 2.7× the bytes to pipe and upload.

use crate::color::ColorInfo;
use crate::ffmpeg;
use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub struct Frame {
    pub width: u32,
    pub height: u32,
    /// NV12: the full-size Y plane (width*height bytes), then the half-size
    /// interleaved U/V plane (width*height/2 bytes).
    pub nv12: Vec<u8>,
    /// How to turn `nv12` into RGB.
    pub color: ColorInfo,
    /// Increments with every frame; lets the UI skip re-uploading.
    pub seq: u64,
}

impl Frame {
    pub fn y_plane(&self) -> &[u8] {
        &self.nv12[..(self.width * self.height) as usize]
    }

    pub fn uv_plane(&self) -> &[u8] {
        &self.nv12[(self.width * self.height) as usize..]
    }

    /// Bytes in one NV12 frame of this size (both sides must be even).
    pub fn nv12_len(width: u32, height: u32) -> usize {
        (width * height) as usize * 3 / 2
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum StreamStatus {
    Connecting,
    Live,
    Retrying {
        error: String,
        attempt: u32,
    },
    Paused,
    /// Input finished (files only).
    Ended,
    Stopped,
    /// FFmpeg itself is missing or broken.
    Failed(String),
}

pub type Waker = Arc<dyn Fn() + Send + Sync>;

#[derive(Clone)]
pub struct StreamSpec {
    /// Everything up to and including `-i <input>`.
    pub input_args: Vec<String>,
    pub width: u32,
    pub height: u32,
    /// Output frame rate. `None` passes every decoded frame through at the
    /// source's own rate (no duplicates, no drops), which is what live
    /// cameras want: their rates differ (15–25 fps), and a fixed rate above
    /// the source's repeats frames unevenly, which reads as judder.
    pub fps: Option<u32>,
    /// Reconnect when the input ends (live cameras) or stop (files).
    pub reconnect: bool,
}

impl StreamSpec {
    pub fn rtsp(url: &str, width: u32, height: u32, fps: Option<u32>) -> Self {
        StreamSpec { input_args: ffmpeg::rtsp_input_args(url, true), width, height, fps, reconnect: true }
    }

    /// Play a recorded file in real time from `start`, at `speed`x.
    pub fn file(path: &std::path::Path, start: Duration, speed: f32, width: u32, height: u32, fps: u32) -> Self {
        let mut a: Vec<String> = vec!["-hide_banner".into(), "-nostdin".into(), "-loglevel".into(), "error".into()];
        a.push("-readrate".into());
        a.push(format!("{:.2}", speed.clamp(0.25, 16.0)));
        if start > Duration::ZERO {
            a.push("-ss".into());
            a.push(format!("{:.3}", start.as_secs_f64()));
        }
        a.push("-i".into());
        a.push(path.to_string_lossy().into_owned());
        StreamSpec { input_args: a, width, height, fps: Some(fps), reconnect: false }
    }

    /// Output frame size; NV12 needs both sides even.
    fn size(&self) -> (u32, u32) {
        (self.width.max(2) & !1, self.height.max(2) & !1)
    }

    fn args(&self) -> Vec<String> {
        let (w, h) = self.size();
        let mut a = Vec::new();
        if cfg!(windows) {
            // Decode on the GPU (any vendor). FFmpeg falls back to software
            // decoding by itself if the GPU can't take this stream.
            a.extend(["-hwaccel", "d3d11va"].map(String::from));
        }
        a.extend(self.input_args.iter().cloned());
        // Errors as before, plus the one-time stream summary (tagged by level
        // so they can be told apart): it says which colour range we're getting.
        match a.iter().position(|x| x == "-loglevel") {
            Some(i) if i + 1 < a.len() => a[i + 1] = LOGLEVEL.into(),
            _ => {
                a.insert(0, LOGLEVEL.into());
                a.insert(0, "-loglevel".into());
            }
        }
        a.insert(0, "-nostats".into());
        a.extend(["-an", "-sn", "-dn", "-vf"].map(String::from));
        let fps = self.fps.map(|f| format!("fps={},", f.max(1))).unwrap_or_default();
        a.push(format!("{fps}scale={w}:{h}:force_original_aspect_ratio=decrease:flags=area,pad={w}:{h}:(ow-iw)/2:(oh-ih)/2"));
        // Output exactly the frames the filters produce: no duplicating or
        // dropping based on (unreliable) camera timestamps. A microsecond time
        // base keeps passed-through arrival times from colliding.
        a.extend(["-pix_fmt", "nv12", "-fps_mode", "passthrough", "-enc_time_base", "1:1000000"].map(String::from));
        a.extend(["-f", "rawvideo", "pipe:1"].map(String::from));
        a
    }
}

/// FFmpeg log level for video: info, with every line tagged `[info]`, `[error]`…
const LOGLEVEL: &str = "+level+info";

/// One line of FFmpeg's stderr that matters to us.
#[derive(Debug, PartialEq)]
enum LogLine {
    /// Explains a failure (shown to the user, made friendly).
    Error(String),
    /// Informational, e.g. the output stream summary.
    Info(String),
}

/// Sort a stderr line. Untagged lines (streams run at `-loglevel error`)
/// count as errors, as they always have.
fn parse_log_line(raw: &str) -> Option<LogLine> {
    let t = raw.trim();
    if is_noise(t) {
        return None;
    }
    const TAGS: [(&str, Option<bool>); 7] = [
        ("[error] ", Some(true)),
        ("[fatal] ", Some(true)),
        ("[panic] ", Some(true)),
        ("[info] ", Some(false)),
        ("[warning] ", None),
        ("[verbose] ", None),
        ("[debug] ", None),
    ];
    let tagged = TAGS.iter().filter_map(|(tag, kind)| t.find(tag).map(|i| (i, *tag, *kind))).min_by_key(|(i, ..)| *i);
    let Some((i, tag, kind)) = tagged else { return Some(LogLine::Error(t.to_string())) };
    // Keep FFmpeg's "[rtsp @ 0x…]" context, drop the level tag.
    let text = format!("{}{}", &t[..i], &t[i + tag.len()..]).trim().to_string();
    match kind {
        _ if is_noise(&text) => None,
        Some(true) => Some(LogLine::Error(text)),
        Some(false) => Some(LogLine::Info(text)),
        None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_passes_frames_through() {
        let a = StreamSpec::rtsp("rtsp://cam/stream2", 640, 360, Some(12)).args();
        let mode = a.iter().position(|x| x == "-fps_mode").expect("fps_mode set");
        assert_eq!(a[mode + 1], "passthrough");
        assert!(mode < a.iter().position(|x| x == "rawvideo").unwrap(), "must come before the output");
    }

    #[test]
    fn native_rate_has_no_fps_filter() {
        let vf = |fps| {
            let a = StreamSpec::rtsp("rtsp://cam/stream1", 2880, 1620, fps).args();
            a[a.iter().position(|x| x == "-vf").unwrap() + 1].clone()
        };
        assert!(!vf(None).contains("fps="), "{}", vf(None));
        assert!(vf(Some(10)).starts_with("fps=10,"), "{}", vf(Some(10)));
    }

    #[test]
    fn stderr_noise_is_not_an_error() {
        assert!(is_noise("[rawvideo @ 0000025] Application provided invalid, non monotonically increasing dts to muxer in stream 0: 7 >= 7"));
        assert!(is_noise("Last message repeated 6 times"));
        assert!(!is_noise("[rtsp @ 0x1] method DESCRIBE failed: 401 Unauthorized"));
    }

    #[test]
    fn hardware_decode_is_an_input_option() {
        let a = StreamSpec::rtsp("rtsp://cam/stream1", 2880, 1620, None).args();
        if cfg!(windows) {
            let hw = a.iter().position(|x| x == "-hwaccel").expect("hwaccel on Windows");
            assert!(hw < a.iter().position(|x| x == "-i").unwrap(), "must be an input option");
        }
    }

    #[test]
    fn nv12_out_with_tagged_logs() {
        let a = StreamSpec::rtsp("rtsp://cam/stream1", 2881, 1620, None).args();
        let after = |s: &str| a[a.iter().position(|x| x == s).unwrap() + 1].clone();
        assert_eq!(after("-pix_fmt"), "nv12");
        assert_eq!(after("-loglevel"), LOGLEVEL);
        assert_eq!(a.iter().filter(|x| *x == "-loglevel").count(), 1);
        assert!(a.contains(&"-nostats".to_string()));
        assert!(after("-vf").contains("scale=2880:1620"), "odd sizes are rounded down for NV12");
    }

    #[test]
    fn log_lines() {
        use LogLine::*;
        assert_eq!(
            parse_log_line("[rtsp @ 0x1] [error] method DESCRIBE failed: 401 Unauthorized"),
            Some(Error("[rtsp @ 0x1] method DESCRIBE failed: 401 Unauthorized".into()))
        );
        assert_eq!(parse_log_line("[fatal] Error opening input files: X"), Some(Error("Error opening input files: X".into())));
        assert_eq!(
            parse_log_line("[info]   Stream #0:0: Video: rawvideo (NV12 / 0x3231564E), nv12(pc, progressive), 64x36"),
            Some(Info("Stream #0:0: Video: rawvideo (NV12 / 0x3231564E), nv12(pc, progressive), 64x36".into()))
        );
        assert_eq!(parse_log_line("[swscaler @ 0x2] [warning] deprecated pixel format used"), None);
        assert_eq!(parse_log_line("[rawvideo @ 0x3] [error] Application provided invalid, non monotonically increasing dts"), None);
        assert_eq!(parse_log_line("[error]     Last message repeated 2 times"), None);
        // Untagged (audio runs at plain `-loglevel error`).
        assert_eq!(parse_log_line("Connection timed out"), Some(Error("Connection timed out".into())));
        assert_eq!(parse_log_line("   "), None);
    }
}

pub(crate) struct Shared {
    frame: Mutex<Option<Arc<Frame>>>,
    status: Mutex<StreamStatus>,
    stop: AtomicBool,
    paused: AtomicBool,
    seq: AtomicU64,
    /// Chunks since the current FFmpeg run started (for playback position).
    run_frames: AtomicU64,
    last_frame: Mutex<Instant>,
    /// FFmpeg's summary of the current run's output video stream (empty
    /// until it prints one); says which colour range the frames are in.
    out_stream: Mutex<String>,
    waker: Option<Waker>,
}

/// What a supervisor runs: the full FFmpeg arguments, and the fixed size of
/// each chunk it reads from stdout (one video frame, or a slice of audio).
pub(crate) struct Run {
    pub args: Vec<String>,
    pub chunk_len: usize,
    pub reconnect: bool,
    /// Hold the first chunk of each run until FFmpeg has described its
    /// output stream (video: the colour range must be known to draw it).
    pub wait_for_out_stream: bool,
}

/// Receives every chunk FFmpeg delivers, on the reader thread. It may hand
/// back a spent buffer, which the reader fills next instead of allocating
/// (a 3K frame is several MB, many times a second).
pub(crate) type Sink = Box<dyn FnMut(Vec<u8>) -> Option<Vec<u8>> + Send>;

/// An FFmpeg child kept alive by a supervisor thread: starts it, reads
/// fixed-size chunks into a sink, notices stalls, reconnects with back-off.
pub(crate) struct Supervisor {
    shared: Arc<Shared>,
    handle: Option<JoinHandle<()>>,
}

impl Supervisor {
    pub(crate) fn start(
        ffmpeg: PathBuf,
        run: Run,
        waker: Option<Waker>,
        make_sink: impl FnOnce(Arc<Shared>) -> Sink,
    ) -> Self {
        let shared = Arc::new(Shared {
            frame: Mutex::new(None),
            status: Mutex::new(StreamStatus::Connecting),
            stop: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            seq: AtomicU64::new(0),
            run_frames: AtomicU64::new(0),
            last_frame: Mutex::new(Instant::now()),
            out_stream: Mutex::new(String::new()),
            waker,
        });
        let sink = make_sink(shared.clone());
        let s2 = shared.clone();
        let handle = thread::Builder::new()
            .name("tapestry-stream".into())
            .spawn(move || supervise(ffmpeg, run, s2, sink))
            .expect("spawn stream thread");
        Supervisor { shared, handle: Some(handle) }
    }

    pub(crate) fn status(&self) -> StreamStatus {
        self.shared.status.lock().unwrap().clone()
    }

    pub(crate) fn frames_this_run(&self) -> u64 {
        self.shared.run_frames.load(Ordering::Relaxed)
    }

    pub(crate) fn set_paused(&self, paused: bool) {
        self.shared.paused.store(paused, Ordering::Relaxed);
    }

    pub(crate) fn is_paused(&self) -> bool {
        self.shared.paused.load(Ordering::Relaxed)
    }

    pub(crate) fn request_stop(&self) {
        self.shared.stop.store(true, Ordering::Relaxed);
    }

    pub(crate) fn stop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        // Don't block the UI thread on join; the supervisor exits within ~200ms.
        self.handle.take();
    }
}

pub struct VideoStream {
    sup: Supervisor,
}

impl VideoStream {
    pub fn start(ffmpeg: PathBuf, spec: StreamSpec, waker: Option<Waker>) -> Self {
        let (w, h) = spec.size();
        let run = Run {
            args: spec.args(),
            chunk_len: Frame::nv12_len(w, h),
            reconnect: spec.reconnect,
            wait_for_out_stream: true,
        };
        // Keep only the newest frame; the UI asks for it when it paints.
        let sup = Supervisor::start(ffmpeg, run, waker, move |shared| {
            Box::new(move |nv12| {
                let color = ColorInfo::from_stream_line(&shared.out_stream.lock().unwrap()).unwrap_or_default();
                let seq = shared.seq.fetch_add(1, Ordering::Relaxed) + 1;
                let old = shared.frame.lock().unwrap().replace(Arc::new(Frame { width: w, height: h, nv12, color, seq }));
                if let Some(wk) = &shared.waker {
                    wk();
                }
                // Recycle the frame this one replaced, unless the UI still holds it.
                old.and_then(|f| Arc::try_unwrap(f).ok()).map(|f| f.nv12)
            })
        });
        VideoStream { sup }
    }

    pub fn latest(&self) -> Option<Arc<Frame>> {
        self.sup.shared.frame.lock().unwrap().clone()
    }

    pub fn status(&self) -> StreamStatus {
        self.sup.status()
    }

    /// Frames delivered by the current FFmpeg run.
    pub fn frames_this_run(&self) -> u64 {
        self.sup.frames_this_run()
    }

    pub fn set_paused(&self, paused: bool) {
        self.sup.set_paused(paused);
    }

    pub fn is_paused(&self) -> bool {
        self.sup.is_paused()
    }

    /// Ask the stream to stop without waiting (pair with `stop` to join).
    pub fn request_stop(&self) {
        self.sup.request_stop();
    }

    /// Stop and wait until FFmpeg is gone.
    pub fn stop(&mut self) {
        self.sup.stop();
    }
}

/// FFmpeg's message when the input lacks the kind of stream we asked for.
const NO_STREAM: &str = "does not contain any stream";

/// FFmpeg stderr lines that never explain a failure: the harmless note that
/// two frames arrived in the same instant, and FFmpeg's "repeated" summaries.
/// Skipping them keeps them from masking (or posing as) the real error.
fn is_noise(line: &str) -> bool {
    line.is_empty() || line.contains("non monotonically increasing dts") || line.contains("Last message repeated")
}

fn set_status(shared: &Shared, s: StreamStatus) {
    let mut cur = shared.status.lock().unwrap();
    if *cur != s {
        *cur = s;
        drop(cur);
        if let Some(w) = &shared.waker {
            w();
        }
    }
}

fn sleep_unless_stopped(shared: &Shared, d: Duration) {
    let until = Instant::now() + d;
    while Instant::now() < until && !shared.stop.load(Ordering::Relaxed) && !shared.paused.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_millis(100));
    }
}

fn kill(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn supervise(ffmpeg_path: PathBuf, spec: Run, shared: Arc<Shared>, sink: Sink) {
    let frame_len = spec.chunk_len;
    let sink = Arc::new(Mutex::new(sink));
    let mut attempt: u32 = 0;

    loop {
        if shared.stop.load(Ordering::Relaxed) {
            break;
        }
        if shared.paused.load(Ordering::Relaxed) {
            set_status(&shared, StreamStatus::Paused);
            thread::sleep(Duration::from_millis(150));
            continue;
        }

        set_status(&shared, StreamStatus::Connecting);
        shared.run_frames.store(0, Ordering::Relaxed);
        let mut cmd = ffmpeg::command(&ffmpeg_path);
        cmd.args(&spec.args).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                set_status(&shared, StreamStatus::Failed(format!("couldn't start FFmpeg ({}): {e}", ffmpeg_path.display())));
                return;
            }
        };

        ffmpeg::adopt(&child);
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let last_err = Arc::new(Mutex::new(String::new()));
        let le = last_err.clone();
        shared.out_stream.lock().unwrap().clear();
        let s_err = shared.clone();
        let err_thread = thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                match parse_log_line(&line) {
                    Some(LogLine::Error(t)) => {
                        let mut last = le.lock().unwrap();
                        // Keep the line that explains a missing stream; FFmpeg
                        // follows it with generic "Error opening output files" lines.
                        if !last.contains(NO_STREAM) {
                            *last = t;
                        }
                    }
                    // The output's summary (input lines name the camera's codec, not rawvideo).
                    Some(LogLine::Info(t)) if t.contains("Video: rawvideo") => *s_err.out_stream.lock().unwrap() = t,
                    _ => {}
                }
            }
        });

        let started = Instant::now();
        *shared.last_frame.lock().unwrap() = started;
        let s2 = shared.clone();
        let sink2 = sink.clone();
        let mut wait_for_out_stream = spec.wait_for_out_stream;
        let reader = thread::spawn(move || {
            let mut out = BufReader::with_capacity(frame_len.clamp(4096, 8 << 20), stdout);
            let mut spare: Option<Vec<u8>> = None;
            loop {
                let mut buf = spare.take().filter(|b| b.len() == frame_len).unwrap_or_else(|| vec![0u8; frame_len]);
                if out.read_exact(&mut buf).is_err() {
                    break;
                }
                if std::mem::take(&mut wait_for_out_stream) {
                    // FFmpeg prints the summary before writing any video, but
                    // our two pipe readers can race; this is a brief wait at most.
                    let until = Instant::now() + Duration::from_secs(1);
                    while s2.out_stream.lock().unwrap().is_empty() && Instant::now() < until {
                        thread::sleep(Duration::from_millis(2));
                    }
                }
                s2.run_frames.fetch_add(1, Ordering::Relaxed);
                *s2.last_frame.lock().unwrap() = Instant::now();
                set_status(&s2, StreamStatus::Live);
                spare = (sink2.lock().unwrap())(buf);
                if s2.stop.load(Ordering::Relaxed) {
                    break;
                }
            }
        });

        // Watch the run.
        let mut went_live = false;
        loop {
            thread::sleep(Duration::from_millis(150));
            if shared.stop.load(Ordering::Relaxed) || shared.paused.load(Ordering::Relaxed) {
                kill(&mut child);
                break;
            }
            if let Ok(Some(_)) = child.try_wait() {
                break;
            }
            let since = shared.last_frame.lock().unwrap().elapsed();
            let got_frames = shared.run_frames.load(Ordering::Relaxed) > 0;
            went_live |= got_frames;
            let stalled = if got_frames { since > Duration::from_secs(12) } else { since > Duration::from_secs(25) };
            if stalled {
                *last_err.lock().unwrap() =
                    if got_frames { "Video froze — reconnecting".into() } else { "Timed out connecting".into() };
                kill(&mut child);
                break;
            }
        }
        let _ = reader.join();
        let _ = err_thread.join();
        let _ = child.wait();
        // A short input can finish between two checks above.
        went_live |= shared.run_frames.load(Ordering::Relaxed) > 0;

        if shared.stop.load(Ordering::Relaxed) {
            break;
        }
        if shared.paused.load(Ordering::Relaxed) {
            continue;
        }
        // The input has nothing we asked for (e.g. sound requested from a camera
        // whose microphone is off): retrying won't help.
        if !went_live && last_err.lock().unwrap().contains(NO_STREAM) {
            let err = last_err.lock().unwrap().clone();
            set_status(&shared, StreamStatus::Failed(ffmpeg::friendly_error(&err)));
            return;
        }
        if !spec.reconnect {
            let err = last_err.lock().unwrap().clone();
            if went_live || err.is_empty() {
                set_status(&shared, StreamStatus::Ended);
            } else {
                set_status(&shared, StreamStatus::Failed(ffmpeg::friendly_error(&err)));
            }
            return;
        }

        if went_live && started.elapsed() > Duration::from_secs(30) {
            attempt = 0;
        }
        attempt += 1;
        let err = last_err.lock().unwrap().clone();
        set_status(&shared, StreamStatus::Retrying { error: ffmpeg::friendly_error(&err), attempt });
        let backoff = Duration::from_secs([1u64, 2, 4, 8, 15][(attempt as usize - 1).min(4)]);
        sleep_unless_stopped(&shared, backoff);
    }
    set_status(&shared, StreamStatus::Stopped);
}
