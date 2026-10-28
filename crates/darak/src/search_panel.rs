//! 검색 결과 패널 (가상 스크롤 표)

use darak_core::i18n::{plural, tr};
use darak_core::{Kind, NodeId, SizeMode, format_time, human_size, thousands};
use darak_core::tr;
use eframe::egui::{self, RichText, Sense};
use crate::doc::Doc;
use crate::theme;

pub fn controls(ui: &mut egui::Ui, doc: &mut Doc) -> bool {
    let mut changed = false;
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        changed = ui.checkbox(&mut doc.search.in_focus_only, tr("현재 폴더 안에서만", "Only in current folder")).changed();
        if changed && doc.focus != darak_core::Tree::ROOT {
            doc.search.note_edit(std::time::Instant::now());
        }
        ui.checkbox(&mut doc.search.highlight, tr("treemap에 강조", "Highlight in treemap"));
    });
    changed
}

pub enum Action {
    None,
    Reveal(NodeId),
    Context(NodeId),
    /// 더블클릭: 파일을 기본 앱으로 연다
    Open(NodeId),
}

pub fn results(ui: &mut egui::Ui, doc: &mut Doc, mode: SizeMode) -> Action {
    let mut action = Action::None;
    let s = &mut doc.search;
    if let Some(e) = &s.error {
        ui.colored_label(theme::of(ui).danger_text, e);
        return action;
    }
    let Some(res) = &s.result else {
        ui.label(RichText::new(tr("검색 중…", "Searching…")).weak());
        return action;
    };
    let (n, size, ms) = (thousands(res.total as u64), human_size(res.total_size), s.elapsed_ms);
    let results = plural(res.total as u64, "result", "results");
    ui.label(tr!("{n}개 · {size} · {ms:.0} ms", "{n} {results} · {size} · {ms:.0} ms"));
    ui.separator();

    let row_h = 20.0;
    let hits = &res.hits;
    let tree = &doc.tree;
    let selected = doc.selected;
    egui::ScrollArea::vertical().auto_shrink([false, false]).show_rows(ui, row_h, hits.len(), |ui, range| {
        let w = ui.available_width();
        for &id in &hits[range] {
            let n = tree.node(id);
            let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, row_h), Sense::click());
            let painter = ui.painter_at(rect);
            let visuals = ui.visuals();
            if selected == Some(id) {
                painter.rect_filled(rect, 3.0, visuals.selection.bg_fill);
            } else if resp.hovered() {
                painter.rect_filled(rect, 3.0, visuals.widgets.hovered.weak_bg_fill);
            }
            let text = visuals.text_color();
            let weak = visuals.weak_text_color();
            let icon = match n.kind {
                Kind::Dir => "📁",
                Kind::Link => "🔗",
                Kind::File => "📄",
            };
            let size_w = 74.0;
            let date_w = if w > 420.0 { 112.0 } else { 0.0 };
            let name_w = (w - size_w - date_w - 12.0).max(40.0);
            let clip = painter.with_clip_rect(egui::Rect::from_min_size(rect.min, egui::vec2(name_w, row_h)));
            clip.text(
                rect.left_center() + egui::vec2(4.0, 0.0),
                egui::Align2::LEFT_CENTER,
                format!("{icon} {}", tree.name(id)),
                egui::FontId::proportional(13.0),
                text,
            );
            painter.text(
                egui::pos2(rect.min.x + name_w + size_w, rect.center().y),
                egui::Align2::RIGHT_CENTER,
                human_size(n.size(mode)),
                egui::FontId::monospace(12.0),
                text,
            );
            if date_w > 0.0 {
                painter.text(
                    egui::pos2(rect.max.x - 4.0, rect.center().y),
                    egui::Align2::RIGHT_CENTER,
                    if n.kind == Kind::File { format_time(n.mtime) } else { String::new() },
                    egui::FontId::proportional(12.0),
                    weak,
                );
            }
            if resp.double_clicked() {
                action = Action::Open(id);
            } else if resp.clicked() {
                action = Action::Reveal(id);
            }
            if resp.secondary_clicked() {
                action = Action::Context(id);
            }
            resp.on_hover_ui(|ui| { ui.label(tree.path(id)); });
        }
    });
    action
}
