//! 검색 결과 패널 (가상 스크롤 표)

use std::time::{Duration, Instant};

use darak_core::search::{Query, now_secs2000, search};
use darak_core::{Kind, NodeId, SizeMode, Tree, format_time, human_size, thousands};
use eframe::egui::{self, Color32, RichText, Sense};

use crate::doc::Doc;

const DEBOUNCE: Duration = Duration::from_millis(150);

/// 입력이 멈추면 검색을 실행한다. 다시 그려야 하면 true.
pub fn update(doc: &mut Doc, mode: SizeMode) -> bool {
    let s = &mut doc.search;
    if s.query.trim().is_empty() {
        if s.result.is_some() || s.error.is_some() {
            s.result = None;
            s.error = None;
            s.applied = None;
            s.bits.clear();
        }
        return false;
    }
    let scope = if s.in_focus_only { doc.focus } else { Tree::ROOT };
    let key = (s.query.clone(), scope, mode, doc.tree.version);
    if s.applied.as_ref() == Some(&key) {
        return false;
    }
    if s.edited_at.is_some_and(|t| t.elapsed() < DEBOUNCE) {
        return true;
    }
    s.applied = Some(key);
    match Query::parse(&s.query, now_secs2000()) {
        Ok(q) if !q.is_empty() => {
            let t = Instant::now();
            s.result = Some(search(&doc.tree, &q, mode, scope, usize::MAX));
            s.elapsed_ms = t.elapsed().as_secs_f64() * 1000.0;
            s.error = None;
            doc.set_search_bits();
        }
        Ok(_) => {
            s.result = None;
            s.bits.clear();
        }
        Err(e) => {
            s.error = Some(e);
        }
    }
    false
}

pub enum Action {
    None,
    Reveal(NodeId),
    Context(NodeId),
    /// 더블클릭: 파일을 기본 앱으로 연다
    Open(NodeId),
}

pub fn panel(ui: &mut egui::Ui, doc: &mut Doc, mode: SizeMode) -> Action {
    let mut action = Action::None;
    let s = &mut doc.search;
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.checkbox(&mut s.in_focus_only, "현재 폴더 안에서만");
        ui.checkbox(&mut s.highlight, "treemap에 강조");
    });
    if let Some(e) = &s.error {
        ui.colored_label(Color32::from_rgb(230, 120, 60), e);
    }
    let Some(res) = &s.result else {
        ui.label(RichText::new("검색 중…").weak());
        return action;
    };
    ui.label(format!(
        "{}개 · {} · {:.0} ms",
        thousands(res.total as u64),
        human_size(res.total_size),
        s.elapsed_ms
    ));
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
                painter.rect_filled(rect, 2.0, visuals.selection.bg_fill);
            } else if resp.hovered() {
                painter.rect_filled(rect, 2.0, visuals.widgets.hovered.weak_bg_fill);
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
            resp.on_hover_text(tree.path(id));
        }
    });
    action
}
