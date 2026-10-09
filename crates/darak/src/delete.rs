//! 삭제 검토 창과 백그라운드 삭제 작업

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::sync::mpsc;

use darak_core::remove::{delete_permanently, protected_reason, recycle, top_level_marks};
use darak_core::{Kind, NodeId, SizeMode, human_size, thousands};
use eframe::egui::{self, Color32, RichText};

use crate::doc::Doc;
use crate::theme;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Recycle,
    Permanent,
}

pub struct Item {
    pub node: NodeId,
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
}

pub struct Outcome {
    pub node: NodeId,
    pub path: String,
    pub size: u64,
    pub error: Option<String>,
}

pub enum Stage {
    Review { method: Method, confirmed: bool, error: Option<String> },
    Running { total: usize, done: Arc<AtomicUsize>, rx: mpsc::Receiver<Vec<Outcome>> },
    Report { outcomes: Vec<Outcome> },
}

pub struct Dialog {
    pub doc_id: u64,
    pub stage: Stage,
}

/// 창이 닫혀야 하면 false. 삭제된 노드는 `removed`에 넣는다.
pub fn show(ctx: &egui::Context, dlg: &mut Dialog, mut doc: Option<&mut Doc>, mode: SizeMode, removed: &mut Vec<NodeId>) -> bool {
    if matches!(dlg.stage, Stage::Review { .. }) && doc.as_ref().is_none_or(|doc| doc.id != dlg.doc_id) {
        return false;
    }
    let mut open = true;
    let mut next: Option<Stage> = None;
    let modal = egui::Modal::new(egui::Id::new("delete-review")).show(ctx, |ui| {
        ui.set_width(640.0);
        match &mut dlg.stage {
            Stage::Review { method, confirmed, error } => {
                let doc = doc.as_mut().expect("review document identity checked");
                let items: Vec<NodeId> = top_level_marks(&doc.tree, &doc.marks.iter().copied().collect::<Vec<_>>());
                let total: u64 = items.iter().map(|&i| doc.tree.node(i).size(mode)).sum();
                let files: u64 = items.iter().map(|&i| doc.tree.node(i).files as u64).sum();
                ui.heading("삭제 검토");
                ui.label(format!("{}개 항목 · {} · 파일 {}개", items.len(), human_size(total), thousands(files)));
                ui.add_space(6.0);
                let mut unmark = None;
                egui::ScrollArea::vertical().max_height(320.0).auto_shrink([false, true]).show(ui, |ui| {
                    for &id in &items {
                        ui.horizontal(|ui| {
                            if ui.small_button("✖").on_hover_text("표시 해제").clicked() {
                                unmark = Some(id);
                            }
                            let n = doc.tree.node(id);
                            ui.add_sized([80.0, 18.0], egui::Label::new(RichText::new(human_size(n.size(mode))).monospace()));
                            let icon = if n.kind == Kind::Dir { "📁" } else { "📄" };
                            ui.label(format!("{icon} {}", doc.tree.path(id)));
                            if let Some(r) = protected_reason(&doc.tree, id) {
                                ui.colored_label(theme::of(ui).warn, format!("⚠ {r}"));
                            }
                        });
                    }
                });
                if let Some(u) = unmark
                    && doc.toggle_mark(u).is_ok() {
                    *error = None;
                }
                ui.separator();
                if ui.radio_value(method, Method::Recycle, "휴지통으로 이동 (되돌릴 수 있음)").changed() {
                    *error = None;
                }
                if ui.radio_value(method, Method::Permanent, "영구 삭제 (되돌릴 수 없음)").changed() {
                    *error = None;
                }
                if *method == Method::Permanent
                    && ui.checkbox(confirmed, RichText::new("되돌릴 수 없다는 것을 이해했습니다").color(theme::of(ui).danger_text)).changed() {
                    *error = None;
                }
                ui.add_space(6.0);
                let any_protected = items.iter().any(|&i| protected_reason(&doc.tree, i).is_some());
                let can_run = !items.is_empty() && !any_protected && (*method == Method::Recycle || *confirmed);
                ui.horizontal(|ui| {
                    let label = match method {
                        Method::Recycle => format!("🗑 휴지통으로 이동 ({})", human_size(total)),
                        Method::Permanent => format!("⚠ 영구 삭제 ({})", human_size(total)),
                    };
                    let btn = egui::Button::new(RichText::new(label).strong().color(Color32::WHITE)).fill(theme::of(ui).danger_fill);
                    if ui.add_enabled(can_run, btn).clicked() {
                        match build_items(doc, &items, mode) {
                            Ok(jobs) => next = Some(start(jobs, *method)),
                            Err(message) => *error = Some(message),
                        }
                    }
                    if ui.button("취소").clicked() {
                        open = false;
                    }
                });
                if let Some(message) = error.as_deref() {
                    ui.colored_label(theme::of(ui).danger_text, message);
                }
            }
            Stage::Running { total, done, rx } => {
                ui.heading("삭제 중…");
                let d = done.load(Relaxed);
                ui.add(egui::ProgressBar::new(d as f32 / (*total).max(1) as f32).text(format!("{d} / {total}")));
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
                if let Ok(outcomes) = rx.try_recv() {
                    removed.extend(outcomes.iter().filter(|o| o.error.is_none()).map(|o| o.node));
                    next = Some(Stage::Report { outcomes });
                }
            }
            Stage::Report { outcomes } => {
                let ok: Vec<&Outcome> = outcomes.iter().filter(|o| o.error.is_none()).collect();
                let failed: Vec<&Outcome> = outcomes.iter().filter(|o| o.error.is_some()).collect();
                let freed: u64 = ok.iter().map(|o| o.size).sum();
                ui.heading("삭제 결과");
                ui.colored_label(theme::of(ui).ok, format!("성공 {}개 · {} 확보", ok.len(), human_size(freed)));
                if !failed.is_empty() {
                    ui.colored_label(theme::of(ui).danger_text, format!("실패 {}개", failed.len()));
                    egui::ScrollArea::vertical().max_height(240.0).show(ui, |ui| {
                        for o in failed {
                            ui.label(RichText::new(&o.path).strong());
                            ui.label(RichText::new(o.error.as_deref().unwrap_or("")).weak());
                        }
                    });
                }
                if ui.button("닫기").clicked() {
                    open = false;
                }
            }
        }
    });
    if let Some(n) = next {
        dlg.stage = n;
    }
    if modal.should_close() && !matches!(dlg.stage, Stage::Running { .. }) {
        open = false;
    }
    open
}

/// 실제 파일 작업 직전에 목록 전체의 보호 정책을 다시 검사한다.
fn build_items(doc: &Doc, ids: &[NodeId], mode: SizeMode) -> Result<Vec<Item>, String> {
    ids.iter()
        .map(|&id| {
            if id as usize >= doc.tree.len() {
                return Err("삭제할 항목이 현재 문서에 없습니다".into());
            }
            if doc.tree.node(id).has(darak_core::flags::REMOVED) {
                return Err("이미 삭제된 항목입니다".into());
            }
            if let Some(reason) = protected_reason(&doc.tree, id) {
                return Err(format!("보호된 항목: {reason}"));
            }
            let n = doc.tree.node(id);
            Ok(Item {
                node: id,
                path: doc.tree.path(id),
                is_dir: n.kind == Kind::Dir,
                size: n.size(mode),
            })
        })
        .collect()
}

fn start(items: Vec<Item>, method: Method) -> Stage {
    let total = items.len();
    let done = Arc::new(AtomicUsize::new(0));
    let (tx, rx) = mpsc::channel();
    {
        let done = done.clone();
        std::thread::spawn(move || {
            let mut out = Vec::with_capacity(items.len());
            for it in items {
                let res = match method {
                    Method::Recycle => recycle(&it.path),
                    Method::Permanent => delete_permanently(&it.path, it.is_dir),
                };
                out.push(Outcome { node: it.node, path: it.path, size: it.size, error: res.err().map(|e| e.to_string()) });
                done.fetch_add(1, Relaxed);
            }
            let _ = tx.send(out);
        });
    }
    Stage::Running { total, done, rx }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::tests::fixture;
    use darak_core::Tree;

    const REVIEW_ERROR: &str = "보호된 항목: 테스트";

    fn review_dialog(method: Method) -> Dialog {
        Dialog {
            doc_id: 1,
            stage: Stage::Review {
                method,
                confirmed: false,
                error: Some(REVIEW_ERROR.into()),
            },
        }
    }

    fn render_review_frame(
        ctx: &egui::Context,
        dlg: &mut Dialog,
        doc: &mut Doc,
        events: Vec<egui::Event>,
        time: f64,
    ) -> egui::FullOutput {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1000.0, 800.0),
            )),
            events,
            time: Some(time),
            ..Default::default()
        };
        let mut removed = Vec::new();
        let mut output = ctx.run_ui(input, |ui| {
            assert!(show(ui.ctx(), dlg, Some(doc), SizeMode::Allocated, &mut removed));
        });
        output.textures_delta.clear();
        assert!(removed.is_empty());
        output
    }

    fn shape_has_text(shape: &egui::epaint::Shape, text: &str) -> bool {
        match shape {
            egui::epaint::Shape::Text(shape) => shape.galley.text().contains(text),
            egui::epaint::Shape::Vec(shapes) => shapes.iter().any(|shape| shape_has_text(shape, text)),
            _ => false,
        }
    }

    fn text_center(shape: &egui::epaint::Shape, text: &str) -> Option<egui::Pos2> {
        match shape {
            egui::epaint::Shape::Text(shape) if shape.galley.text() == text => {
                Some(shape.pos + shape.galley.size() * 0.5)
            }
            egui::epaint::Shape::Vec(shapes) => shapes.iter().find_map(|shape| text_center(shape, text)),
            _ => None,
        }
    }

    #[test]
    fn review_error_survives_idle_frames() {
        let ctx = egui::Context::default();
        let mut doc = fixture(1);
        doc.toggle_mark(3).unwrap();
        let mut dlg = review_dialog(Method::Recycle);
        for frame in 0..4 {
            let output = render_review_frame(&ctx, &mut dlg, &mut doc, vec![], frame as f64 * 0.1);
            if frame >= 2 {
                assert!(output.shapes.iter().any(|shape| shape_has_text(&shape.shape, REVIEW_ERROR)));
                assert!(matches!(&dlg.stage, Stage::Review { error: Some(error), .. } if error == REVIEW_ERROR));
            }
        }
    }

    #[test]
    fn review_changes_clear_error() {
        for (method, label) in [
            (Method::Recycle, "영구 삭제 (되돌릴 수 없음)"),
            (Method::Permanent, "휴지통으로 이동 (되돌릴 수 있음)"),
            (Method::Permanent, "되돌릴 수 없다는 것을 이해했습니다"),
            (Method::Recycle, "✖"),
        ] {
            let ctx = egui::Context::default();
            let mut doc = fixture(1);
            doc.toggle_mark(3).unwrap();
            let mut dlg = review_dialog(method);
            let mut output = render_review_frame(&ctx, &mut dlg, &mut doc, vec![], 0.0);
            for frame in 1..4 {
                output = render_review_frame(&ctx, &mut dlg, &mut doc, vec![], frame as f64 * 0.1);
            }
            let pos = output.shapes.iter().find_map(|shape| text_center(&shape.shape, label))
                .unwrap_or_else(|| panic!("review control missing: {label}"));
            for (time, pressed) in [(0.4, true), (0.5, false)] {
                render_review_frame(&ctx, &mut dlg, &mut doc, vec![
                    egui::Event::PointerMoved(pos),
                    egui::Event::PointerButton {
                        pos,
                        button: egui::PointerButton::Primary,
                        pressed,
                        modifiers: egui::Modifiers::default(),
                    },
                ], time);
            }
            let output = render_review_frame(&ctx, &mut dlg, &mut doc, vec![], 0.6);
            assert!(matches!(&dlg.stage, Stage::Review { error: None, .. }), "stale error after {label}");
            assert!(!output.shapes.iter().any(|shape| shape_has_text(&shape.shape, REVIEW_ERROR)));
            if label == "✖" {
                assert!(doc.marks.is_empty());
            }
        }
    }

    #[test]
    fn refuses_protected_jobs() {
        let mut doc = fixture(1);
        assert_eq!(
            build_items(&doc, &[2, 3], SizeMode::Allocated)
                .unwrap()
                .len(),
            2
        );
        assert!(build_items(&doc, &[2, Tree::ROOT], SizeMode::Allocated).is_err());
        assert!(build_items(&doc, &[2, u32::MAX], SizeMode::Allocated).is_err());
        Arc::get_mut(&mut doc.tree).unwrap().nodes[1].flags |= darak_core::flags::LOSSY_NAME;
        assert!(build_items(&doc, &[2, 3], SizeMode::Allocated).is_err());
        Arc::get_mut(&mut doc.tree).unwrap().nodes[1].flags &= !darak_core::flags::LOSSY_NAME;
        Arc::get_mut(&mut doc.tree).unwrap().nodes[3].flags |= darak_core::flags::REMOVED;
        assert!(build_items(&doc, &[2, 3], SizeMode::Allocated).is_err());
    }
}
