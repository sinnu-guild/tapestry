//! The engine ties it together and runs independently of the window: camera
//! links, motion state, recording decisions, motion log and cleanup. It keeps
//! working while Tapestry is minimised.

use crate::clips::{self, MotionSpan};
use crate::config::{Camera, Config, Quality, RecordMode};
use crate::link::{CameraLink, LinkEvent, PtzCommand};
use crate::onvif::{DeviceInfo, Preset};
use crate::recorder::{Namer, Recorder, RecorderEvent};
use crate::stream::Waker;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

#[derive(Debug, Clone, PartialEq)]
pub enum LinkState {
    Connecting,
    Online,
    /// Online, but motion alerts aren't available from this camera.
    NoEvents(String),
    Offline(String),
    AuthFailed(String),
}

#[derive(Debug, Clone)]
pub struct CameraState {
    pub name: String,
    pub slug: String,
    pub link: LinkState,
    pub info: Option<DeviceInfo>,
    pub has_ptz: bool,
    pub presets: Vec<Preset>,
    pub motion: bool,
    pub motion_since: Option<SystemTime>,
    pub last_motion: Option<SystemTime>,
    pub motion_count_today: u32,
    pub recording: Option<PathBuf>,
    pub record_error: Option<String>,
    pub manual_until: Option<SystemTime>,
    pub ptz_error: Option<(SystemTime, String)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Motion,
    Info,
    Warn,
}

#[derive(Debug, Clone)]
pub struct LogEntry {
    pub at: SystemTime,
    pub camera: Option<usize>,
    pub level: LogLevel,
    pub text: String,
}

enum Cmd {
    Link(usize, LinkEvent),
    RecordNow(usize, Duration),
    StopManual(usize),
    Shutdown,
}

pub struct EngineOptions {
    pub ffmpeg: Option<PathBuf>,
    pub namer: Namer,
    pub waker: Option<Waker>,
}

pub struct Engine {
    cfg: Config,
    links: Vec<CameraLink>,
    state: Arc<Mutex<Vec<CameraState>>>,
    log: Arc<Mutex<VecDeque<LogEntry>>>,
    log_version: Arc<AtomicU64>,
    tx: Sender<Cmd>,
    thread: Option<JoinHandle<()>>,
}

const LOG_MAX: usize = 500;
/// If a camera never sends "motion ended", give up after this long.
const MOTION_STALE: Duration = Duration::from_secs(5 * 60);

impl Engine {
    pub fn start(cfg: Config, opts: EngineOptions) -> Engine {
        let state = Arc::new(Mutex::new(
            cfg.cameras
                .iter()
                .map(|c| CameraState {
                    name: c.name.clone(),
                    slug: c.slug(),
                    link: LinkState::Connecting,
                    info: None,
                    has_ptz: false,
                    presets: Vec::new(),
                    motion: false,
                    motion_since: None,
                    last_motion: None,
                    motion_count_today: 0,
                    recording: None,
                    record_error: None,
                    manual_until: None,
                    ptz_error: None,
                })
                .collect::<Vec<_>>(),
        ));
        let log = Arc::new(Mutex::new(VecDeque::new()));
        let log_version = Arc::new(AtomicU64::new(0));
        let (tx, rx) = mpsc::channel::<Cmd>();

        let links = cfg
            .cameras
            .iter()
            .enumerate()
            .map(|(i, cam)| {
                let tx = tx.clone();
                CameraLink::start(cam.clone(), move |ev| {
                    let _ = tx.send(Cmd::Link(i, ev));
                })
            })
            .collect();

        let worker = Worker {
            cfg: cfg.clone(),
            ffmpeg: opts.ffmpeg,
            namer: opts.namer,
            waker: opts.waker,
            state: state.clone(),
            log: log.clone(),
            log_version: log_version.clone(),
            cams: cfg.cameras.iter().map(|_| CamRuntime::default()).collect(),
        };
        let thread = thread::Builder::new().name("tapestry-engine".into()).spawn(move || worker.run(rx)).expect("spawn engine");

        Engine { cfg, links, state, log, log_version, tx, thread: Some(thread) }
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    pub fn cameras(&self) -> Vec<CameraState> {
        self.state.lock().unwrap().clone()
    }

    pub fn log_version(&self) -> u64 {
        self.log_version.load(Ordering::Relaxed)
    }

    pub fn log(&self) -> Vec<LogEntry> {
        self.log.lock().unwrap().iter().cloned().collect()
    }

    pub fn ptz(&self, camera: usize, cmd: PtzCommand) {
        if let Some(l) = self.links.get(camera) {
            l.ptz(cmd);
        }
    }

    /// Record now regardless of motion, for `dur`.
    pub fn record_now(&self, camera: usize, dur: Duration) {
        let _ = self.tx.send(Cmd::RecordNow(camera, dur));
    }

    pub fn stop_manual(&self, camera: usize) {
        let _ = self.tx.send(Cmd::StopManual(camera));
    }

    /// Feed a synthetic link event (tests, and the "test motion" button).
    pub fn inject(&self, camera: usize, ev: LinkEvent) {
        let _ = self.tx.send(Cmd::Link(camera, ev));
    }

    /// Stop everything, finishing any recording cleanly. Blocks briefly.
    pub fn shutdown(&mut self) {
        for l in &self.links {
            l.shutdown();
        }
        let _ = self.tx.send(Cmd::Shutdown);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[derive(Default)]
struct CamRuntime {
    recorder: Option<Recorder>,
    motion_started_unix: Option<i64>,
    last_motion_instant: Option<Instant>,
    last_true_instant: Option<Instant>,
    manual_until: Option<Instant>,
}

struct Worker {
    cfg: Config,
    ffmpeg: Option<PathBuf>,
    namer: Namer,
    waker: Option<Waker>,
    state: Arc<Mutex<Vec<CameraState>>>,
    log: Arc<Mutex<VecDeque<LogEntry>>>,
    log_version: Arc<AtomicU64>,
    cams: Vec<CamRuntime>,
}

impl Worker {
    fn wake(&self) {
        if let Some(w) = &self.waker {
            w();
        }
    }

    fn push_log(&self, camera: Option<usize>, level: LogLevel, text: String) {
        let mut log = self.log.lock().unwrap();
        log.push_back(LogEntry { at: SystemTime::now(), camera, level, text });
        while log.len() > LOG_MAX {
            log.pop_front();
        }
        drop(log);
        self.log_version.fetch_add(1, Ordering::Relaxed);
    }

    fn cam_name(&self, i: usize) -> String {
        self.cfg.cameras.get(i).map(|c| c.name.clone()).unwrap_or_default()
    }

    fn run(mut self, rx: mpsc::Receiver<Cmd>) {
        if self.ffmpeg.is_none() && self.cfg.record_mode != RecordMode::Off {
            self.push_log(None, LogLevel::Warn, "FFmpeg not found — recording is disabled until it's installed".into());
        }
        let mut last_cleanup: Option<Instant> = None;
        let mut today = self.namer.as_ref()().0;

        loop {
            match rx.recv_timeout(Duration::from_millis(250)) {
                Ok(Cmd::Shutdown) | Err(RecvTimeoutError::Disconnected) => break,
                Ok(Cmd::Link(i, ev)) => self.on_link(i, ev),
                Ok(Cmd::RecordNow(i, d)) => {
                    if let Some(c) = self.cams.get_mut(i) {
                        c.manual_until = Some(Instant::now() + d);
                        self.state.lock().unwrap()[i].manual_until = Some(SystemTime::now() + d);
                        self.push_log(
                            Some(i),
                            LogLevel::Info,
                            format!("{}: manual recording for {} min", self.cam_name(i), d.as_secs() / 60),
                        );
                    }
                }
                Ok(Cmd::StopManual(i)) => {
                    if let Some(c) = self.cams.get_mut(i) {
                        c.manual_until = None;
                        self.state.lock().unwrap()[i].manual_until = None;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
            }

            // Reset per-day counters at local midnight.
            let day = self.namer.as_ref()().0;
            if day != today {
                today = day;
                for s in self.state.lock().unwrap().iter_mut() {
                    s.motion_count_today = 0;
                }
            }

            self.tick_motion_staleness();
            self.tick_recorders();

            if last_cleanup.map_or(true, |t| t.elapsed() > Duration::from_secs(30 * 60)) {
                last_cleanup = Some(Instant::now());
                self.cleanup();
            }
        }

        // Shutdown: close motion spans and finish recordings.
        for i in 0..self.cams.len() {
            self.end_motion(i);
            if let Some(mut r) = self.cams[i].recorder.take() {
                r.shutdown();
            }
        }
    }

    fn on_link(&mut self, i: usize, ev: LinkEvent) {
        if i >= self.cams.len() {
            return;
        }
        let name = self.cam_name(i);
        match ev {
            LinkEvent::Connecting => {
                let mut st = self.state.lock().unwrap();
                // Only show "connecting" the first time; a quiet re-subscribe
                // or reconnect shouldn't flicker the status or spam the log.
                if st[i].info.is_none() && !matches!(st[i].link, LinkState::Offline(_) | LinkState::AuthFailed(_)) {
                    st[i].link = LinkState::Connecting;
                }
            }
            LinkEvent::Connected { info, has_ptz, presets } => {
                let was = self.state.lock().unwrap()[i].link.clone();
                {
                    let mut st = self.state.lock().unwrap();
                    st[i].link = LinkState::Online;
                    st[i].has_ptz = has_ptz;
                    st[i].presets = presets;
                    st[i].info = Some(info.clone());
                }
                if was != LinkState::Online {
                    let model = if info.model.is_empty() { String::new() } else { format!(" ({})", info.model) };
                    self.push_log(Some(i), LogLevel::Info, format!("{name}{model} connected"));
                }
            }
            LinkEvent::EventsUnavailable(e) => {
                self.state.lock().unwrap()[i].link = LinkState::NoEvents(e.clone());
                self.push_log(Some(i), LogLevel::Warn, format!("{name}: motion alerts unavailable ({e})"));
            }
            LinkEvent::Offline(e) => {
                let was = self.state.lock().unwrap()[i].link.clone();
                self.state.lock().unwrap()[i].link = LinkState::Offline(e.clone());
                self.set_motion(i, false);
                if !matches!(was, LinkState::Offline(_)) {
                    self.push_log(Some(i), LogLevel::Warn, format!("{name} offline: {e}"));
                }
            }
            LinkEvent::AuthFailed(e) => {
                let was = self.state.lock().unwrap()[i].link.clone();
                self.state.lock().unwrap()[i].link = LinkState::AuthFailed(e.clone());
                if !matches!(was, LinkState::AuthFailed(_)) {
                    self.push_log(
                        Some(i),
                        LogLevel::Warn,
                        format!("{name}: login rejected — check the Camera Account username/password"),
                    );
                }
            }
            LinkEvent::Detection { kind, active } => {
                if kind == "Motion" {
                    self.set_motion(i, active);
                } else if active {
                    self.push_log(Some(i), LogLevel::Motion, format!("{name}: {kind}"));
                }
            }
            LinkEvent::PtzError(e) => {
                self.state.lock().unwrap()[i].ptz_error = Some((SystemTime::now(), e));
            }
        }
        self.wake();
    }

    fn set_motion(&mut self, i: usize, active: bool) {
        let was = self.state.lock().unwrap()[i].motion;
        if active {
            self.cams[i].last_true_instant = Some(Instant::now());
            self.cams[i].last_motion_instant = Some(Instant::now());
        }
        if active == was {
            return;
        }
        let now = SystemTime::now();
        if active {
            self.cams[i].motion_started_unix = Some(clips::system_time_to_unix(now));
            {
                let mut st = self.state.lock().unwrap();
                st[i].motion = true;
                st[i].motion_since = Some(now);
                st[i].last_motion = Some(now);
                st[i].motion_count_today += 1;
            }
            self.push_log(Some(i), LogLevel::Motion, format!("Motion: {}", self.cam_name(i)));
        } else {
            self.cams[i].last_motion_instant = Some(Instant::now());
            {
                let mut st = self.state.lock().unwrap();
                st[i].motion = false;
                st[i].motion_since = None;
                st[i].last_motion = Some(now);
            }
            self.end_motion(i);
        }
        self.wake();
    }

    fn end_motion(&mut self, i: usize) {
        if let Some(start) = self.cams[i].motion_started_unix.take() {
            let end = clips::system_time_to_unix(SystemTime::now()).max(start);
            let path = clips::motion_log_path(&self.cfg.recordings_dir, &self.cfg.cameras[i].slug());
            let _ = clips::append_motion(&path, MotionSpan { start, end });
        }
    }

    fn tick_motion_staleness(&mut self) {
        for i in 0..self.cams.len() {
            let motion = self.state.lock().unwrap()[i].motion;
            if motion && self.cams[i].last_true_instant.map_or(true, |t| t.elapsed() > MOTION_STALE) {
                self.set_motion(i, false);
            }
        }
    }
}
