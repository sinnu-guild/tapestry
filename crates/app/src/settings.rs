//! Settings window: cameras (with a connection test), recording, general.

use crate::app::{TapestryApp, TestState};
use crate::{theme, util};
use eframe::egui::{self, RichText};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use tapestry_core::onvif::OnvifClient;
use tapestry_core::{ffmpeg, Camera, Config, Quality, RecordMode};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Cameras,
    Recording,
    General,
}

pub struct SettingsDraft {
    pub cfg: Config,
    pub tab: Tab,
    pub tests: Vec<Option<Arc<Mutex<TestState>>>>,
    pub show_passwords: bool,
    pub ffmpeg_check: (String, Option<PathBuf>),
    pub error: Option<String>,
}

impl SettingsDraft {
    pub fn new(cfg: &Config, first_run: bool) -> Self {
        let mut cfg = cfg.clone();
        if first_run && cfg.cameras.is_empty() {
            cfg.cameras.push(Camera { name: "Camera 1".into(), ..Camera::default() });
        }
        let tests = cfg.cameras.iter().map(|_| None).collect();
        let check = (cfg.ffmpeg.clone(), ffmpeg::locate(&cfg.ffmpeg));
        SettingsDraft { cfg, tab: Tab::Cameras, tests, show_passwords: false, ffmpeg_check: check, error: None }
    }
}

enum Action {
    None,
    Save,
    Cancel,
}

/// Talk to the camera the same way the app will: ONVIF login, then one frame of video.
fn run_test(cam: Camera, ff: Option<PathBuf>, out: Arc<Mutex<TestState>>) {
    std::thread::spawn(move || {
        let mut c = OnvifClient::new(&cam.host, cam.onvif_port, &cam.username, &cam.password);
        let onvif = c.connect();
        let video = match &ff {
            None => Err("FFmpeg not installed, couldn't test video".to_string()),
            Some(ff) => {
                let o = ffmpeg::command(ff)
                    .args(ffmpeg::rtsp_input_args(&cam.rtsp_url(Quality::Sub), false))
                    .args(["-frames:v", "1", "-f", "null", "-"])
                    .stderr(Stdio::piped())
                    .output();
                match o {
                    Ok(o) if o.status.success() => Ok(()),
                    Ok(o) => {
                        Err(ffmpeg::friendly_error(&ffmpeg::last_line(&String::from_utf8_lossy(&o.stderr)).unwrap_or_default()))
                    }
                    Err(e) => Err(e.to_string()),
                }
            }
        };
        let state = match (onvif, video) {
            (Ok(info), Ok(())) => {
                let ptz = if c.has_ptz() { "pan/tilt ✓" } else { "no pan/tilt" };
                TestState::Ok(format!("✓ {} (firmware {}) · video ✓ · {ptz}", info.model, info.firmware))
            }
            (Ok(info), Err(v)) => TestState::Err(format!("Logged in to {} but video failed: {v}", info.model)),
            (Err(e), Ok(())) => TestState::Err(format!("Video works, but pan/tilt & motion alerts failed: {e}")),
            (Err(e), Err(_)) => TestState::Err(e.to_string()),
        };
        *out.lock().unwrap() = state;
    });
}

impl TapestryApp {
    pub fn settings_window(&mut self, ctx: &egui::Context) {
        let Some(mut d) = self.settings.take() else { return };
        let mut open = true;
        let mut action = Action::None;

        egui::Window::new("Settings").open(&mut open).collapsible(false).resizable(true).default_size([600.0, 560.0]).show(
            ctx,
            |ui| {
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut d.tab, Tab::Cameras, "📷 Cameras");
                    ui.selectable_value(&mut d.tab, Tab::Recording, "⏺ Recording");
                    ui.selectable_value(&mut d.tab, Tab::General, "⚙ General");
                });
                ui.separator();
                egui::ScrollArea::vertical().auto_shrink([false, false]).max_height(ui.available_height() - 44.0).show(
                    ui,
                    |ui| match d.tab {
                        Tab::Cameras => cameras_tab(ui, &mut d, self.ffmpeg.clone()),
                        Tab::Recording => recording_tab(ui, &mut d),
                        Tab::General => general_tab(ui, &mut d),
                    },
                );
                ui.separator();
                ui.horizontal(|ui| {
                    if ui
                        .add(egui::Button::new(RichText::new("Save & apply").strong()).fill(theme::AMBER.gamma_multiply(0.35)))
                        .clicked()
                    {
                        action = Action::Save;
                    }
                    if ui.button("Cancel").clicked() {
                        action = Action::Cancel;
                    }
                    if let Some(e) = &d.error {
                        ui.label(RichText::new(e).color(theme::MOTION));
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(
                            RichText::new(tapestry_core::config::config_path().display().to_string()).color(theme::MUTED).small(),
                        );
                    });
                });
            },
        );

        match action {
            Action::Save => {
                // Drop untouched blank rows; insist on an address for the rest.
                d.cfg.cameras.retain(|c| !(c.host.trim().is_empty() && c.username.is_empty() && c.password.is_empty()));
                if let Some(c) = d.cfg.cameras.iter().find(|c| c.host.trim().is_empty()) {
                    d.error = Some(format!("“{}” needs an IP address", c.name));
                    self.settings = Some(d);
                    return;
                }
                let mut slugs: Vec<String> = d.cfg.cameras.iter().map(|c| c.slug()).collect();
                slugs.sort();
                slugs.dedup();
                if slugs.len() != d.cfg.cameras.len() {
                    d.error = Some("Give each camera a different name".into());
                    self.settings = Some(d);
                    return;
                }
                for (i, c) in d.cfg.cameras.iter_mut().enumerate() {
                    c.host = c.host.trim().to_string();
                    if c.name.trim().is_empty() {
                        c.name = format!("Camera {}", i + 1);
                    }
                }
                d.cfg.sound = self.cfg.sound;
                d.cfg.sound_volume = self.cfg.sound_volume;
                self.cfg = d.cfg;
                self.save_config();
                self.restart(ctx);
                self.toast("Settings saved", theme::LIVE);
            }
            Action::Cancel => {}
            Action::None => {
                if open {
                    self.settings = Some(d);
                }
            }
        }
    }
}

fn cameras_tab(ui: &mut egui::Ui, d: &mut SettingsDraft, ff: Option<PathBuf>) {
    ui.label(
        RichText::new("Use each camera's Camera Account (Tapo app → camera → ⚙ → Advanced Settings → Camera Account). The IP address is under Device Info. Tip: give the cameras fixed IPs in your router so they don't move.")
            .color(theme::MUTED)
            .small(),
    );
    ui.checkbox(&mut d.show_passwords, "Show passwords");
    ui.add_space(6.0);

    let mut remove = None;
    let n = d.cfg.cameras.len();
    for i in 0..n {
        egui::Frame::none().fill(theme::RAISED).rounding(8.0).inner_margin(egui::Margin::same(10.0)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            let cam = &mut d.cfg.cameras[i];
            egui::Grid::new(("cam-grid", i)).num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                ui.label("Name");
                ui.add(egui::TextEdit::singleline(&mut cam.name).hint_text("Front gate").desired_width(240.0));
                ui.end_row();
                ui.label("IP address");
                ui.add(egui::TextEdit::singleline(&mut cam.host).hint_text("192.168.1.50").desired_width(240.0));
                ui.end_row();
                ui.label("Username");
                ui.add(egui::TextEdit::singleline(&mut cam.username).hint_text("camera account username").desired_width(240.0));
                ui.end_row();
                ui.label("Password");
                ui.add(egui::TextEdit::singleline(&mut cam.password).password(!d.show_passwords).desired_width(240.0));
                ui.end_row();
            });
            ui.collapsing("Advanced", |ui| {
                egui::Grid::new(("cam-adv", i)).num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                    ui.label("ONVIF port");
                    ui.add(egui::DragValue::new(&mut cam.onvif_port));
                    ui.end_row();
                    ui.label("RTSP port");
                    ui.add(egui::DragValue::new(&mut cam.rtsp_port));
                    ui.end_row();
                    ui.label("HD stream path");
                    ui.add(egui::TextEdit::singleline(&mut cam.main_path).desired_width(120.0));
                    ui.end_row();
                    ui.label("SD stream path");
                    ui.add(egui::TextEdit::singleline(&mut cam.sub_path).desired_width(120.0));
                    ui.end_row();
                });
                ui.checkbox(&mut cam.invert_ptz, "Camera is mounted upside down (flip pan/tilt)");
                ui.checkbox(&mut cam.record, "Record this camera");
            });
            ui.horizontal(|ui| {
                let running = matches!(d.tests[i].as_ref().map(|t| matches!(*t.lock().unwrap(), TestState::Running)), Some(true));
                if ui.add_enabled(!running && !cam.host.trim().is_empty(), egui::Button::new("Test connection")).clicked() {
                    let state = Arc::new(Mutex::new(TestState::Running));
                    let mut c = cam.clone();
                    c.host = c.host.trim().to_string();
                    run_test(c, ff.clone(), state.clone());
                    d.tests[i] = Some(state);
                }
                if let Some(t) = &d.tests[i] {
                    match &*t.lock().unwrap() {
                        TestState::Running => {
                            ui.spinner();
                            ui.label(RichText::new("Talking to the camera…").color(theme::MUTED));
                        }
                        TestState::Ok(m) => {
                            ui.label(RichText::new(m).color(theme::LIVE));
                        }
                        TestState::Err(m) => {
                            ui.label(RichText::new(m).color(theme::MOTION));
                        }
                    }
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button("Remove").clicked() {
                        remove = Some(i);
                    }
                });
            });
        });
        ui.add_space(6.0);
    }
    if let Some(i) = remove {
        d.cfg.cameras.remove(i);
        d.tests.remove(i);
    }
    if ui.button("＋ Add camera").clicked() {
        let n = d.cfg.cameras.len() + 1;
        // Copy the login from the previous camera; people often reuse it.
        let (u, p) = d.cfg.cameras.last().map(|c| (c.username.clone(), c.password.clone())).unwrap_or_default();
        d.cfg.cameras.push(Camera { name: format!("Camera {n}"), username: u, password: p, ..Camera::default() });
        d.tests.push(None);
    }
    ui.ctx().request_repaint_after(std::time::Duration::from_millis(250));
}

fn recording_tab(ui: &mut egui::Ui, d: &mut SettingsDraft) {
    let c = &mut d.cfg;
    ui.label(RichText::new("When to record").strong());
    for (mode, title, blurb) in [
        (
            RecordMode::Motion,
            "On motion",
            "Save a clip whenever a camera reports motion, plus a little after it stops. Light on disk.",
        ),
        (
            RecordMode::Continuous,
            "All the time",
            "Record everything in back-to-back files. Roughly 10–30 GB a day for three cameras.",
        ),
        (RecordMode::Off, "Off", "Live view only. You can still record manually with R."),
    ] {
        ui.radio_value(&mut c.record_mode, mode, RichText::new(title).strong());
        ui.label(RichText::new(blurb).color(theme::MUTED).small());
    }
    ui.add_space(10.0);
    egui::Grid::new("rec-grid").num_columns(2).spacing([12.0, 10.0]).show(ui, |ui| {
        ui.label("Keep recording after motion");
        ui.add(egui::Slider::new(&mut c.post_roll_secs, 5..=120).suffix(" s"));
        ui.end_row();
        ui.label("Split files every");
        let mut mins = (c.max_clip_secs / 60).max(1);
        ui.add(egui::Slider::new(&mut mins, 1..=30).suffix(" min"));
        c.max_clip_secs = mins * 60;
        ui.end_row();
        ui.label("Delete clips older than");
        ui.add(egui::Slider::new(&mut c.keep_days, 0..=90).suffix(" days"));
        ui.end_row();
        ui.label("");
        ui.label(
            RichText::new(if c.keep_days == 0 { "0 = keep forever" } else { "Snapshots are never deleted." })
                .color(theme::MUTED)
                .small(),
        );
        ui.end_row();
        ui.label("Max space for clips");
        ui.add(egui::Slider::new(&mut c.max_storage_gb, 0..=2000).logarithmic(true).suffix(" GB"));
        ui.end_row();
        ui.label("");
        ui.label(RichText::new("Oldest clips go first when full. 0 = no limit.").color(theme::MUTED).small());
        ui.end_row();
    });
    ui.add_space(10.0);
    ui.label(RichText::new("Recordings folder").strong());
    let mut dir = c.recordings_dir.to_string_lossy().into_owned();
    ui.horizontal(|ui| {
        if ui.add(egui::TextEdit::singleline(&mut dir).desired_width(380.0)).changed() {
            c.recordings_dir = PathBuf::from(dir.trim());
        }
        if ui.small_button("Open").clicked() {
            let _ = std::fs::create_dir_all(&c.recordings_dir);
            util::open_path(&c.recordings_dir);
        }
        if ui.small_button("Default").clicked() {
            c.recordings_dir = tapestry_core::config::default_recordings_dir();
        }
    });
    ui.label(
        RichText::new("Tapestry only ever deletes files it created (named like 14-05-33.mp4 inside date folders).")
            .color(theme::MUTED)
            .small(),
    );
}

fn general_tab(ui: &mut egui::Ui, d: &mut SettingsDraft) {
    ui.label(RichText::new("FFmpeg").strong());
    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut d.cfg.ffmpeg).hint_text("automatic (PATH or next to Tapestry)").desired_width(380.0),
        );
    });
    if d.ffmpeg_check.0 != d.cfg.ffmpeg {
        d.ffmpeg_check = (d.cfg.ffmpeg.clone(), ffmpeg::locate(&d.cfg.ffmpeg));
    }
    match &d.ffmpeg_check.1 {
        Some(p) => ui.label(RichText::new(format!("✓ using {}", p.display())).color(theme::LIVE).small()),
        None => ui.label(RichText::new("✗ not found — install with: winget install Gyan.FFmpeg").color(theme::MOTION).small()),
    };
    ui.add_space(10.0);

    let c = &mut d.cfg;
    ui.label(RichText::new("Wall").strong());
    egui::Grid::new("gen-grid").num_columns(2).spacing([12.0, 10.0]).show(ui, |ui| {
        ui.label("Grid frame rate");
        c.grid_fps = c.grid_fps.min(tapestry_core::config::GRID_FPS_NATIVE);
        ui.add(egui::Slider::new(&mut c.grid_fps, 2..=tapestry_core::config::GRID_FPS_NATIVE).custom_formatter(|v, _| {
            if v as u32 >= tapestry_core::config::GRID_FPS_NATIVE {
                "Native".into()
            } else {
                format!("{v:.0} fps")
            }
        }));
        ui.end_row();
        ui.label("");
        ui.label(
            RichText::new("Native = every frame each camera sends. Lower = less CPU. The camera you open always plays at full rate.")
                .color(theme::MUTED)
                .small(),
        );
        ui.end_row();
    });
    ui.checkbox(&mut c.flash_taskbar, "Flash the taskbar button when there's motion and Tapestry isn't in front");
    ui.checkbox(&mut c.always_on_top, "Keep Tapestry on top of other windows");
    ui.add_space(10.0);
    ui.label(RichText::new("Pan / tilt").strong());
    ui.checkbox(&mut c.click_to_aim, "Click on the video to aim the camera there");
    ui.add_enabled_ui(c.click_to_aim, |ui| {
        ui.horizontal(|ui| {
            ui.label("Aim strength");
            ui.add(egui::Slider::new(&mut c.aim_gain, 0.05..=1.0));
        });
        ui.label(RichText::new("If a click overshoots, lower this; if it falls short, raise it.").color(theme::MUTED).small());
    });
}
