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
