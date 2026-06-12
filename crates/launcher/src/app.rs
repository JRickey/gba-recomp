//! Application shell: chrome header with tab pills, content area, footer.

use eframe::egui::{self, Align2, FontId, Pos2, Rect, Vec2};

use crate::screens::{av::AvScreen, input::InputScreen, play::PlayScreen};
use crate::theme;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Play,
    Input,
    Audio,
    Video,
}

pub struct LauncherApp {
    tab: Tab,
    play: PlayScreen,
    input: InputScreen,
    av: AvScreen,
}

impl LauncherApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        theme::install(&cc.egui_ctx);
        Self {
            tab: Tab::Play,
            play: PlayScreen::new(),
            input: InputScreen::new(),
            av: AvScreen::new(),
        }
    }

    fn header(&mut self, ui: &mut egui::Ui) {
        let h = 56.0;
        let rect = Rect::from_min_size(ui.max_rect().min, Vec2::new(ui.max_rect().width(), h));
        theme::chrome_strip(ui.painter(), rect);
        theme::wordmark(
            ui.painter(),
            Pos2::new(rect.left() + 18.0, rect.center().y - 6.0),
            "GBA \u{25CF} RECOMP",
            24.0,
        );
        ui.painter().text(
            Pos2::new(rect.left() + 20.0, rect.center().y + 15.0),
            Align2::LEFT_CENTER,
            "S T A T I C   R E C O M P I L A T I O N   S Y S T E M",
            FontId::proportional(9.0),
            theme::INDIGO,
        );

        // tab pills, right-aligned in the strip
        let tabs = [
            (Tab::Play, "PLAY"),
            (Tab::Input, "INPUT"),
            (Tab::Audio, "AUDIO"),
            (Tab::Video, "VIDEO"),
        ];
        // Shrink the pills before they can run into the wordmark block.
        let gap = 8.0;
        let room = rect.width() - 18.0 - 300.0 - gap * (tabs.len() - 1) as f32;
        let pill = Vec2::new((room / tabs.len() as f32).clamp(64.0, 88.0), 30.0);
        let total = pill.x * tabs.len() as f32 + gap * (tabs.len() - 1) as f32;
        let mut cursor = Pos2::new(rect.right() - 18.0 - total, rect.center().y - pill.y / 2.0);
        for (tab, label) in tabs {
            let r = Rect::from_min_size(cursor, pill);
            let mut child = ui.new_child(egui::UiBuilder::new().max_rect(r));
            if theme::glossy_button(&mut child, label, self.tab == tab, pill).clicked() {
                self.tab = tab;
            }
            cursor.x += pill.x + gap;
        }
        ui.advance_cursor_after_rect(rect);
    }

    fn scrolled(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, add);
    }

    fn footer(&self, ui: &mut egui::Ui) {
        let h = 22.0;
        let rect = Rect::from_min_size(
            Pos2::new(ui.max_rect().left(), ui.max_rect().bottom() - h),
            Vec2::new(ui.max_rect().width(), h),
        );
        theme::chrome_strip(ui.painter(), rect);
        ui.painter().text(
            Pos2::new(rect.left() + 10.0, rect.center().y),
            Align2::LEFT_CENTER,
            format!("Version {} — JRickey", env!("CARGO_PKG_VERSION")),
            FontId::proportional(10.0),
            theme::INK,
        );
        ui.painter().text(
            Pos2::new(rect.right() - 10.0, rect.center().y),
            Align2::RIGHT_CENTER,
            "BEST ENJOYED AT 240 \u{00D7} 160",
            FontId::proportional(10.0),
            theme::INDIGO,
        );
    }
}

impl eframe::App for LauncherApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let t = ui.ctx().input(|i| i.time);
        theme::paint_background(ui, t);
        self.header(ui);
        self.footer(ui);

        let content = Rect::from_min_max(
            ui.max_rect().min + Vec2::new(16.0, 72.0),
            ui.max_rect().max - Vec2::new(16.0, 38.0),
        );
        let mut inner = ui.new_child(egui::UiBuilder::new().max_rect(content));
        // Nothing a screen draws may spill onto the header/footer chrome.
        inner.shrink_clip_rect(content);
        match self.tab {
            Tab::Play => self.play.ui(&mut inner),
            Tab::Input => self.input.ui(&mut inner),
            // Settings can outgrow small windows; scroll instead of clipping.
            Tab::Audio => Self::scrolled(&mut inner, |ui| self.av.audio_ui(ui)),
            Tab::Video => Self::scrolled(&mut inner, |ui| self.av.video_ui(ui)),
        }

        // gentle ambient animation
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(50));
    }
}
