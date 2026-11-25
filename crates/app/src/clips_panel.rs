//! Right-hand panel: a 24-hour timeline per camera (recordings + motion)
//! and the list of clips for the chosen day.

use crate::app::TapestryApp;
use crate::{theme, util};
use eframe::egui::{self, pos2, vec2, Align2, Color32, FontId, Rect, RichText, Sense, Stroke};
use std::time::{Duration, Instant};
use tapestry_core::clips::{self, Clip};

impl TapestryApp {
    pub fn refresh_clips(&mut self, force: bool) {
        if !force && self.clips.scanned.map_or(false, |t| t.elapsed() < Duration::from_secs(8)) {
            return;
        }
        let root = &self.cfg.recordings_dir;
        self.clips.clips = clips::scan(root);
        self.clips.usage = self.clips.clips.iter().map(|c| c.bytes).sum();
        self.clips.motion =
            self.cfg.cameras.iter().map(|c| (c.slug(), clips::read_motion(&clips::motion_log_path(root, &c.slug())))).collect();
        self.clips.scanned = Some(Instant::now());
    }

    fn days(&self) -> Vec<String> {
        let mut days: Vec<String> = self
            .clips
            .clips
            .iter()
            .filter(|c| self.clips_cam.as_ref().map_or(true, |s| &c.camera_slug == s))
            .map(|c| c.day.clone())
            .collect();
        days.push(util::today());
        days.sort();
        days.dedup();
        days.reverse(); // newest first
        days
    }

    pub fn clips_panel(&mut self, ctx: &egui::Context) {
        self.refresh_clips(false);
        egui::SidePanel::right("clips")
            .resizable(true)
            .default_width(380.0)
            .width_range(300.0..=620.0)
            .frame(egui::Frame::none().fill(theme::PANEL).inner_margin(egui::Margin::same(12.0)))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Clips").size(18.0).strong());
                    ui.label(RichText::new(util::bytes(self.clips.usage)).color(theme::MUTED).small())
                        .on_hover_text(format!("{}", self.cfg.recordings_dir.display()));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("×").clicked() {
                            self.show_clips = false;
                        }
                        if ui.small_button("📂 Folder").clicked() {
                            let _ = std::fs::create_dir_all(&self.cfg.recordings_dir);
                            util::open_path(&self.cfg.recordings_dir);
                        }
                        if ui.small_button("↻").on_hover_text("Rescan").clicked() {
                            self.refresh_clips(true);
                        }
                    });
                });

                // Camera filter.
                ui.horizontal_wrapped(|ui| {
                    if ui.selectable_label(self.clips_cam.is_none(), "All").clicked() {
                        self.clips_cam = None;
                    }
                    for c in self.cfg.cameras.clone() {
                        let slug = c.slug();
                        if ui.selectable_label(self.clips_cam.as_deref() == Some(slug.as_str()), c.name.as_str()).clicked() {
                            self.clips_cam = Some(slug);
                        }
                    }
                });

                // Day navigation.
                let days = self.days();
                if !days.contains(&self.clips_day) {
                    self.clips_day = days.first().cloned().unwrap_or_else(util::today);
                }
                let k = days.iter().position(|d| *d == self.clips_day).unwrap_or(0);
                ui.horizontal(|ui| {
                    if ui.add_enabled(k + 1 < days.len(), egui::Button::new("◀").small()).clicked() {
                        self.clips_day = days[k + 1].clone();
                    }
                    ui.label(RichText::new(util::day_label(&self.clips_day)).strong());
                    ui.label(RichText::new(&self.clips_day).color(theme::MUTED).small());
                    if ui.add_enabled(k > 0, egui::Button::new("▶").small()).clicked() {
                        self.clips_day = days[k - 1].clone();
                    }
                });
                ui.add_space(4.0);
                self.timeline(ui);
                ui.add_space(6.0);
                ui.separator();
                self.clip_list(ui);
            });
    }

    fn timeline(&mut self, ui: &mut egui::Ui) {
        let day = self.clips_day.clone();
        let cams: Vec<(usize, String, String)> = self
            .cfg
            .cameras
            .iter()
            .enumerate()
            .filter(|(_, c)| self.clips_cam.as_ref().map_or(true, |s| *s == c.slug()))
            .map(|(i, c)| (i, c.name.clone(), c.slug()))
            .collect();
        let w = ui.available_width();
        let row_h = 30.0;
        let (rect, _) = ui.allocate_exact_size(vec2(w, row_h * cams.len() as f32 + 16.0), Sense::hover());
        let painter = ui.painter_at(rect);
        let to_x = |secs: f32, r: &Rect| r.left() + (secs / 86_400.0).clamp(0.0, 1.0) * r.width();

        let now_secs = if day == util::today() { util::secs_into_day(&day, &chrono::Local::now()) } else { None };
        let mut open: Option<(Clip, Duration)> = None;
        let mut miss: Option<f32> = None;

        for (row, (_, name, slug)) in cams.iter().enumerate() {
            let top = rect.top() + row as f32 * row_h;
            painter.text(pos2(rect.left(), top), Align2::LEFT_TOP, name, FontId::proportional(11.0), theme::MUTED);
            let strip = Rect::from_min_size(pos2(rect.left(), top + 14.0), vec2(w, 12.0));
            painter.rect_filled(strip, 3.0, theme::RAISED);

            let day_clips: Vec<&Clip> = self.clips.clips.iter().filter(|c| c.camera_slug == *slug && c.day == day).collect();
            let spans: Vec<(f32, f32, &Clip)> = day_clips
                .iter()
                .filter_map(|c| {
                    let start = util::clip_start(c)?;
                    let s = util::secs_into_day(&day, &start)?;
                    let len = util::clip_len(c).map(|d| d.as_secs_f32()).unwrap_or(30.0).max(20.0);
                    Some((s, s + len, *c))
                })
                .collect();
            for (a, b, _) in &spans {
                let r = Rect::from_min_max(
                    pos2(to_x(*a, &strip), strip.top()),
                    pos2(to_x(*b, &strip).max(to_x(*a, &strip) + 2.0), strip.bottom()),
                );
                painter.rect_filled(r, 2.0, theme::TEAL.gamma_multiply(0.75));
            }
            if let Some((_, m)) = self.clips.motion.iter().find(|(s, _)| s == slug) {
                for span in m {
                    let (Some(a), Some(b)) =
                        (util::unix_to_local(span.start), util::unix_to_local(span.end.max(span.start + 20)))
                    else {
                        continue;
                    };
                    let (Some(a), Some(b)) = (util::secs_into_day(&day, &a), util::secs_into_day(&day, &b)) else { continue };
                    if b < 0.0 || a > 86_400.0 {
                        continue;
                    }
                    let r = Rect::from_min_max(
                        pos2(to_x(a, &strip), strip.top() + 3.0),
                        pos2(to_x(b, &strip).max(to_x(a, &strip) + 2.0), strip.bottom() - 3.0),
                    );
                    painter.rect_filled(r, 1.0, theme::MOTION);
                }
            }

            let id = ui.id().with(("timeline", slug));
            let resp = ui.interact(strip, id, Sense::click());
            if let Some(h) = resp.hover_pos() {
                let secs = ((h.x - strip.left()) / strip.width()).clamp(0.0, 1.0) * 86_400.0;
                painter
                    .line_segment([pos2(h.x, strip.top() - 2.0), pos2(h.x, strip.bottom() + 2.0)], Stroke::new(1.0_f32, theme::TEXT));
                let s = secs as u32;
                resp.clone().on_hover_text_at_pointer(format!("{:02}:{:02} — click to play", s / 3600, (s % 3600) / 60));
            }
            if resp.clicked() {
                if let Some(h) = resp.interact_pointer_pos() {
                    let secs = ((h.x - strip.left()) / strip.width()).clamp(0.0, 1.0) * 86_400.0;
                    // Clip covering that moment, else the next one within 3 minutes.
                    // (A pixel is ~2 minutes wide on a narrow panel, so be forgiving.)
                    let slack = 86_400.0 / strip.width() * 3.0;
                    let hit = spans
                        .iter()
                        .find(|(a, b, _)| secs >= a - slack && secs <= *b)
                        .map(|(a, _, c)| ((*c).clone(), Duration::from_secs_f32((secs - a).max(0.0))))
                        .or_else(|| {
                            spans
                                .iter()
                                .filter(|(a, _, _)| *a >= secs && *a - secs < 180.0)
                                .min_by(|x, y| x.0.total_cmp(&y.0))
                                .map(|(_, _, c)| ((*c).clone(), Duration::ZERO))
                        });
                    match hit {
                        Some(h) => open = Some(h),
                        None => miss = Some(secs),
                    }
                }
            }
            if let Some(n) = now_secs {
                let x = to_x(n, &strip);
                painter.line_segment([pos2(x, strip.top() - 2.0), pos2(x, strip.bottom() + 2.0)], Stroke::new(2.0_f32, theme::AMBER));
            }
        }

        // Hour labels.
        let base = rect.bottom() - 12.0;
        for h in [0, 6, 12, 18, 24] {
            let x = rect.left() + h as f32 / 24.0 * w;
            let anchor = match h {
                0 => Align2::LEFT_TOP,
                24 => Align2::RIGHT_TOP,
                _ => Align2::CENTER_TOP,
            };
            painter.text(pos2(x, base), anchor, format!("{h:02}:00"), FontId::proportional(10.0), theme::MUTED);
        }
        if cams.is_empty() {
            painter.text(rect.center(), Align2::CENTER_CENTER, "No cameras yet", FontId::proportional(12.0), theme::MUTED);
        }

        let ctx = ui.ctx().clone();
        if let Some((clip, at)) = open {
            self.open_clip(&ctx, clip, at);
        } else if let Some(secs) = miss {
            let s = secs as u32;
            self.toast(format!("No recording at {:02}:{:02}", s / 3600, (s % 3600) / 60), theme::MUTED);
        }
    }

    fn clip_list(&mut self, ui: &mut egui::Ui) {
        let day = self.clips_day.clone();
        let list: Vec<Clip> = self
            .clips
            .clips
            .iter()
            .filter(|c| c.day == day && self.clips_cam.as_ref().map_or(true, |s| &c.camera_slug == s))
            .cloned()
            .collect();
        let names: Vec<(String, String)> = self.cfg.cameras.iter().map(|c| (c.slug(), c.name.clone())).collect();
        let recording: Vec<std::path::PathBuf> = self.cams.iter().filter_map(|c| c.recording.clone()).collect();
        let current = self.playback.as_ref().map(|p| p.clip.path.clone());
        let mut open = None;

        if list.is_empty() {
            ui.add_space(12.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("No clips on this day").color(theme::MUTED));
                let hint = match self.cfg.record_mode {
                    tapestry_core::RecordMode::Off => "Recording is off — turn it on in Settings.",
                    tapestry_core::RecordMode::Motion => "Clips appear here when a camera sees motion.",
                    tapestry_core::RecordMode::Continuous => "Recordings appear here every few minutes.",
                };
                ui.label(RichText::new(hint).color(theme::MUTED).small());
            });
            return;
        }

        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            for c in &list {
                let name =
                    names.iter().find(|(s, _)| *s == c.camera_slug).map(|(_, n)| n.as_str()).unwrap_or(c.camera_slug.as_str());
                let time = c.hms().map(|(h, m, s)| format!("{h:02}:{m:02}:{s:02}")).unwrap_or_default();
                let live = recording.contains(&c.path);
                let len = if live {
                    "recording…".to_string()
                } else {
                    util::clip_len(c).map(util::dur).unwrap_or_else(|| "–".into())
                };
                let selected = current.as_ref() == Some(&c.path);

                let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 30.0), Sense::click());
                let p = ui.painter();
                let bg = if selected {
                    theme::AMBER.gamma_multiply(0.25)
                } else if resp.hovered() {
                    theme::RAISED
                } else {
                    Color32::TRANSPARENT
                };
                p.rect_filled(rect, 6.0, bg);
                let y = rect.center().y;
                p.text(pos2(rect.left() + 8.0, y), Align2::LEFT_CENTER, &time, FontId::monospace(13.0), theme::TEXT);
                p.text(pos2(rect.left() + 86.0, y), Align2::LEFT_CENTER, name, FontId::proportional(13.0), theme::TEXT);
                let right = format!("{len}   {}", util::bytes(c.bytes));
                p.text(
                    pos2(rect.right() - 8.0, y),
                    Align2::RIGHT_CENTER,
                    right,
                    FontId::proportional(12.0),
                    if live { theme::MOTION } else { theme::MUTED },
                );
                if resp.clicked() {
                    open = Some(c.clone());
                }
                resp.context_menu(|ui| {
                    if ui.button("▶ Play").clicked() {
                        open = Some(c.clone());
                        ui.close_menu();
                    }
                    if ui.button("↗ Open in player").clicked() {
                        util::open_path(&c.path);
                        ui.close_menu();
                    }
                    if ui.button("📂 Show in folder").clicked() {
                        util::show_in_folder(&c.path);
                        ui.close_menu();
                    }
                });
            }
        });

        if let Some(c) = open {
            let ctx = ui.ctx().clone();
            self.open_clip(&ctx, c, Duration::ZERO);
        }
    }
}
