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

    /// A column of small live tiles; click to focus that camera.
    fn filmstrip(&mut self, ui: &mut egui::Ui, rect: Rect, skip: Option<usize>) {
        let cams: Vec<usize> = (0..self.cfg.cameras.len()).filter(|i| Some(*i) != skip).collect();
        if cams.is_empty() {
            return;
        }
        let gap = 8.0;
        let h = ((rect.height() - gap * (cams.len() as f32 - 1.0)) / cams.len() as f32).min(rect.width() / ASPECT + 4.0);
        for (k, i) in cams.into_iter().enumerate() {
            let r = Rect::from_min_size(rect.min + vec2(0.0, k as f32 * (h + gap)), vec2(rect.width(), h));
            let resp = self.tile(ui, i, r, true);
            if resp.clicked() {
                self.open_focus(ui.ctx(), i);
            }
            self.tile_menu(&resp, i);
        }
    }

    // ----- focus ----------------------------------------------------------------

    pub fn focus_view(&mut self, ui: &mut egui::Ui) {
        let Some(cam) = self.focus.as_ref().map(|f| f.cam) else {
            self.center = Center::Grid;
            return;
        };
        let area = ui.available_rect_before_wrap();
        ui.allocate_rect(area, Sense::hover());
        let others = self.cfg.cameras.len() > 1;
        let strip_w = if others { (area.width() * 0.2).clamp(170.0, 300.0) } else { 0.0 };
        let bar = Rect::from_min_size(area.min, vec2(area.width() - strip_w - if others { 8.0 } else { 0.0 }, 34.0));
        let main = Rect::from_min_max(pos2(area.left(), bar.bottom() + 6.0), pos2(bar.right(), area.bottom()));

        self.focus_toolbar(ui, bar, cam);
        self.focus_video(ui, main, cam);
        if others {
            let strip = Rect::from_min_max(pos2(area.right() - strip_w, area.top()), area.max);
            self.filmstrip(ui, strip, Some(cam));
        }
    }

    fn focus_toolbar(&mut self, ui: &mut egui::Ui, bar: Rect, cam: usize) {
        let c = self.cams.get(cam).cloned();
        ui.allocate_new_ui(egui::UiBuilder::new().max_rect(bar), |ui| {
            ui.horizontal_centered(|ui| {
                if ui.button("◀ All cameras").on_hover_text("Esc").clicked() {
                    self.close_focus();
                    return;
                }
                let name = self.cfg.cameras.get(cam).map(|c| c.name.clone()).unwrap_or_default();
                ui.label(RichText::new(name).size(16.0).strong());
                if let Some(c) = &c {
                    let (dot, tip) = status_dot(c);
                    ui.label(RichText::new("●").color(dot)).on_hover_text(tip);
                    if let Some(info) = &c.info {
                        ui.label(RichText::new(format!("{} · fw {}", info.model, info.firmware)).color(theme::MUTED).small());
                    }
                    if let Some((t, e)) = &c.ptz_error {
                        if SystemTime::now().duration_since(*t).unwrap_or_default() < Duration::from_secs(8) {
                            ui.label(RichText::new(format!("pan/tilt: {e}")).color(theme::WARN).small());
                        }
                    }
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("🎞 Clips").clicked() {
                        self.clips_cam = self.cfg.cameras.get(cam).map(|c| c.slug());
                        self.show_clips = true;
                    }
                    let manual = c.as_ref().and_then(|c| c.manual_until);
                    let label = match manual {
                        Some(until) => {
                            let left = until.duration_since(SystemTime::now()).unwrap_or_default();
                            format!("⏹ Stop ({})", util::dur(left))
                        }
                        None => "⏺ Record 5 min".into(),
                    };
                    if ui.button(label).on_hover_text("R — record now regardless of motion").clicked() {
                        self.record_now(cam);
                    }
                    if ui.button("📷 Snapshot").on_hover_text("Ctrl+S — full-quality still").clicked() {
                        self.snapshot(cam);
                    }
                    if let Some(c) = &c {
                        if c.has_ptz && !c.presets.is_empty() {
                            ui.menu_button("📍 Presets", |ui| {
                                for p in &c.presets {
                                    if ui.button(&p.name).clicked() {
                                        if let Some(e) = &self.engine {
                                            e.ptz(cam, PtzCommand::GotoPreset(p.token.clone()));
                                        }
                                        ui.close_menu();
                                    }
                                }
                            });
                        }
                    }
                });
            });
        });
    }

    fn focus_video(&mut self, ui: &mut egui::Ui, area: Rect, cam: usize) {
        let has_ptz =
            self.cams.get(cam).map_or(false, |c| c.has_ptz && matches!(c.link, LinkState::Online | LinkState::NoEvents(_)));
        let click_to_aim = self.cfg.click_to_aim;
        let gain = self.cfg.aim_gain;
        let problem = self.stream_problem(cam, true);
        let motion = self.cams.get(cam).map_or(false, |c| c.motion);

        let video = util::fit_aspect(area, ASPECT);
        let resp = ui.interact(video, Id::new("focus-video"), Sense::click_and_drag());
        let painter = ui.painter_at(area);
        painter.rect_filled(area, 8.0, theme::TILE);

        let Some(f) = self.focus.as_mut() else { return };

        // Digital zoom: scroll to zoom around the cursor, drag to pan.
        if resp.hovered() {
            let scroll = ui.input(|i| i.smooth_scroll_delta.y);
            if scroll != 0.0 {
                if let Some(p) = resp.hover_pos() {
                    let old = f.zoom;
                    let new = (old * (1.0 + scroll * 0.0025)).clamp(1.0, 6.0);
                    let fx = (p.x - video.left()) / video.width();
                    let fy = (p.y - video.top()) / video.height();
                    let u = f.center.x - 0.5 / old + fx / old;
                    let v = f.center.y - 0.5 / old + fy / old;
                    f.center = pos2(u - fx / new + 0.5 / new, v - fy / new + 0.5 / new);
                    f.zoom = new;
                }
            }
        }
        if resp.dragged() && f.zoom > 1.0 {
            let d = resp.drag_delta();
            f.center.x -= d.x / video.width() / f.zoom;
            f.center.y -= d.y / video.height() / f.zoom;
        }
        let half = 0.5 / f.zoom;
        f.center.x = f.center.x.clamp(half, 1.0 - half);
        f.center.y = f.center.y.clamp(half, 1.0 - half);
        let uv = Rect::from_center_size(f.center, vec2(1.0 / f.zoom, 1.0 / f.zoom));

        if let Some(tex) = &f.view.tex {
            painter.image(tex.id(), video, uv, Color32::WHITE);
        }
        if let Some((msg, _)) = &problem {
            if f.view.tex.is_some() {
                painter.rect_filled(video, 0.0, Color32::from_black_alpha(150));
            }
            painter.text(video.center(), Align2::CENTER_CENTER, msg, FontId::proportional(16.0), theme::MUTED);
        }
        if motion {
            let a = 0.45 + 0.55 * pulse(ui.ctx(), 0.9);
            painter.rect_stroke(video.shrink(1.5), 6.0, Stroke::new(3.0_f32, theme::MOTION.gamma_multiply(a)));
            pill(&painter, video.right_top() + vec2(-12.0, 12.0), Align2::RIGHT_TOP, "MOTION", Color32::BLACK, theme::WARN, 14.0);
        }

        let zoomed = f.zoom > 1.01;
        if zoomed {
            let r = pill(
                &painter,
                video.left_top() + vec2(12.0, 12.0),
                Align2::LEFT_TOP,
                &format!("{:.1}× zoom — click to reset", f.zoom),
                theme::TEXT,
                Color32::from_black_alpha(170),
                12.0,
            );
            if ui.interact(r, Id::new("zoom-reset"), Sense::click()).clicked() {
                f.zoom = 1.0;
                f.center = pos2(0.5, 0.5);
            }
        }

        // Click-to-aim: nudge the camera toward where you clicked.
        let mut nudge = None;
        if resp.clicked() && !zoomed && has_ptz && click_to_aim {
            if let Some(p) = resp.interact_pointer_pos() {
                let dx = (p.x - video.center().x) / (video.width() / 2.0);
                let dy = (p.y - video.center().y) / (video.height() / 2.0);
                nudge = Some((dx * gain, -dy * gain));
                f.aim_flash = Some((p, std::time::Instant::now()));
            }
        }
        if let Some((p, t)) = f.aim_flash {
            let k = t.elapsed().as_secs_f32() / 0.6;
            if k < 1.0 {
                painter.circle_stroke(p, 6.0 + 28.0 * k, Stroke::new(2.0_f32, theme::AMBER.gamma_multiply(1.0 - k)));
                painter.line_segment(
                    [p - vec2(8.0, 0.0), p + vec2(8.0, 0.0)],
                    Stroke::new(1.5_f32, theme::AMBER.gamma_multiply(1.0 - k)),
                );
                painter.line_segment(
                    [p - vec2(0.0, 8.0), p + vec2(0.0, 8.0)],
                    Stroke::new(1.5_f32, theme::AMBER.gamma_multiply(1.0 - k)),
                );
                ui.ctx().request_repaint();
            } else {
                f.aim_flash = None;
            }
        }

        // D-pad (press and hold).
        let mut dpad = None;
        if has_ptz {
            let c = video.right_bottom() - vec2(78.0, 78.0);
            let hover = resp.hovered() || ui.rect_contains_pointer(Rect::from_center_size(c, vec2(140.0, 140.0)));
            painter.circle_filled(c, 60.0, Color32::from_black_alpha(if hover { 150 } else { 90 }));
            let fast = !ui.input(|i| i.modifiers.shift);
            let speed = if fast { 0.6 } else { 0.25 };
            for (k, (off, glyph, dir)) in [
                (vec2(0.0, -34.0), "▲", (0.0, 1.0)),
                (vec2(0.0, 34.0), "▼", (0.0, -1.0)),
                (vec2(-34.0, 0.0), "◀", (-1.0, 0.0)),
                (vec2(34.0, 0.0), "▶", (1.0, 0.0)),
            ]
            .into_iter()
            .enumerate()
            {
                let r = Rect::from_center_size(c + off, vec2(34.0, 34.0));
                let b = ui.interact(r, Id::new(("dpad", k)), Sense::click_and_drag());
                let down = b.is_pointer_button_down_on();
                if down {
                    dpad = Some((dir.0 * speed, dir.1 * speed));
                }
                let col = if down {
                    theme::AMBER
                } else if b.hovered() {
                    theme::TEXT
                } else {
                    theme::TEXT.gamma_multiply(if hover { 0.85 } else { 0.5 })
                };
                painter.circle_filled(r.center(), 16.0, if down { Color32::from_black_alpha(200) } else { Color32::TRANSPARENT });
                painter.text(r.center(), Align2::CENTER_CENTER, glyph, FontId::proportional(18.0), col);
            }
            painter.text(c, Align2::CENTER_CENTER, "⊕", FontId::proportional(14.0), theme::MUTED);
            if hover {
                painter.text(
                    c + vec2(0.0, 74.0),
                    Align2::CENTER_TOP,
                    if click_to_aim {
                        "hold to steer · arrows/WASD · click video to aim"
                    } else {
                        "hold to steer · arrows/WASD"
                    },
                    FontId::proportional(11.0),
                    theme::MUTED,
                );
            }
        }

        // Keyboard steering.
        let keys = if has_ptz && !ui.ctx().wants_keyboard_input() {
            ui.input(|i| {
                let k = |a: egui::Key, b: egui::Key| i.key_down(a) || i.key_down(b);
                let x = (k(egui::Key::ArrowRight, egui::Key::D) as i32 - k(egui::Key::ArrowLeft, egui::Key::A) as i32) as f32;
                let y = (k(egui::Key::ArrowUp, egui::Key::W) as i32 - k(egui::Key::ArrowDown, egui::Key::S) as i32) as f32;
                let speed = if i.modifiers.shift { 0.25 } else { 0.6 };
                // Ctrl+S is snapshot, not "tilt down".
                if i.modifiers.ctrl || (x == 0.0 && y == 0.0) {
                    None
                } else {
                    Some((x * speed, y * speed))
                }
            })
        } else {
            None
        };

        let window_focused = ui.input(|i| i.viewport().focused).unwrap_or(true);
        let want = if window_focused { dpad.or(keys) } else { None };
        self.ptz(cam, want);
        if want.is_some() {
            ui.ctx().request_repaint();
        }
        if let (Some((pan, tilt)), Some(e)) = (nudge, &self.engine) {
            e.ptz(cam, PtzCommand::Nudge { pan, tilt });
        }
    }

    // ----- playback -------------------------------------------------------------

    pub fn playback_view(&mut self, ui: &mut egui::Ui) {
        if self.playback.is_none() {
            self.center = Center::Grid;
            return;
        }
        let area = ui.available_rect_before_wrap();
        ui.allocate_rect(area, Sense::hover());
        let others = !self.cfg.cameras.is_empty();
        let strip_w = if others { (area.width() * 0.18).clamp(160.0, 280.0) } else { 0.0 };
        let left_w = area.width() - strip_w - if others { 8.0 } else { 0.0 };
        let bar = Rect::from_min_size(area.min, vec2(left_w, 34.0));
        let controls = Rect::from_min_max(pos2(area.left(), area.bottom() - 64.0), pos2(area.left() + left_w, area.bottom()));
        let main = Rect::from_min_max(pos2(area.left(), bar.bottom() + 6.0), pos2(area.left() + left_w, controls.top() - 6.0));

        self.playback_toolbar(ui, bar);
        self.playback_video(ui, main);
        self.playback_controls(ui, controls);
        if others {
            let strip = Rect::from_min_max(pos2(area.right() - strip_w, area.top()), area.max);
            self.filmstrip(ui, strip, None);
        }
    }

    fn playback_toolbar(&mut self, ui: &mut egui::Ui, bar: Rect) {
        let Some(pb) = &self.playback else { return };
        let clip = pb.clip.clone();
        let cam_name =
            pb.cam.and_then(|i| self.cfg.cameras.get(i)).map(|c| c.name.clone()).unwrap_or_else(|| clip.camera_slug.clone());
        let when = util::clip_start(&clip).map(|t| t.format("%H:%M:%S").to_string()).unwrap_or_default();
        let ctx = ui.ctx().clone();
        ui.allocate_new_ui(egui::UiBuilder::new().max_rect(bar), |ui| {
            ui.horizontal_centered(|ui| {
                if ui.button("◀ Back to live").on_hover_text("Esc").clicked() {
                    self.close_playback();
                    return;
                }
                ui.label(RichText::new(format!("{cam_name}  ·  {} {when}", util::day_label(&clip.day))).size(16.0).strong());
                ui.label(RichText::new(util::bytes(clip.bytes)).color(theme::MUTED).small());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("📂 Show in folder").clicked() {
                        util::show_in_folder(&clip.path);
                    }
                    if ui.button("↗ Open in player").clicked() {
                        util::open_path(&clip.path);
                    }
                    let (prev, next) = self.neighbours(&clip);
                    if ui.add_enabled(next.is_some(), egui::Button::new("Newer ⏭")).clicked() {
                        if let Some(c) = next {
                            self.open_clip(&ctx, c, Duration::ZERO);
                        }
                    }
                    if ui.add_enabled(prev.is_some(), egui::Button::new("⏮ Older")).clicked() {
                        if let Some(c) = prev {
                            self.open_clip(&ctx, c, Duration::ZERO);
                        }
                    }
                });
            });
        });
    }

    /// The clips just before/after this one from the same camera.
    fn neighbours(
        &mut self,
        clip: &tapestry_core::clips::Clip,
    ) -> (Option<tapestry_core::clips::Clip>, Option<tapestry_core::clips::Clip>) {
        self.refresh_clips(false);
        let same: Vec<_> = self.clips.clips.iter().filter(|c| c.camera_slug == clip.camera_slug).collect(); // newest first
        let Some(k) = same.iter().position(|c| c.path == clip.path) else { return (None, None) };
        let newer = if k > 0 { Some(same[k - 1].clone()) } else { None };
        let older = same.get(k + 1).map(|c| (*c).clone());
        (older, newer)
    }

    fn playback_video(&mut self, ui: &mut egui::Ui, area: Rect) {
        let Some(pb) = &self.playback else { return };
        let painter = ui.painter_at(area);
        painter.rect_filled(area, 8.0, theme::TILE);
        let video = util::fit_aspect(area, ASPECT);
        if let Some(tex) = &pb.view.tex {
            painter.image(tex.id(), video, FULL_UV, Color32::WHITE);
        }
        let status = pb.view.stream.as_ref().map(|s| s.status());
        let paused = pb.paused_at.is_some();
        let overlay = match status {
            _ if paused => Some("❚❚  paused — Space to play".to_string()),
            Some(StreamStatus::Connecting) if pb.view.tex.is_none() => Some("Loading…".into()),
            Some(StreamStatus::Ended) => Some("Finished — Space to replay".into()),
            Some(StreamStatus::Failed(e)) => Some(format!("Can't play this clip: {e}")),
            _ => None,
        };
        if let Some(t) = overlay {
            pill(&painter, video.center(), Align2::CENTER_CENTER, &t, theme::TEXT, Color32::from_black_alpha(170), 15.0);
        }
        let resp = ui.interact(video, Id::new("pb-video"), Sense::click());
        if resp.clicked() {
            let ctx = ui.ctx().clone();
            self.toggle_pause(&ctx);
        }
    }

    fn playback_controls(&mut self, ui: &mut egui::Ui, rect: Rect) {
        let ctx = ui.ctx().clone();
        let Some(pb) = &self.playback else { return };
        let pos = pb.position();
        let len = pb.length;
        let speed = pb.speed;
        let paused =
            pb.paused_at.is_some() || pb.view.stream.as_ref().map_or(true, |s| matches!(s.status(), StreamStatus::Ended));
        let clip = pb.clip.clone();
        let clip_start_unix = util::clip_start(&clip).map(|t| t.timestamp());
        let spans: Vec<(f32, f32)> = match (clip_start_unix, len) {
            (Some(s0), Some(l)) => self
                .clips
                .motion
                .iter()
                .filter(|(slug, _)| *slug == clip.camera_slug)
                .flat_map(|(_, v)| v.iter())
                .filter_map(|m| {
                    let a = (m.start - s0) as f32;
                    let b = (m.end.max(m.start + 1) - s0) as f32;
                    let l = l.as_secs_f32();
                    (b > 0.0 && a < l).then(|| (a.max(0.0) / l, (b / l).min(1.0)))
                })
                .collect(),
            _ => Vec::new(),
        };

        ui.allocate_new_ui(egui::UiBuilder::new().max_rect(rect), |ui| {
            egui::Frame::none().fill(theme::PANEL).rounding(8.0).inner_margin(egui::Margin::symmetric(12.0, 8.0)).show(
                ui,
                |ui| {
                    ui.set_min_width(rect.width() - 24.0);
                    // Scrub bar.
                    let (bar, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 16.0), Sense::click_and_drag());
                    let p = ui.painter();
                    let track = Rect::from_center_size(bar.center(), vec2(bar.width(), 6.0));
                    p.rect_filled(track, 3.0, theme::RAISED);
                    for (a, b) in &spans {
                        let r = Rect::from_min_max(
                            pos2(track.left() + a * track.width(), track.top()),
                            pos2(track.left() + b * track.width(), track.bottom()),
                        );
                        p.rect_filled(r, 2.0, theme::MOTION.gamma_multiply(0.7));
                    }
                    if let Some(l) = len {
                        let frac = (pos.as_secs_f32() / l.as_secs_f32().max(0.1)).clamp(0.0, 1.0);
                        let done = Rect::from_min_max(track.min, pos2(track.left() + frac * track.width(), track.bottom()));
                        p.rect_filled(done, 3.0, theme::AMBER.gamma_multiply(0.8));
                        p.circle_filled(pos2(done.right(), track.center().y), 6.0, theme::AMBER);
                        if let Some(h) = resp.hover_pos() {
                            let hf = ((h.x - track.left()) / track.width()).clamp(0.0, 1.0);
                            let t = Duration::from_secs_f32(hf * l.as_secs_f32());
                            p.line_segment(
                                [pos2(h.x, track.top() - 4.0), pos2(h.x, track.bottom() + 4.0)],
                                Stroke::new(1.0_f32, theme::TEXT),
                            );
                            let wall = clip_start_unix
                                .and_then(|s| util::unix_to_local(s + t.as_secs() as i64))
                                .map(|d| d.format("  %H:%M:%S").to_string())
                                .unwrap_or_default();
                            resp.clone().on_hover_text_at_pointer(format!("{}{wall}", util::dur(t)));
                        }
                        if resp.clicked() || resp.drag_stopped() {
                            if let Some(h) = resp.interact_pointer_pos() {
                                let hf = ((h.x - track.left()) / track.width()).clamp(0.0, 1.0);
                                let at = Duration::from_secs_f32(hf * l.as_secs_f32());
                                self.seek(&ctx, at, true);
                            }
                        }
                    }

                    ui.horizontal(|ui| {
                        if ui.button(if paused { "▶" } else { "❚❚" }).on_hover_text("Space").clicked() {
                            self.toggle_pause(&ctx);
                        }
                        if ui.button("⟲ 10s").on_hover_text("←").clicked() {
                            self.seek(&ctx, pos.saturating_sub(Duration::from_secs(10)), true);
                        }
                        if ui.button("10s ⟳").on_hover_text("→").clicked() {
                            self.seek(&ctx, pos + Duration::from_secs(10), true);
                        }
                        let wall = clip_start_unix
                            .and_then(|s| util::unix_to_local(s + pos.as_secs() as i64))
                            .map(|d| d.format("%H:%M:%S").to_string());
                        ui.label(
                            RichText::new(format!("{} / {}", util::dur(pos), len.map(util::dur).unwrap_or_else(|| "–".into())))
                                .monospace(),
                        );
                        if let Some(w) = wall {
                            ui.label(RichText::new(format!("({w})")).color(theme::MUTED).monospace());
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            for s in [8.0f32, 4.0, 2.0, 1.0] {
                                if ui.selectable_label((speed - s).abs() < 0.01, format!("{s}×")).clicked()
                                    && (speed - s).abs() > 0.01
                                {
                                    if let Some(pb) = self.playback.as_mut() {
                                        pb.speed = s;
                                    }
                                    self.seek(&ctx, pos, true);
                                }
                            }
                            if !spans.is_empty() {
                                ui.label(RichText::new("▮ motion").color(theme::MOTION).small());
                            }
                        });
                    });
                },
            );
        });
        if !paused {
            ctx.request_repaint_after(Duration::from_millis(200));
        }
    }
}
