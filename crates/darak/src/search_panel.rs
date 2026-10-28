//! 검색 결과 패널 (가상 스크롤 표)

use darak_core::i18n::{plural, tr};
use darak_core::{Kind, NodeId, SizeMode, format_time, human_size, thousands};
use darak_core::tr;
use eframe::egui::{self, RichText, Sense, text::{LayoutJob, TextFormat, TextWrapping}};
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
    // 「검색 입력 중」 보드: caption 12, text.secondary
    ui.label(theme::caption(tr!("결과 {n}개 · {size} · {ms:.0} ms", "{n} {results} · {size} · {ms:.0} ms")).weak());
    ui.separator();

    // 행 높이 24 (작은 컨트롤 · 최소 누름 영역). 안쪽 여백 8
    let row_h = theme::H_SMALL;
    let hits = &res.hits;
    let tree = &doc.tree;
    let selected = doc.selected;
    let t = theme::of(ui);
    egui::ScrollArea::vertical().auto_shrink([false, false]).show_rows(ui, row_h, hits.len(), |ui, range| {
        let w = ui.available_width();
        for &id in &hits[range] {
            let n = tree.node(id);
            let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, row_h), Sense::click());
            let painter = ui.painter_at(rect);
            let radius = egui::CornerRadius::same(theme::R_CONTROL);
            if selected == Some(id) {
                painter.rect_filled(rect, radius, t.line);
            } else if resp.hovered() {
                painter.rect_filled(rect, radius, t.surface_raised);
            }
            let icon = match n.kind {
                Kind::Dir => "📁",
                Kind::Link => "🔗",
                Kind::File => "📄",
            };
            // [8][이름 …][8][크기 74][날짜 112][8]: 넓을 때만 날짜
            let (size_w, date_w) = (74.0, if w > 420.0 { 112.0 } else { 0.0 });
            let right = rect.max.x - 8.0;
            let size_right = right - date_w;
            let name_w = (size_right - size_w - 8.0 - (rect.min.x + 8.0)).max(40.0);
            let mut job = LayoutJob::single_section(
                format!("{icon} {}", tree.name(id)),
                TextFormat::simple(egui::FontId::proportional(theme::BODY), t.text_primary),
            );
            job.wrap = TextWrapping { max_width: name_w, max_rows: 1, break_anywhere: true, overflow_character: Some('…') };
            let name = painter.layout_job(job);
            let at = egui::Align2::LEFT_CENTER.anchor_size(egui::pos2(rect.min.x + 8.0, rect.center().y), name.size());
            painter.galley(at.min, name, t.text_primary);
            painter.text(
                egui::pos2(size_right, rect.center().y),
                egui::Align2::RIGHT_CENTER,
                human_size(n.size(mode)),
                egui::FontId::monospace(theme::MONO),
                t.text_primary,
            );
            if date_w > 0.0 {
                painter.text(
                    egui::pos2(right, rect.center().y),
                    egui::Align2::RIGHT_CENTER,
                    if n.kind == Kind::File { format_time(n.mtime) } else { String::new() },
                    egui::FontId::proportional(theme::CAPTION),
                    t.text_secondary,
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
            resp.on_hover_ui(|ui| { ui.label(theme::mono(tree.path(id))); });
        }
    });
    action
}
