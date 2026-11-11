//! Records one camera's main stream to MP4 files with FFmpeg (no
//! re-encoding of video, so it's cheap on CPU).
//!
//! Files are fragmented MP4, so even if the PC loses power mid-clip the file
//! up to that point still plays.

use crate::ffmpeg;
use crate::time::{civil_from_days, unix_now};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// Produces (day folder, file stem) for a new recording, e.g.
/// ("2026-11-20", "14-05-33"). The app supplies a local-time namer.
pub type Namer = Arc<dyn Fn() -> (String, String) + Send + Sync>;

/// UTC-based namer (used when no local clock helper is available).
pub fn utc_namer() -> Namer {
    Arc::new(|| {
        let now = unix_now();
        let (y, m, d) = civil_from_days(now.div_euclid(86400));
        let s = now.rem_euclid(86400);
        (format!("{y:04}-{m:02}-{d:02}"), format!("{:02}-{:02}-{:02}", s / 3600, (s % 3600) / 60, s % 60))
    })
}

#[derive(Debug, Clone, PartialEq)]
pub enum RecorderEvent {
    Started(PathBuf),
    Finished(PathBuf),
    Failed(String),
}

pub struct Recorder {
    ffmpeg: PathBuf,
    /// Everything up to and including `-i <input>`.
    input_args: Vec<String>,
    dir: PathBuf,
    namer: Namer,
    pub max_clip: Duration,
    child: Option<Child>,
    current: Option<PathBuf>,
    started: Instant,
    stderr_tail: Arc<Mutex<String>>,
    retry_at: Option<Instant>,
    failures: u32,
}

impl Recorder {
    /// `dir` is this camera's folder, e.g. `Videos/Tapestry/front-gate`.
    pub fn new(ffmpeg: PathBuf, rtsp_url: String, dir: PathBuf, namer: Namer, max_clip: Duration) -> Self {
        Recorder {
            ffmpeg,
            // No -nostdin: we stop recordings gracefully by sending 'q'.
            input_args: ["-hide_banner", "-loglevel", "error", "-rtsp_transport", "tcp", "-timeout", "8000000", "-i"]
                .iter()
                .map(|s| s.to_string())
                .chain(std::iter::once(rtsp_url))
                .collect(),
            dir,
            namer,
            max_clip: max_clip.max(Duration::from_secs(10)),
            child: None,
            current: None,
            started: Instant::now(),
            stderr_tail: Arc::new(Mutex::new(String::new())),
            retry_at: None,
            failures: 0,
        }
    }

    /// Replace the input (tests use a local file instead of a camera).
    pub fn with_input_args(mut self, args: Vec<String>) -> Self {
        self.input_args = args;
        self
    }

    pub fn is_recording(&self) -> bool {
        self.child.is_some()
    }

    pub fn current(&self) -> Option<&Path> {
        self.current.as_deref()
    }

    /// Drive the recorder: call regularly (every few hundred ms) with
    /// whether recording is wanted right now.
    pub fn update(&mut self, want: bool) -> Vec<RecorderEvent> {
        let mut ev = Vec::new();

        // Did FFmpeg exit on its own?
        if let Some(child) = self.child.as_mut() {
            if let Ok(Some(status)) = child.try_wait() {
                self.child = None;
                let path = self.current.take();
                let ran = self.started.elapsed();
                let size = path.as_ref().and_then(|p| std::fs::metadata(p).ok()).map(|m| m.len()).unwrap_or(0);
                if ran < Duration::from_secs(4) || size < 4096 {
                    let err = self.stderr_tail.lock().unwrap().clone();
                    if let Some(p) = &path {
                        if size < 4096 {
                            let _ = std::fs::remove_file(p);
                        }
                    }
                    self.fail(
                        &mut ev,
                        if err.is_empty() { format!("FFmpeg exited ({status})") } else { ffmpeg::friendly_error(&err) },
                    );
                } else if let Some(p) = path {
                    self.failures = 0;
                    ev.push(RecorderEvent::Finished(p));
                }
            }
        }

        if want {
            if self.child.is_some() && self.started.elapsed() >= self.max_clip {
                if let Some(p) = self.stop_child() {
                    ev.push(RecorderEvent::Finished(p));
                }
            }
            if self.child.is_none() && self.retry_at.map_or(true, |t| Instant::now() >= t) {
                match self.start() {
                    Ok(p) => ev.push(RecorderEvent::Started(p)),
                    Err(e) => self.fail(&mut ev, e),
                }
            }
        } else {
            self.retry_at = None;
            if self.child.is_some() {
                if let Some(p) = self.stop_child() {
                    ev.push(RecorderEvent::Finished(p));
                }
            }
        }
        ev
    }

    fn fail(&mut self, ev: &mut Vec<RecorderEvent>, msg: String) {
        self.failures += 1;
        let secs = [3u64, 6, 15, 30, 60][(self.failures as usize - 1).min(4)];
        self.retry_at = Some(Instant::now() + Duration::from_secs(secs));
        ev.push(RecorderEvent::Failed(msg));
    }

    fn next_path(&self) -> std::io::Result<PathBuf> {
        let (day, stem) = (self.namer)();
        let dir = self.dir.join(day);
        std::fs::create_dir_all(&dir)?;
        let mut p = dir.join(format!("{stem}.mp4"));
        let mut n = 2;
        while p.exists() {
            p = dir.join(format!("{stem}-{n}.mp4"));
            n += 1;
        }
        Ok(p)
    }

    fn start(&mut self) -> Result<PathBuf, String> {
        let path = self.next_path().map_err(|e| format!("can't create recordings folder: {e}"))?;
        let mut cmd = ffmpeg::command(&self.ffmpeg);
        cmd.args(&self.input_args)
            .args(["-map", "0:v:0", "-map", "0:a:0?", "-c:v", "copy", "-c:a", "aac", "-b:a", "64k"])
            .args(["-t", &(self.max_clip.as_secs() + 10).to_string()])
            .args(["-movflags", "+frag_keyframe+empty_moov+default_base_moof", "-f", "mp4", "-y"])
            .arg(&path)
            .stdin(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().map_err(|e| format!("couldn't start FFmpeg: {e}"))?;
        ffmpeg::adopt(&child);
        let tail = self.stderr_tail.clone();
        tail.lock().unwrap().clear();
        if let Some(err) = child.stderr.take() {
            thread::spawn(move || {
                for line in BufReader::new(err).lines().map_while(Result::ok) {
                    if !line.trim().is_empty() {
                        *tail.lock().unwrap() = line.trim().to_string();
                    }
                }
            });
        }
        self.child = Some(child);
        self.current = Some(path.clone());
        self.started = Instant::now();
        Ok(path)
    }

    /// Ask FFmpeg to finish the file ('q'), then make sure it's gone.
    fn stop_child(&mut self) -> Option<PathBuf> {
        let mut child = self.child.take()?;
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(b"q");
            let _ = stdin.flush();
        }
        let deadline = Instant::now() + Duration::from_secs(4);
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(50)),
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break;
                }
            }
        }
        self.failures = 0;
        let p = self.current.take()?;
        match std::fs::metadata(&p) {
            Ok(m) if m.len() >= 4096 => Some(p),
            _ => {
                let _ = std::fs::remove_file(&p);
                None
            }
        }
    }

    /// Stop now (app exit, settings change).
    pub fn shutdown(&mut self) -> Option<PathBuf> {
        self.stop_child()
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.stop_child();
    }
}
