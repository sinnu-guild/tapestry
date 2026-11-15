//! App state and the per-frame loop. Views live in `views.rs`, the clips
//! side panel in `clips_panel.rs`, settings in `settings.rs`.

use crate::settings::SettingsDraft;
use crate::{theme, util};
use eframe::egui::{self, Color32, RichText, TextureHandle, TextureOptions};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};
use tapestry_core::clips::{Clip, MotionSpan};
use tapestry_core::{
    ffmpeg, CameraState, Config, Engine, EngineOptions, Frame, LinkState, PtzCommand, Quality, StreamSpec, VideoStream,
};

/// Grid tiles use the camera's low-quality stream at this size.
pub const GRID_SIZE: [u32; 2] = [640, 360];
/// The focused camera uses the high-quality stream at this size.
pub const FOCUS_SIZE: [u32; 2] = [1920, 1080];
pub const FOCUS_FPS: u32 = 15;
pub const PLAYBACK_SIZE: [u32; 2] = [1600, 900];
pub const PLAYBACK_FPS: u32 = 15;

/// A video stream plus the GPU texture it feeds.
pub struct LiveView {
    pub stream: Option<VideoStream>,
    pub tex: Option<TextureHandle>,
    pub seq: u64,
}

impl LiveView {
    pub fn new(stream: Option<VideoStream>) -> Self {
        LiveView { stream, tex: None, seq: 0 }
    }

    /// Upload the newest frame (if any) to the texture.
    pub fn upload(&mut self, ctx: &egui::Context, name: &str) {
        let Some(frame) = self.stream.as_ref().and_then(|s| s.latest()) else { return };
        if frame.seq == self.seq {
            return;
        }
        self.seq = frame.seq;
        let img = to_image(&frame);
        match &mut self.tex {
            Some(t) => t.set(img, TextureOptions::LINEAR),
            None => self.tex = Some(ctx.load_texture(name, img, TextureOptions::LINEAR)),
        }
    }
}

fn to_image(f: &Frame) -> egui::ColorImage {
    egui::ColorImage::from_rgba_unmultiplied([f.width as usize, f.height as usize], &f.rgba)
}

pub struct Focus {
    pub cam: usize,
    pub view: LiveView,
    /// Digital zoom (1 = off) and the centre of the visible area in 0..1 uv.
    pub zoom: f32,
    pub center: egui::Pos2,
    /// Last click-to-aim point, for a little ripple.
    pub aim_flash: Option<(egui::Pos2, Instant)>,
}

pub struct Playback {
    pub clip: Clip,
    pub cam: Option<usize>,
    pub length: Option<Duration>,
    pub offset: Duration,
    pub speed: f32,
    pub paused_at: Option<Duration>,
    pub view: LiveView,
}

impl Playback {
    pub fn position(&self) -> Duration {
        if let Some(p) = self.paused_at {
            return p;
        }
        let frames = self.view.stream.as_ref().map(|s| s.frames_this_run()).unwrap_or(0);
        self.offset + Duration::from_secs_f64(frames as f64 / PLAYBACK_FPS as f64)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Center {
    Grid,
    Focus,
    Playback,
}

pub enum TestState {
    Running,
    Ok(String),
    Err(String),
}

pub struct ClipsCache {
    pub clips: Vec<Clip>,
    pub motion: Vec<(String, Vec<MotionSpan>)>,
    pub scanned: Option<Instant>,
    pub usage: u64,
}

pub struct TapestryApp {
    pub cfg: Config,
    pub ffmpeg: Option<PathBuf>,
    pub ffmpeg_version: Option<String>,
    pub engine: Option<Engine>,
    pub cams: Vec<CameraState>,
    pub tiles: Vec<LiveView>,
    pub center: Center,
    pub focus: Option<Focus>,
    pub playback: Option<Playback>,

    pub show_clips: bool,
    pub clips_cam: Option<String>,
    pub clips_day: String,
    pub clips: ClipsCache,

    pub settings: Option<SettingsDraft>,
    pub show_activity: bool,
    pub log_seen: u64,
    pub toasts: Vec<(Instant, String, Color32)>,
    pub jobs: Arc<Mutex<Vec<(String, Color32)>>>,

    pub prev_motion: Vec<bool>,
    pub title: String,
    pub ptz_sent: Option<(usize, (f32, f32))>,
    pub minimized: bool,
    pub closing: bool,
}

impl TapestryApp {
    pub fn new(cc: &eframe::CreationContext<'_>, cfg: Config, warnings: Vec<String>) -> Self {
        theme::apply(&cc.egui_ctx);
        let mut app = TapestryApp {
            cfg,
            ffmpeg: None,
            ffmpeg_version: None,
            engine: None,
            cams: Vec::new(),
            tiles: Vec::new(),
            center: Center::Grid,
            focus: None,
            playback: None,
            show_clips: false,
            clips_cam: None,
            clips_day: util::today(),
            clips: ClipsCache { clips: Vec::new(), motion: Vec::new(), scanned: None, usage: 0 },
            settings: None,
            show_activity: false,
            log_seen: 0,
            toasts: Vec::new(),
            jobs: Arc::new(Mutex::new(Vec::new())),
            prev_motion: Vec::new(),
            title: String::new(),
            ptz_sent: None,
            minimized: false,
            closing: false,
        };
        for w in warnings {
            app.toast(format!("Settings: {w}"), theme::WARN);
        }
        app.restart(&cc.egui_ctx);
        if app.cfg.cameras.is_empty() {
            app.settings = Some(SettingsDraft::new(&app.cfg, true));
        }
        app
    }

    /// (Re)start the engine and all streams from `self.cfg`.
    pub fn restart(&mut self, ctx: &egui::Context) {
        self.stop_ptz();
        if let Some(mut e) = self.engine.take() {
            e.shutdown();
        }
        self.tiles.clear();
        self.focus = None;
        self.playback = None;
        self.center = Center::Grid;

        self.ffmpeg = ffmpeg::locate(&self.cfg.ffmpeg);
        self.ffmpeg_version = self.ffmpeg.as_deref().and_then(ffmpeg::version);
        if self.ffmpeg.is_some() && self.ffmpeg_version.is_none() {
            // Found a file but it doesn't run.
            self.ffmpeg = None;
        }

        let waker = waker(ctx);
        self.engine = Some(Engine::start(
            self.cfg.clone(),
            EngineOptions { ffmpeg: self.ffmpeg.clone(), namer: util::local_namer(), waker: Some(waker.clone()) },
        ));
        for cam in &self.cfg.cameras {
            let stream = self.ffmpeg.clone().map(|ff| {
                VideoStream::start(
                    ff,
                    StreamSpec::rtsp(&cam.rtsp_url(Quality::Sub), GRID_SIZE[0], GRID_SIZE[1], self.cfg.grid_fps),
                    Some(waker.clone()),
                )
            });
            self.tiles.push(LiveView::new(stream));
        }
        self.prev_motion = vec![false; self.cfg.cameras.len()];
        self.cams = self.engine.as_ref().map(|e| e.cameras()).unwrap_or_default();
        self.clips.scanned = None;
    }

    pub fn toast(&mut self, text: impl Into<String>, color: Color32) {
        self.toasts.push((Instant::now(), text.into(), color));
    }

    // ----- focus / playback -------------------------------------------------

    pub fn open_focus(&mut self, ctx: &egui::Context, cam: usize) {
        if cam >= self.cfg.cameras.len() {
            return;
        }
        self.close_focus();
        self.playback = None;
        let stream = self.ffmpeg.clone().map(|ff| {
            VideoStream::start(
                ff,
                StreamSpec::rtsp(&self.cfg.cameras[cam].rtsp_url(Quality::Main), FOCUS_SIZE[0], FOCUS_SIZE[1], FOCUS_FPS),
                Some(waker(ctx)),
            )
        });
        // Free the camera's grid stream while focused (cameras limit viewers).
        if let Some(s) = self.tiles.get(cam).and_then(|t| t.stream.as_ref()) {
            s.set_paused(true);
        }
        self.focus = Some(Focus { cam, view: LiveView::new(stream), zoom: 1.0, center: egui::pos2(0.5, 0.5), aim_flash: None });
        self.center = Center::Focus;
    }

    pub fn close_focus(&mut self) {
        self.stop_ptz();
        if let Some(f) = self.focus.take() {
            if let Some(s) = self.tiles.get(f.cam).and_then(|t| t.stream.as_ref()) {
                s.set_paused(false);
            }
        }
        if self.center == Center::Focus {
            self.center = Center::Grid;
        }
    }

    pub fn open_clip(&mut self, ctx: &egui::Context, clip: Clip, at: Duration) {
        let Some(ff) = self.ffmpeg.clone() else {
            self.toast("FFmpeg is needed to play clips", theme::WARN);
            return;
        };
        self.close_focus();
        let cam = self.cfg.cameras.iter().position(|c| c.slug() == clip.camera_slug);
        let length = ffmpeg::probe_duration(&ff, &clip.path).or_else(|| util::clip_len(&clip));
        let at = match length {
            Some(l) if at >= l => Duration::ZERO,
            _ => at,
        };
        let stream = VideoStream::start(
            ff,
            StreamSpec::file(&clip.path, at, 1.0, PLAYBACK_SIZE[0], PLAYBACK_SIZE[1], PLAYBACK_FPS),
            Some(waker(ctx)),
        );
        self.playback =
            Some(Playback { clip, cam, length, offset: at, speed: 1.0, paused_at: None, view: LiveView::new(Some(stream)) });
        self.center = Center::Playback;
    }

    /// Restart playback at `at` (seek / speed change / resume).
    pub fn seek(&mut self, ctx: &egui::Context, at: Duration, keep_paused: bool) {
        let Some(ff) = self.ffmpeg.clone() else { return };
        let Some(pb) = self.playback.as_mut() else { return };
        let at = match pb.length {
            Some(l) => at.min(l.saturating_sub(Duration::from_millis(300))),
            None => at,
        };
        pb.offset = at;
        if keep_paused && pb.paused_at.is_some() {
            // Show the frame at the new position, then hold.
            pb.paused_at = Some(at);
        } else {
            pb.paused_at = None;
        }
        pb.view.stream = Some(VideoStream::start(
            ff,
            StreamSpec::file(&pb.clip.path, at, pb.speed, PLAYBACK_SIZE[0], PLAYBACK_SIZE[1], PLAYBACK_FPS),
            Some(waker(ctx)),
        ));
        pb.view.seq = 0;
    }

    pub fn close_playback(&mut self) {
        self.playback = None;
        if self.center == Center::Playback {
            self.center = Center::Grid;
        }
    }

    // ----- camera actions -----------------------------------------------------

    pub fn ptz(&mut self, cam: usize, dir: Option<(f32, f32)>) {
        let want = dir.map(|d| (cam, d));
        if want == self.ptz_sent {
            return;
        }
        if let Some(e) = &self.engine {
            match dir {
                Some((pan, tilt)) => e.ptz(cam, PtzCommand::Move { pan, tilt }),
                None => {
                    if let Some((c, _)) = self.ptz_sent {
                        e.ptz(c, PtzCommand::Stop);
                    }
                }
            }
        }
        self.ptz_sent = want;
    }

    pub fn stop_ptz(&mut self) {
        if let (Some(e), Some((c, _))) = (&self.engine, self.ptz_sent) {
            e.ptz(c, PtzCommand::Stop);
        }
        self.ptz_sent = None;
    }

    pub fn snapshot(&mut self, cam: usize) {
        let Some(ff) = self.ffmpeg.clone() else {
            self.toast("FFmpeg is needed for snapshots", theme::WARN);
            return;
        };
        let Some(c) = self.cfg.cameras.get(cam).cloned() else { return };
        let stamp = chrono::Local::now().format("%Y-%m-%d_%H-%M-%S").to_string();
        let out = self.cfg.recordings_dir.join(c.slug()).join("snapshots").join(format!("{stamp}.jpg"));
        let jobs = self.jobs.clone();
        self.toast(format!("Taking a full-quality snapshot of {}…", c.name), theme::MUTED);
        std::thread::spawn(move || {
            let msg = match ffmpeg::snapshot(&ff, &c.rtsp_url(Quality::Main), &out) {
                Ok(()) => (format!("Snapshot saved: {}", out.display()), theme::LIVE),
                Err(e) => (format!("Snapshot failed: {}", ffmpeg::friendly_error(&e)), theme::MOTION),
            };
            jobs.lock().unwrap().push(msg);
        });
    }

    pub fn record_now(&mut self, cam: usize) {
        if let Some(e) = &self.engine {
            let manual = self.cams.get(cam).and_then(|c| c.manual_until).is_some();
            if manual {
                e.stop_manual(cam);
            } else {
                e.record_now(cam, Duration::from_secs(5 * 60));
            }
        }
    }

    pub fn save_config(&mut self) {
        if let Err(e) = self.cfg.save() {
            self.toast(format!("Couldn't save settings: {e}"), theme::MOTION);
        }
    }

    // ----- per-frame ------------------------------------------------------------

    fn poll(&mut self, ctx: &egui::Context) {
        if let Some(e) = &self.engine {
            self.cams = e.cameras();
        }
        let finished = std::mem::take(&mut *self.jobs.lock().unwrap());
        for (msg, color) in finished {
            self.toast(msg, color);
        }
        self.toasts.retain(|(t, _, _)| t.elapsed() < Duration::from_secs(6));

        // Pause grid streams while minimised; motion + recording carry on.
        let minimized = ctx.input(|i| i.viewport().minimized).unwrap_or(false);
        if minimized != self.minimized {
            self.minimized = minimized;
            let focused = self.focus.as_ref().map(|f| f.cam);
            for (i, t) in self.tiles.iter().enumerate() {
                if let Some(s) = &t.stream {
                    s.set_paused(minimized || Some(i) == focused);
                }
            }
            if let Some(f) = &self.focus {
                if let Some(s) = &f.view.stream {
                    s.set_paused(minimized);
                }
            }
        }

        // Motion started somewhere?
        let window_focused = ctx.input(|i| i.viewport().focused).unwrap_or(true);
        let mut new_motion = Vec::new();
        for (i, c) in self.cams.iter().enumerate() {
            let was = self.prev_motion.get(i).copied().unwrap_or(false);
            if c.motion && !was {
                new_motion.push(c.name.clone());
            }
        }
        self.prev_motion = self.cams.iter().map(|c| c.motion).collect();
        if !new_motion.is_empty() && self.cfg.flash_taskbar && !window_focused {
            ctx.send_viewport_cmd(egui::ViewportCommand::RequestUserAttention(egui::UserAttentionType::Informational));
        }

        // Window title doubles as an alert you can see from the taskbar.
        let moving: Vec<&str> = self.cams.iter().filter(|c| c.motion).map(|c| c.name.as_str()).collect();
        let title =
            if moving.is_empty() { "Tapestry".to_string() } else { format!("● Motion: {} — Tapestry", moving.join(", ")) };
        if title != self.title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.title = title;
        }
    }

    fn upload(&mut self, ctx: &egui::Context) {
        for (i, t) in self.tiles.iter_mut().enumerate() {
            t.upload(ctx, &format!("tile-{i}"));
        }
        if let Some(f) = &mut self.focus {
            f.view.upload(ctx, "focus");
        }
        if let Some(p) = &mut self.playback {
            p.view.upload(ctx, "playback");
            // Seeking while paused: show the first frame at the new spot, then hold.
            let got_frame = p.view.stream.as_ref().map_or(false, |s| s.frames_this_run() > 0);
            if p.paused_at.is_some() && p.view.seq > 0 && got_frame {
                p.view.stream = None;
            }
        }
    }

    fn keys(&mut self, ctx: &egui::Context) {
        if ctx.wants_keyboard_input() {
            return;
        }
        let (esc, digits, c_key, s_key, r_key, p_key, f_key, space, left, right) = ctx.input(|i| {
            let digits: Vec<usize> = [
                egui::Key::Num1,
                egui::Key::Num2,
                egui::Key::Num3,
                egui::Key::Num4,
                egui::Key::Num5,
                egui::Key::Num6,
                egui::Key::Num7,
                egui::Key::Num8,
                egui::Key::Num9,
            ]
            .iter()
            .enumerate()
            .filter(|(_, k)| i.key_pressed(**k))
            .map(|(n, _)| n)
            .collect();
            (
                i.key_pressed(egui::Key::Escape) || i.key_pressed(egui::Key::Num0),
                digits,
                i.key_pressed(egui::Key::C),
                i.key_pressed(egui::Key::S) && i.modifiers.ctrl,
                i.key_pressed(egui::Key::R),
                i.key_pressed(egui::Key::P),
                i.key_pressed(egui::Key::F11) || (i.key_pressed(egui::Key::F) && !i.modifiers.any()),
                i.key_pressed(egui::Key::Space),
                i.key_pressed(egui::Key::ArrowLeft),
                i.key_pressed(egui::Key::ArrowRight),
            )
        });

        if esc {
            let fullscreen = ctx.input(|i| i.viewport().fullscreen).unwrap_or(false);
            if fullscreen {
                ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
            } else {
                self.close_focus();
                self.close_playback();
            }
        }
        if let Some(&d) = digits.first() {
            self.open_focus(ctx, d);
        }
        if c_key {
            self.show_clips = !self.show_clips;
        }
        if p_key {
            self.toggle_pin(ctx);
        }
        if f_key {
            let fs = ctx.input(|i| i.viewport().fullscreen).unwrap_or(false);
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(!fs));
        }
        match self.center {
            Center::Focus => {
                let cam = self.focus.as_ref().map(|f| f.cam).unwrap_or(0);
                if s_key {
                    self.snapshot(cam);
                }
                if r_key {
                    self.record_now(cam);
                }
            }
            Center::Playback => {
                if space {
                    self.toggle_pause(ctx);
                }
                if left || right {
                    if let Some(pos) = self.playback.as_ref().map(|p| p.position()) {
                        let to = if left { pos.saturating_sub(Duration::from_secs(10)) } else { pos + Duration::from_secs(10) };
                        self.seek(ctx, to, true);
                    }
                }
            }
            Center::Grid => {}
        }
    }

    pub fn toggle_pause(&mut self, ctx: &egui::Context) {
        let Some(pb) = self.playback.as_mut() else { return };
        let ended = pb.view.stream.as_ref().map_or(false, |s| matches!(s.status(), tapestry_core::StreamStatus::Ended));
        if ended {
            self.seek(ctx, Duration::ZERO, false);
        } else if let Some(at) = pb.paused_at {
            self.seek(ctx, at, false);
        } else {
            pb.paused_at = Some(pb.position());
            pb.view.stream = None;
        }
    }

    pub fn toggle_pin(&mut self, ctx: &egui::Context) {
        self.cfg.always_on_top = !self.cfg.always_on_top;
        ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(if self.cfg.always_on_top {
            egui::WindowLevel::AlwaysOnTop
        } else {
            egui::WindowLevel::Normal
        }));
        self.save_config();
        self.toast(if self.cfg.always_on_top { "Pinned on top" } else { "Unpinned" }, theme::MUTED);
    }

    // ----- chrome -----------------------------------------------------------------
}
