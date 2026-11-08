//! Live (or file) video → RGBA frames, via an FFmpeg child process.
//!
//! Each `VideoStream` owns a supervisor thread that starts FFmpeg, reads
//! fixed-size raw frames from its stdout, notices stalls, and reconnects with
//! back-off. The UI only ever asks "what's the latest frame?".

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
    /// Tightly packed RGBA, width*height*4 bytes.
    pub rgba: Vec<u8>,
    /// Increments with every frame; lets the UI skip re-uploading.
    pub seq: u64,
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
    pub fps: u32,
    /// Reconnect when the input ends (live cameras) or stop (files).
    pub reconnect: bool,
}

impl StreamSpec {
    pub fn rtsp(url: &str, width: u32, height: u32, fps: u32) -> Self {
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
        StreamSpec { input_args: a, width, height, fps, reconnect: false }
    }

    fn args(&self) -> Vec<String> {
        let (w, h) = (self.width, self.height);
        let mut a = self.input_args.clone();
        a.extend(["-an", "-sn", "-dn", "-vf"].map(String::from));
        a.push(format!(
            "fps={fps},scale={w}:{h}:force_original_aspect_ratio=decrease:flags=bilinear,pad={w}:{h}:(ow-iw)/2:(oh-ih)/2",
            fps = self.fps.max(1)
        ));
        a.extend(["-pix_fmt", "rgba", "-f", "rawvideo", "pipe:1"].map(String::from));
        a
    }
}

struct Shared {
    frame: Mutex<Option<Arc<Frame>>>,
    status: Mutex<StreamStatus>,
    stop: AtomicBool,
    paused: AtomicBool,
    seq: AtomicU64,
    /// Frames since the current FFmpeg run started (for playback position).
    run_frames: AtomicU64,
    last_frame: Mutex<Instant>,
    waker: Option<Waker>,
}

pub struct VideoStream {
    shared: Arc<Shared>,
    handle: Option<JoinHandle<()>>,
    pub spec_fps: u32,
}

impl VideoStream {
    pub fn start(ffmpeg: PathBuf, spec: StreamSpec, waker: Option<Waker>) -> Self {
        let shared = Arc::new(Shared {
            frame: Mutex::new(None),
            status: Mutex::new(StreamStatus::Connecting),
            stop: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            seq: AtomicU64::new(0),
            run_frames: AtomicU64::new(0),
            last_frame: Mutex::new(Instant::now()),
            waker,
        });
        let fps = spec.fps;
        let s2 = shared.clone();
        let handle = thread::Builder::new()
            .name("tapestry-stream".into())
            .spawn(move || supervise(ffmpeg, spec, s2))
            .expect("spawn stream thread");
        VideoStream { shared, handle: Some(handle), spec_fps: fps }
    }

    pub fn latest(&self) -> Option<Arc<Frame>> {
        self.shared.frame.lock().unwrap().clone()
    }

    pub fn status(&self) -> StreamStatus {
        self.shared.status.lock().unwrap().clone()
    }

    /// Frames delivered by the current FFmpeg run.
    pub fn frames_this_run(&self) -> u64 {
        self.shared.run_frames.load(Ordering::Relaxed)
    }

    pub fn set_paused(&self, paused: bool) {
        self.shared.paused.store(paused, Ordering::Relaxed);
    }

    pub fn is_paused(&self) -> bool {
        self.shared.paused.load(Ordering::Relaxed)
    }

    /// Ask the stream to stop without waiting (pair with `stop` to join).
    pub fn request_stop(&self) {
        self.shared.stop.store(true, Ordering::Relaxed);
    }

    /// Stop and wait until FFmpeg is gone.
    pub fn stop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for VideoStream {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        // Don't block the UI thread on join; the supervisor exits within ~200ms.
        self.handle.take();
    }
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

fn supervise(ffmpeg_path: PathBuf, spec: StreamSpec, shared: Arc<Shared>) {
    let frame_len = (spec.width * spec.height * 4) as usize;
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
        cmd.args(spec.args()).stdout(Stdio::piped()).stderr(Stdio::piped());
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
        let err_thread = thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                let t = line.trim();
                if !t.is_empty() {
                    *le.lock().unwrap() = t.to_string();
                }
            }
        });

        let started = Instant::now();
        *shared.last_frame.lock().unwrap() = started;
        let s2 = shared.clone();
        let (w, h) = (spec.width, spec.height);
        let reader = thread::spawn(move || {
            let mut out = BufReader::with_capacity(frame_len.min(8 << 20), stdout);
            loop {
                let mut buf = vec![0u8; frame_len];
                if out.read_exact(&mut buf).is_err() {
                    break;
                }
                let seq = s2.seq.fetch_add(1, Ordering::Relaxed) + 1;
                s2.run_frames.fetch_add(1, Ordering::Relaxed);
                *s2.frame.lock().unwrap() = Some(Arc::new(Frame { width: w, height: h, rgba: buf, seq }));
                *s2.last_frame.lock().unwrap() = Instant::now();
                {
                    let mut st = s2.status.lock().unwrap();
                    if *st != StreamStatus::Live {
                        *st = StreamStatus::Live;
                    }
                }
                if let Some(wk) = &s2.waker {
                    wk();
                }
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

        if shared.stop.load(Ordering::Relaxed) {
            break;
        }
        if shared.paused.load(Ordering::Relaxed) {
            continue;
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
