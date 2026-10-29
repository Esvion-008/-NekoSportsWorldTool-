//! 运行日志：彩色等宽滚动，自动滚底，上限 2000 行。
//! 入站行的 √/×/⚠ 前缀仅用于着色，落盘展示时剥离。

use super::theme;
use crate::textlog::{classify, LogKind};
use egui::{Color32, RichText, ScrollArea};
use std::collections::VecDeque;

const MAX_LINES: usize = 2000;

#[derive(Debug, Clone)]
pub struct LogLine {
    pub text: String,
    pub kind: LogKind,
}

impl LogLine {
    fn color(&self) -> Color32 {
        match self.kind {
            LogKind::Plain => theme::plain(),
            LogKind::Ok => theme::ok(),
            LogKind::Err => theme::err(),
            LogKind::Warn => theme::warn(),
        }
    }
}

#[derive(Default)]
pub struct LogStore {
    lines: VecDeque<LogLine>,
}

impl LogStore {
    pub fn push(&mut self, msg: &str) {
        for line in msg.lines() {
            let t = line.trim();
            if t.is_empty() {
                continue;
            }
            if self.lines.len() >= MAX_LINES {
                self.lines.pop_front();
            }
            let (kind, text) = classify(t);
            self.lines.push_back(LogLine { text: text.to_string(), kind });
        }
    }

    #[allow(dead_code)]
    pub fn clear(&mut self) {
        self.lines.clear();
    }

    pub fn len(&self) -> usize {
        self.lines.len()
    }

    pub fn render(&mut self, ui: &mut egui::Ui) {
        // 虚拟滚动：只布局可视区内的行（中文文本 galley 布局开销大，
        // 全量渲染 2000 行会明显掉帧；show_rows 按行高裁剪，滚多快都只画可见行）
        self.render_tail(ui, f32::INFINITY);
    }

    /// 渲染最近若干行（账号页实时日志面板用），固定最大高度、自动滚底。
    pub fn render_tail(&mut self, ui: &mut egui::Ui, max_height: f32) {
        let font = egui::FontId::monospace(12.0);
        let row_height = ui.fonts(|f| f.row_height(&font)) + ui.spacing().item_spacing.y;
        let total = self.lines.len();
        let take = total.min(400);
        let start = total - take;
        ScrollArea::vertical()
            .id_salt("log_tail_scroll")
            .stick_to_bottom(true)
            .max_height(max_height)
            .auto_shrink([false, false])
            .show_rows(ui, row_height, take, |ui, range| {
                let slice = self.lines.iter().skip(start + range.start).take(range.len());
                for line in slice {
                    ui.label(
                        RichText::new(&line.text)
                            .monospace()
                            .size(12.0)
                            .color(line.color()),
                    );
                }
            });
    }
}
