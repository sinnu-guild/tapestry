//! The three centre views: the camera grid, a focused camera (with pan/tilt,
//! click-to-aim and digital zoom), and clip playback.

use crate::app::{pulse, status_dot, Center, TapestryApp};
use crate::{theme, util};
use eframe::egui::{self, pos2, vec2, Align2, Color32, FontId, Id, Rect, RichText, Sense, Stroke};
use std::time::{Duration, SystemTime};
use tapestry_core::{LinkState, PtzCommand, StreamStatus};

const ASPECT: f32 = 16.0 / 9.0;
const FULL_UV: Rect = Rect { min: pos2(0.0, 0.0), max: pos2(1.0, 1.0) };

fn pill(painter: &egui::Painter, at: egui::Pos2, anchor: Align2, text: &str, fg: Color32, bg: Color32, size: f32) -> Rect {
    let galley = painter.layout_no_wrap(text.to_string(), FontId::proportional(size), fg);
    let pad = vec2(7.0, 3.0);
    let rect = anchor.anchor_size(at, galley.size() + pad * 2.0);
    painter.rect_filled(rect, 5.0, bg);
    painter.galley(rect.min + pad, galley, fg);
    rect
}

impl TapestryApp {
    fn stream_problem(&self, i: usize, focus: bool) -> Option<(String, bool)> {
        let status = if focus {
            self.focus.as_ref().and_then(|f| f.view.stream.as_ref()).map(|s| s.status())
        } else {
            self.tiles.get(i).and_then(|t| t.stream.as_ref()).map(|s| s.status())
        };
        match status {
            None => Some(("FFmpeg needed for video".into(), false)),
            Some(StreamStatus::Live) => None,
            Some(StreamStatus::Connecting) => Some(("Connecting…".into(), true)),
            Some(StreamStatus::Retrying { error, attempt }) => Some((format!("{error}\nretrying (attempt {attempt})"), true)),
            Some(StreamStatus::Paused) => Some(("Paused".into(), false)),
            Some(StreamStatus::Ended) | Some(StreamStatus::Stopped) => Some(("Stream ended".into(), false)),
            Some(StreamStatus::Failed(e)) => Some((e, false)),
        }
    }

    /// Paint one camera tile. Returns the tile's click response.
    fn tile(&mut self, ui: &mut egui::Ui, i: usize, rect: Rect, compact: bool) -> egui::Response {
        let resp = ui.interact(rect, Id::new(("tile", i)), Sense::click());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 8.0, theme::TILE);
        let video = util::fit_aspect(rect, ASPECT);
        let has_tex = if let Some(tex) = self.tiles.get(i).and_then(|t| t.tex.as_ref()) {
            painter.image(tex.id(), video, FULL_UV, Color32::WHITE);
            true
        } else {
            false
        };

        let focused_elsewhere = self.focus.as_ref().map_or(false, |f| f.cam == i);
        if let Some((msg, busy)) = self.stream_problem(i, false).filter(|_| !focused_elsewhere) {
            if has_tex {
                painter.rect_filled(video, 0.0, Color32::from_black_alpha(150));
            }
            let size = if compact { 11.0 } else { 14.0 };
            let msg = if busy {
                format!("{} {msg}", ["◐", "◓", "◑", "◒"][(ui.input(|i| i.time) * 6.0) as usize % 4])
            } else {
                msg
            };
            painter.text(rect.center(), Align2::CENTER_CENTER, msg, FontId::proportional(size), theme::MUTED);
            if busy {
                ui.ctx().request_repaint_after(Duration::from_millis(150));
            }
        }

        let cam = self.cams.get(i).cloned();
        let name = self.cfg.cameras.get(i).map(|c| c.name.clone()).unwrap_or_default();
        let fs = if compact { 11.0 } else { 13.0 };
        let m = if compact { 6.0 } else { 10.0 };
        pill(
            &painter,
            rect.left_top() + vec2(m, m),
            Align2::LEFT_TOP,
            &format!("{}  {name}", i + 1),
            theme::TEXT,
            Color32::from_black_alpha(170),
            fs,
        );

        if let Some(c) = &cam {
            let mut x = rect.right() - m;
            if c.recording.is_some() {
                let a = 0.6 + 0.4 * pulse(ui.ctx(), 1.2);
                let r = pill(
                    &painter,
                    pos2(x, rect.top() + m),
                    Align2::RIGHT_TOP,
                    "● REC",
                    Color32::WHITE,
                    theme::MOTION.gamma_multiply(a),
                    fs,
                );
                x = r.left() - 6.0;
            }
            if c.motion {
                let r = pill(&painter, pos2(x, rect.top() + m), Align2::RIGHT_TOP, "MOTION", Color32::BLACK, theme::WARN, fs);
                x = r.left() - 6.0;
            }
            match &c.link {
                LinkState::AuthFailed(_) => {
                    pill(&painter, pos2(x, rect.top() + m), Align2::RIGHT_TOP, "⚠ login", Color32::BLACK, theme::WARN, fs);
                }
                LinkState::Offline(_) if has_tex => {
                    pill(&painter, pos2(x, rect.top() + m), Align2::RIGHT_TOP, "⚠ no alerts", Color32::BLACK, theme::WARN, fs);
                }
                _ => {}
            }
            if !compact {
                let line = match (c.motion, c.last_motion) {
                    (true, _) => None,
                    (false, Some(t)) => Some(format!("motion {}", util::ago(t))),
                    _ => None,
                };
                if let Some(line) = line {
                    pill(
                        &painter,
                        rect.left_bottom() + vec2(m, -m),
                        Align2::LEFT_BOTTOM,
                        &line,
                        theme::MUTED,
                        Color32::from_black_alpha(150),
                        11.0,
                    );
                }
            }
            if c.motion {
                let a = 0.45 + 0.55 * pulse(ui.ctx(), 0.9);
                painter.rect_stroke(rect.shrink(1.5), 8.0, Stroke::new(3.0_f32, theme::MOTION.gamma_multiply(a)));
            }
        }
        if resp.hovered() {
            painter.rect_stroke(rect.shrink(0.5), 8.0, Stroke::new(1.0_f32, theme::AMBER.gamma_multiply(0.7)));
        }
        resp
    }

    fn tile_menu(&mut self, resp: &egui::Response, i: usize) {
        resp.context_menu(|ui| {
            if ui.button("🔍 Open").clicked() {
                self.open_focus(ui.ctx(), i);
                ui.close_menu();
            }
            if ui.button("📷 Snapshot").clicked() {
                self.snapshot(i);
                ui.close_menu();
            }
            let manual = self.cams.get(i).and_then(|c| c.manual_until).is_some();
            if ui.button(if manual { "⏹ Stop manual recording" } else { "⏺ Record 5 minutes" }).clicked() {
                self.record_now(i);
                ui.close_menu();
            }
            if ui.button("🎞 Clips from this camera").clicked() {
                self.clips_cam = self.cfg.cameras.get(i).map(|c| c.slug());
                self.show_clips = true;
                ui.close_menu();
            }
        });
    }

    // ----- grid -----------------------------------------------------------

    pub fn grid_view(&mut self, ui: &mut egui::Ui) {
        let n = self.cfg.cameras.len();
        if n == 0 {
            self.welcome(ui);
            return;
        }
        let (cols, rows) = match n {
            1 => (1, 1),
            2 => (2, 1),
            3 | 4 => (2, 2),
            5 | 6 => (3, 2),
            _ => {
                let c = (n as f32).sqrt().ceil() as usize;
                (c, (n + c - 1) / c)
            }
        };
        let area = ui.available_rect_before_wrap();
        ui.allocate_rect(area, Sense::hover());
        let gap = 8.0;
        let cw = (area.width() - gap * (cols as f32 - 1.0)) / cols as f32;
        let ch = (area.height() - gap * (rows as f32 - 1.0)) / rows as f32;
        for k in 0..cols * rows {
            let (r, c) = (k / cols, k % cols);
            let rect = Rect::from_min_size(area.min + vec2(c as f32 * (cw + gap), r as f32 * (ch + gap)), vec2(cw, ch));
            if k < n {
                let resp = self.tile(ui, k, rect, false).on_hover_text("Click to open · right-click for more");
                if resp.clicked() {
                    self.open_focus(ui.ctx(), k);
                }
                self.tile_menu(&resp, k);
            } else if k == n {
                self.today_card(ui, rect);
            }
        }
    }

    /// Fills the spare grid cell: today at a glance.
    fn today_card(&mut self, ui: &mut egui::Ui, rect: Rect) {
        self.refresh_clips(false); // throttled; keeps the disk-usage line current
        ui.allocate_new_ui(egui::UiBuilder::new().max_rect(rect), |ui| {
            egui::Frame::none().fill(theme::PANEL).rounding(8.0).inner_margin(egui::Margin::same(16.0)).show(ui, |ui| {
                ui.set_min_size(rect.size() - vec2(32.0, 32.0));
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Today").size(20.0).strong());
                    ui.label(RichText::new(chrono::Local::now().format("%A %d %B").to_string()).color(theme::MUTED));
                });
                ui.add_space(8.0);
                let cams = self.cams.clone();
                egui::Grid::new("today-grid").num_columns(3).spacing([18.0, 8.0]).show(ui, |ui| {
                    for (i, c) in cams.iter().enumerate() {
                        let (dot, tip) = status_dot(c);
                        ui.horizontal(|ui| {
                            ui.label(RichText::new("●").color(dot)).on_hover_text(tip);
                            if ui.link(RichText::new(&c.name).strong()).clicked() {
                                self.open_focus(ui.ctx(), i);
                            }
                        });
                        let count = c.motion_count_today;
                        ui.label(
                            RichText::new(match count {
                                0 => "no motion".to_string(),
                                1 => "1 motion event".to_string(),
                                n => format!("{n} motion events"),
                            })
                            .color(if count > 0 { theme::TEXT } else { theme::MUTED }),
                        );
                        ui.label(
                            RichText::new(if c.motion {
                                "moving now".into()
                            } else {
                                c.last_motion.map(util::ago).unwrap_or_default()
                            })
                            .color(if c.motion { theme::MOTION } else { theme::MUTED }),
                        );
                        ui.end_row();
                    }
                });
                ui.add_space(12.0);
                let recent: Vec<_> = self
                    .engine
                    .as_ref()
                    .map(|e| e.log())
                    .unwrap_or_default()
                    .into_iter()
                    .rev()
                    .filter(|l| l.level == tapestry_core::LogLevel::Motion)
                    .take(6)
                    .collect();
                if !recent.is_empty() {
                    ui.label(RichText::new("Recent").color(theme::MUTED).small());
                    for l in recent {
                        ui.label(RichText::new(format!("{}  {}", util::hhmm(l.at), l.text)).small());
                    }
                }
                ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
                    if self.clips.scanned.is_some() {
                        ui.label(
                            RichText::new(format!("Recordings: {} on disk", util::bytes(self.clips.usage)))
                                .color(theme::MUTED)
                                .small(),
                        );
                    }
                });
            });
        });
    }

    fn welcome(&mut self, ui: &mut egui::Ui) {
        ui.vertical_centered(|ui| {
            ui.add_space(ui.available_height() * 0.18);
            ui.label(RichText::new("Tapestry").size(40.0).color(theme::AMBER).strong());
            ui.label(RichText::new("all your Tapo cameras, one alt-tab away").color(theme::MUTED));
            ui.add_space(24.0);
            ui.label("Before adding cameras, create a Camera Account on each one in the Tapo phone app:");
            ui.label(RichText::new("Camera → ⚙ Settings → Advanced Settings → Camera Account").strong());
            ui.label(RichText::new("Pick a username + password (it's separate from your TP-Link login). You'll also need each camera's IP address,\nwhich is under Settings → Device Info.").color(theme::MUTED));
            ui.add_space(18.0);
            if ui.add(egui::Button::new(RichText::new("  Add your cameras  ").size(16.0)).fill(theme::AMBER.gamma_multiply(0.35))).clicked() {
                self.settings = Some(crate::settings::SettingsDraft::new(&self.cfg, true));
            }
        });
    }

    // ----- shared filmstrip ---------------------------------------------------
}
