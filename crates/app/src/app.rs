//! App state and the per-frame loop. Views live in `views.rs`, the clips
//! side panel in `clips_panel.rs`, settings in `settings.rs`.

use crate::settings::SettingsDraft;
use crate::video_gl::{VideoRenderer, VideoTexture};
use crate::{theme, util};
use eframe::egui::{self, Color32, RichText};
use eframe::glow;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};
use tapestry_core::clips::{Clip, MotionSpan};
use tapestry_core::{
    ffmpeg, AudioStream, CameraState, Config, Engine, EngineOptions, LinkState, PtzCommand, Quality, StreamSpec, StreamStatus,
    VideoStream,
};

/// Grid tiles use the camera's low-quality stream at this size.
pub const GRID_SIZE: [u32; 2] = [640, 360];
/// The focused camera uses the high-quality stream at this size, at the
/// camera's own frame rate.
pub const FOCUS_SIZE: [u32; 2] = [1920, 1080];
pub const PLAYBACK_SIZE: [u32; 2] = [1600, 900];
pub const PLAYBACK_FPS: u32 = 15;

/// A video stream plus the GPU texture it feeds.
pub struct LiveView {
    pub stream: Option<VideoStream>,
    pub tex: Option<VideoTexture>,
    pub seq: u64,
}

impl LiveView {
    pub fn new(stream: Option<VideoStream>) -> Self {
        LiveView { stream, tex: None, seq: 0 }
    }

    /// Upload the newest frame (if any) straight from the stream's buffer to the GPU.
    pub fn upload(&mut self, gpu: Option<(&glow::Context, &VideoRenderer)>) {
        let Some(frame) = self.stream.as_ref().and_then(|s| s.latest()) else { return };
        if frame.seq == self.seq {
            return;
        }
        self.seq = frame.seq;
        if let Some((gl, renderer)) = gpu {
            renderer.upload(gl, &mut self.tex, &frame);
        }
    }
}

/// Size to decode the focused camera at: its native main-stream size when
/// ONVIF reported one (so digital zoom has real detail), else 1080p.
fn focus_size(main: Option<(u32, u32)>) -> (u32, u32) {
    match main {
        Some((w, h)) if w >= 320 && h >= 180 => {
            let (w, h) = (w.min(3840), h.min(2160));
            (w & !1, h & !1)
        }
        _ => (FOCUS_SIZE[0], FOCUS_SIZE[1]),
    }
}

pub struct Focus {
    pub cam: usize,
    pub view: LiveView,
    /// Digital zoom (1 = off) and the centre of the visible area in 0..1 uv.
    pub zoom: f32,
    pub center: egui::Pos2,
    /// Last click-to-aim point, for a little ripple.
    pub aim_flash: Option<(egui::Pos2, Instant)>,
    /// The camera's grid stream keeps running (and is shown) until the
    /// full-quality stream delivers its first frame; then it's paused.
    pub grid_released: bool,
    /// The camera's sound, while it's open (and sound is on).
    pub audio: Option<AudioStream>,
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
    /// Sound volume in percent, shared live with the audio stream.
    pub volume: Arc<AtomicU32>,
    /// Draws video frames on the GPU (None if OpenGL couldn't set it up).
    pub video: Option<Arc<VideoRenderer>>,
}

impl TapestryApp {
    pub fn new(cc: &eframe::CreationContext<'_>, cfg: Config, warnings: Vec<String>) -> Self {
        theme::apply(&cc.egui_ctx);
        let video = match cc.gl.as_deref().map(VideoRenderer::new) {
            Some(Ok(r)) => Ok(Arc::new(r)),
            Some(Err(e)) => Err(e),
            None => Err("no OpenGL context".to_string()),
        };
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
            volume: Arc::new(AtomicU32::new(0)),
            video: video.as_ref().ok().cloned(),
        };
        if let Err(e) = video {
            app.toast(format!("Can't show video: {e}"), theme::MOTION);
        }
        app.volume.store(app.cfg.sound_volume, Ordering::Relaxed);
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
                    StreamSpec::rtsp(&cam.rtsp_url(Quality::Sub), GRID_SIZE[0], GRID_SIZE[1], self.cfg.grid_fps_cap()),
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
        let (w, h) = focus_size(self.cams.get(cam).and_then(|c| c.main_size));
        let stream = self.ffmpeg.clone().map(|ff| {
            VideoStream::start(
                ff,
                StreamSpec::rtsp(&self.cfg.cameras[cam].rtsp_url(Quality::Main), w, h, None),
                Some(waker(ctx)),
            )
        });
        // The grid stream stays live as a stand-in until the full-quality
        // picture arrives; `upload` pauses it then (cameras limit viewers).
        self.focus = Some(Focus {
            cam,
            view: LiveView::new(stream),
            zoom: 1.0,
            center: egui::pos2(0.5, 0.5),
            aim_flash: None,
            grid_released: false,
            audio: None,
        });
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

    /// Start the open camera's sound, if sound is on and nothing's stopping it.
    pub fn start_audio(&mut self, ctx: &egui::Context) {
        let Some(ff) = self.ffmpeg.clone() else { return };
        let Some(f) = self.focus.as_mut() else { return };
        // Wait until the grid stand-in is released, so the camera never
        // has more than three viewers (recorder, full quality, sound).
        if !self.cfg.sound || f.audio.is_some() || !f.grid_released || self.minimized {
            return;
        }
        let Some(cam) = self.cfg.cameras.get(f.cam) else { return };
        // The sub stream: same sound, far less video to throw away.
        f.audio = Some(AudioStream::rtsp(ff, &cam.rtsp_url(Quality::Sub), self.volume.clone(), Some(waker(ctx))));
    }

    /// Mute / unmute (M). Remembered across restarts.
    pub fn toggle_sound(&mut self, ctx: &egui::Context) {
        self.cfg.sound = !self.cfg.sound;
        if self.cfg.sound {
            self.start_audio(ctx);
        } else if let Some(f) = self.focus.as_mut() {
            f.audio = None;
        }
        self.save_config();
    }

    pub fn set_volume(&mut self, percent: u32) {
        self.cfg.sound_volume = percent.min(tapestry_core::audio::MAX_VOLUME);
        self.volume.store(self.cfg.sound_volume, Ordering::Relaxed);
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
            let focused = self.focus.as_ref().filter(|f| f.grid_released).map(|f| f.cam);
            for (i, t) in self.tiles.iter().enumerate() {
                if let Some(s) = &t.stream {
                    s.set_paused(minimized || Some(i) == focused);
                }
            }
            if let Some(f) = &self.focus {
                if let Some(s) = &f.view.stream {
                    s.set_paused(minimized);
                }
                if let Some(a) = &f.audio {
                    a.set_paused(minimized);
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

    fn upload(&mut self, ctx: &egui::Context, gl: Option<&Arc<glow::Context>>) {
        let renderer = self.video.clone();
        let gpu = gl.map(|g| &**g).zip(renderer.as_deref());
        if let Some((gl, r)) = gpu {
            r.free_unused(gl);
        }
        for t in self.tiles.iter_mut() {
            t.upload(gpu);
        }
        let mut released_now = false;
        if let Some(f) = &mut self.focus {
            f.view.upload(gpu);
            if !f.grid_released {
                // Release the stand-in grid stream once the full-quality picture
                // is up, or as soon as the camera refuses/drops the extra viewer.
                let status = f.view.stream.as_ref().map(|s| s.status());
                let ready = f.view.seq > 0
                    || matches!(status, None | Some(StreamStatus::Retrying { .. }) | Some(StreamStatus::Failed(_)));
                if ready {
                    f.grid_released = true;
                    released_now = true;
                    if let Some(s) = self.tiles.get(f.cam).and_then(|t| t.stream.as_ref()) {
                        s.set_paused(true);
                    }
                }
            }
        }
        if released_now {
            self.start_audio(ctx);
        }
        if let Some(p) = &mut self.playback {
            p.view.upload(gpu);
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
        let (esc, digits, c_key, s_key, r_key, p_key, f_key, space, left, right, m_key) = ctx.input(|i| {
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
                i.key_pressed(egui::Key::M) && !i.modifiers.any(),
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
                if m_key {
                    self.toggle_sound(ctx);
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

    fn top_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("top")
            .frame(egui::Frame::none().fill(theme::PANEL).inner_margin(egui::Margin::symmetric(12.0, 8.0)))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("TAPESTRY").strong().size(15.0).color(theme::AMBER));
                    ui.add_space(10.0);
                    let cams = self.cams.clone();
                    for (i, c) in cams.iter().enumerate() {
                        let (dot, tip) = status_dot(c);
                        let mut text = RichText::new(format!("{}  {}", i + 1, c.name)).size(13.0);
                        if c.motion {
                            text = text.color(theme::MOTION).strong();
                        }
                        let chip = ui.add(egui::Button::new(text).fill(theme::RAISED).rounding(12.0));
                        // Status dot drawn over the chip's left padding.
                        let p = chip.rect.left_center() + egui::vec2(-2.0, 0.0);
                        ui.painter().circle_filled(p, 4.0, dot);
                        let chip = chip.on_hover_text(tip);
                        if chip.clicked() {
                            self.open_focus(ctx, i);
                        }
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("⚙ Settings").clicked() {
                            self.settings = Some(SettingsDraft::new(&self.cfg, false));
                        }
                        let pin = if self.cfg.always_on_top { "📌 Pinned" } else { "📌 Pin" };
                        if ui
                            .selectable_label(self.cfg.always_on_top, pin)
                            .on_hover_text("Keep Tapestry above other windows (P)")
                            .clicked()
                        {
                            self.toggle_pin(ctx);
                        }
                        if ui
                            .selectable_label(self.show_clips, "🎞 Clips")
                            .on_hover_text("Recorded clips and motion timeline (C)")
                            .clicked()
                        {
                            self.show_clips = !self.show_clips;
                        }
                        let unseen = self.engine.as_ref().map(|e| e.log_version()).unwrap_or(0).saturating_sub(self.log_seen);
                        let label = if unseen > 0 && !self.show_activity {
                            format!("🔔 Activity ({unseen})")
                        } else {
                            "🔔 Activity".into()
                        };
                        if ui.selectable_label(self.show_activity, label).clicked() {
                            self.show_activity = !self.show_activity;
                        }
                        let rec: Vec<&str> =
                            self.cams.iter().filter(|c| c.recording.is_some()).map(|c| c.name.as_str()).collect();
                        if !rec.is_empty() {
                            let pulse = pulse(ui.ctx(), 1.2);
                            ui.label(
                                RichText::new(format!("● REC {}", rec.len()))
                                    .color(theme::MOTION.gamma_multiply(0.55 + 0.45 * pulse))
                                    .strong(),
                            )
                            .on_hover_text(format!("Recording: {}", rec.join(", ")));
                        }
                    });
                });
            });
    }

    fn banners(&mut self, ctx: &egui::Context) {
        if self.ffmpeg.is_none() {
            egui::TopBottomPanel::top("ffmpeg-banner")
                .frame(
                    egui::Frame::none()
                        .fill(Color32::from_rgb(0x3A, 0x2A, 0x10))
                        .inner_margin(egui::Margin::symmetric(12.0, 8.0)),
                )
                .show(ctx, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(RichText::new("FFmpeg not found.").strong().color(theme::WARN));
                        ui.label("Tapestry uses it for video. Install it in PowerShell with");
                        ui.code("winget install Gyan.FFmpeg");
                        if ui.small_button("Copy").clicked() {
                            ui.output_mut(|o| o.copied_text = "winget install Gyan.FFmpeg".into());
                        }
                        ui.label("then restart Tapestry — or set its location in Settings.");
                        if ui.small_button("Check again").clicked() {
                            self.restart(ui.ctx());
                        }
                    });
                });
        }
    }

    fn bottom_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::bottom("bottom")
            .frame(egui::Frame::none().fill(theme::PANEL).inner_margin(egui::Margin::symmetric(12.0, 5.0)))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    let last = self.engine.as_ref().and_then(|e| e.log().last().cloned());
                    if let Some(l) = last {
                        let color = match l.level {
                            tapestry_core::LogLevel::Motion => theme::MOTION,
                            tapestry_core::LogLevel::Warn => theme::WARN,
                            tapestry_core::LogLevel::Info => theme::MUTED,
                        };
                        ui.label(RichText::new(util::hhmm(l.at)).color(theme::MUTED).small());
                        ui.label(RichText::new(l.text).color(color).small());
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(
                            RichText::new(chrono::Local::now().format("%a %d %b  %H:%M").to_string()).color(theme::MUTED).small(),
                        );
                        ui.separator();
                        let mode = match self.cfg.record_mode {
                            tapestry_core::RecordMode::Off => "Recording off",
                            tapestry_core::RecordMode::Motion => "Records on motion",
                            tapestry_core::RecordMode::Continuous => "Recording continuously",
                        };
                        ui.label(RichText::new(mode).color(theme::MUTED).small());
                        ui.separator();
                        ui.label(
                            RichText::new("1-9 camera · Esc grid · M sound · C clips · P pin · F fullscreen").color(theme::MUTED).small(),
                        );
                    });
                });
            });
    }

    fn activity_window(&mut self, ctx: &egui::Context) {
        if !self.show_activity {
            return;
        }
        let Some(engine) = &self.engine else { return };
        self.log_seen = engine.log_version();
        let log = engine.log();
        let mut open = true;
        egui::Window::new("Activity").open(&mut open).default_width(420.0).default_height(360.0).show(ctx, |ui| {
            egui::ScrollArea::vertical().auto_shrink([false, false]).stick_to_bottom(true).show(ui, |ui| {
                if log.is_empty() {
                    ui.weak("Nothing yet.");
                }
                for l in &log {
                    let color = match l.level {
                        tapestry_core::LogLevel::Motion => theme::MOTION,
                        tapestry_core::LogLevel::Warn => theme::WARN,
                        tapestry_core::LogLevel::Info => theme::TEXT,
                    };
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(util::hhmmss(l.at)).monospace().color(theme::MUTED));
                        ui.label(RichText::new(&l.text).color(color));
                    });
                }
            });
        });
        self.show_activity = open;
    }

    fn toasts(&mut self, ctx: &egui::Context) {
        if self.toasts.is_empty() {
            return;
        }
        egui::Area::new(egui::Id::new("toasts"))
            .anchor(egui::Align2::RIGHT_BOTTOM, egui::vec2(-16.0, -40.0))
            .interactable(false)
            .show(ctx, |ui| {
                for (_, text, color) in &self.toasts {
                    egui::Frame::none()
                        .fill(theme::RAISED)
                        .stroke(egui::Stroke::new(1.0_f32, *color))
                        .rounding(8.0)
                        .inner_margin(egui::Margin::symmetric(12.0, 8.0))
                        .show(ui, |ui| {
                            ui.set_max_width(460.0);
                            ui.label(RichText::new(text).color(theme::TEXT));
                        });
                    ui.add_space(6.0);
                }
            });
    }
}

impl eframe::App for TapestryApp {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        if ctx.input(|i| i.viewport().close_requested()) && !self.closing {
            // Finish any recording cleanly before the window goes.
            self.closing = true;
            self.stop_ptz();
            // Stop every FFmpeg we started and wait for them to exit.
            let mut streams: Vec<&mut VideoStream> = self.tiles.iter_mut().filter_map(|t| t.stream.as_mut()).collect();
            if let Some(s) = self.focus.as_mut().and_then(|f| f.view.stream.as_mut()) {
                streams.push(s);
            }
            if let Some(s) = self.playback.as_mut().and_then(|p| p.view.stream.as_mut()) {
                streams.push(s);
            }
            for s in &streams {
                s.request_stop();
            }
            for s in streams {
                s.stop();
            }
            if let Some(a) = self.focus.as_mut().and_then(|f| f.audio.as_mut()) {
                a.stop();
            }
            self.tiles.clear();
            self.focus = None;
            self.playback = None;
            if let Some(mut e) = self.engine.take() {
                e.shutdown();
            }
            return;
        }

        self.poll(ctx);
        self.upload(ctx, frame.gl());
        self.keys(ctx);

        self.top_bar(ctx);
        self.banners(ctx);
        self.bottom_bar(ctx);
        if self.show_clips {
            self.clips_panel(ctx);
        }
        egui::CentralPanel::default().frame(egui::Frame::none().fill(theme::BG).inner_margin(egui::Margin::same(8.0))).show(
            ctx,
            |ui| match self.center {
                Center::Grid => self.grid_view(ui),
                Center::Focus => self.focus_view(ui),
                Center::Playback => self.playback_view(ui),
            },
        );
        self.settings_window(ctx);
        self.activity_window(ctx);
        self.toasts(ctx);

        // Keep clocks, "x min ago" and pulses moving even without new frames.
        let animating = self.cams.iter().any(|c| c.motion || c.recording.is_some()) || !self.toasts.is_empty();
        ctx.request_repaint_after(if animating { Duration::from_millis(50) } else { Duration::from_secs(1) });
    }
}

/// Repaint the UI when a background thread has something new.
pub fn waker(ctx: &egui::Context) -> tapestry_core::stream::Waker {
    let ctx = ctx.clone();
    Arc::new(move || ctx.request_repaint())
}

/// 0..1 sine pulse with the given period.
pub fn pulse(ctx: &egui::Context, period: f32) -> f32 {
    let t = ctx.input(|i| i.time) as f32;
    0.5 + 0.5 * (t * std::f32::consts::TAU / period).sin()
}

pub fn status_dot(c: &CameraState) -> (Color32, String) {
    match &c.link {
        LinkState::Online => {
            if c.motion {
                (theme::MOTION, "Motion right now".into())
            } else {
                let last =
                    c.last_motion.map(|t| format!("Last motion {}", util::ago(t))).unwrap_or_else(|| "No motion yet".into());
                (theme::LIVE, format!("Online · {last}"))
            }
        }
        LinkState::NoEvents(e) => (theme::WARN, format!("Online, but no motion alerts: {e}")),
        LinkState::Connecting => (theme::MUTED, "Connecting…".into()),
        LinkState::Offline(e) => (theme::MOTION.gamma_multiply(0.6), format!("Offline: {e}")),
        LinkState::AuthFailed(e) => (theme::WARN, format!("Login rejected: {e}")),
    }
}

#[allow(dead_code)]
pub fn since(t: SystemTime) -> Duration {
    SystemTime::now().duration_since(t).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::focus_size;

    #[test]
    fn focus_uses_native_size_when_known() {
        assert_eq!(focus_size(Some((2880, 1620))), (2880, 1620));
        assert_eq!(focus_size(Some((2561, 1441))), (2560, 1440), "rounded to even");
        assert_eq!(focus_size(Some((7680, 4320))), (3840, 2160), "capped at 4K");
        assert_eq!(focus_size(None), (1920, 1080));
        assert_eq!(focus_size(Some((0, 0))), (1920, 1080));
    }
}
