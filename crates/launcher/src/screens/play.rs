//! Cartridge selection and launch: system file dialog, drag-and-drop,
//! and a persisted recently-played shelf.

use std::path::{Path, PathBuf};

use eframe::egui::{self, Align2, CornerRadius, FontId, Pos2, Rect, Sense, Stroke, StrokeKind, Ui,
    Vec2};

use crate::{platform, theme};

const MAX_RECENTS: usize = 10;

pub struct PlayScreen {
    recents: Vec<PathBuf>,
    status: String,
}

enum RowAction {
    None,
    Play,
    Forget,
}

impl PlayScreen {
    pub fn new() -> Self {
        Self { recents: load_recents(), status: String::new() }
    }

    pub fn ui(&mut self, ui: &mut Ui) {
        // files dropped anywhere count as an insert
        let dropped: Vec<PathBuf> = ui.ctx().input(|i| {
            i.raw.dropped_files.iter().filter_map(|f| f.path.clone()).collect()
        });
        for path in dropped {
            self.insert(path);
        }

        theme::glass_frame().show(ui, |ui| {
            ui.set_min_size(Vec2::new(ui.available_width(), ui.available_height()));
            theme::section_title(ui, "Cartridges");
            ui.add_space(6.0);

            // the big inviting button
            ui.vertical_centered(|ui| {
                let size = Vec2::new(260.0, 46.0);
                if theme::glossy_button(ui, "\u{25B6}  LOAD CARTRIDGE\u{2026}", true, size).clicked()
                {
                    if let Some(path) = platform::pick_rom() {
                        self.insert(path);
                    }
                }
                ui.add_space(2.0);
                ui.label(
                    egui::RichText::new("or drop a .gba file anywhere in this window")
                        .size(11.0)
                        .color(theme::white(120)),
                );
            });

            ui.add_space(12.0);
            theme::section_title(ui, "Recently Played");
            ui.add_space(4.0);

            if self.recents.is_empty() {
                ui.label(
                    egui::RichText::new("Nothing here yet — your shelf is waiting.")
                        .italics()
                        .color(theme::white(110)),
                );
            }

            let mut play: Option<PathBuf> = None;
            let mut forget: Option<usize> = None;
            egui::ScrollArea::vertical()
                .max_height(ui.available_height() - 26.0)
                .show(ui, |ui| {
                    for (i, path) in self.recents.iter().enumerate() {
                        match recent_row(ui, path) {
                            RowAction::Play => play = Some(path.clone()),
                            RowAction::Forget => forget = Some(i),
                            RowAction::None => {}
                        }
                        ui.add_space(4.0);
                    }
                });
            if let Some(i) = forget {
                self.recents.remove(i);
                save_recents(&self.recents);
            }
            if let Some(path) = play {
                self.launch(&path);
            }

            if !self.status.is_empty() {
                ui.add_space(2.0);
                ui.label(egui::RichText::new(&self.status).size(11.0).color(theme::CYAN));
            }
        });
    }

    fn insert(&mut self, path: PathBuf) {
        self.recents.retain(|p| *p != path);
        self.recents.insert(0, path);
        self.recents.truncate(MAX_RECENTS);
        save_recents(&self.recents);
    }

    fn launch(&mut self, path: &Path) {
        self.status = match platform::launch(path) {
            Ok(pid) => format!("\u{25B6} now playing: {} (pid {pid})", stem(path)),
            Err(e) => format!("\u{2716} {e}"),
        };
        // move to top of the shelf
        self.insert(path.to_path_buf());
    }
}

fn stem(path: &Path) -> String {
    path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
}

/// One shelf row: orb, name, location, hover PLAY chevron, forget cross.
fn recent_row(ui: &mut Ui, path: &Path) -> RowAction {
    let width = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, 40.0), Sense::click());
    if !ui.is_rect_visible(rect) {
        return RowAction::None;
    }
    let hovered = resp.hovered();
    let exists = path.is_file();
    let p = ui.painter();

    let cr = CornerRadius::same(9);
    p.rect_filled(rect, cr, theme::white(if hovered { 30 } else { 12 }));
    p.rect_stroke(rect, cr, Stroke::new(1.0, theme::white(if hovered { 90 } else { 30 })),
        StrokeKind::Inside);

    theme::orb(
        p,
        Pos2::new(rect.left() + 20.0, rect.center().y),
        7.0,
        if exists { theme::LIME } else { theme::SILVER_LO },
    );
    p.text(
        Pos2::new(rect.left() + 38.0, rect.center().y - 8.0),
        Align2::LEFT_CENTER,
        stem(path),
        FontId::proportional(14.0),
        theme::SILVER_HI,
    );
    let location = if exists {
        path.parent().map(|d| d.display().to_string()).unwrap_or_default()
    } else {
        "missing — was it moved?".to_string()
    };
    p.text(
        Pos2::new(rect.left() + 38.0, rect.center().y + 9.0),
        Align2::LEFT_CENTER,
        location,
        FontId::proportional(10.0),
        theme::white(110),
    );

    // forget cross (always present, quiet)
    let cross = Rect::from_center_size(
        Pos2::new(rect.right() - 18.0, rect.center().y),
        Vec2::splat(16.0),
    );
    let cross_resp = ui.interact(cross, resp.id.with("forget"), Sense::click());
    let cc = if cross_resp.hovered() { theme::AMBER } else { theme::white(90) };
    p.line_segment([cross.min + Vec2::splat(4.0), cross.max - Vec2::splat(4.0)],
        Stroke::new(1.6, cc));
    p.line_segment(
        [Pos2::new(cross.min.x + 4.0, cross.max.y - 4.0), Pos2::new(cross.max.x - 4.0, cross.min.y + 4.0)],
        Stroke::new(1.6, cc),
    );

    if hovered && exists {
        p.text(
            Pos2::new(rect.right() - 44.0, rect.center().y),
            Align2::RIGHT_CENTER,
            "PLAY \u{25B8}",
            FontId::proportional(12.0),
            theme::CYAN,
        );
    }

    if cross_resp.clicked() {
        RowAction::Forget
    } else if resp.clicked() && exists {
        RowAction::Play
    } else {
        RowAction::None
    }
}

fn recents_file() -> Option<PathBuf> {
    platform::config_dir().map(|d| d.join("recents.txt"))
}

fn load_recents() -> Vec<PathBuf> {
    let Some(f) = recents_file() else { return Vec::new() };
    let Ok(text) = std::fs::read_to_string(f) else { return Vec::new() };
    text.lines().filter(|l| !l.trim().is_empty()).map(PathBuf::from).take(MAX_RECENTS).collect()
}

fn save_recents(recents: &[PathBuf]) {
    if let Some(f) = recents_file() {
        let text: String =
            recents.iter().map(|p| format!("{}\n", p.display())).collect();
        let _ = std::fs::write(f, text);
    }
}
