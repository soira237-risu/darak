//! 삭제 검토 창과 백그라운드 삭제 작업

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::sync::mpsc;

use darak_core::remove::{delete_permanently, protected_reason, recycle, top_level_marks};
use darak_core::{Kind, NodeId, SizeMode, human_size, thousands};
use eframe::egui::{self, Color32, RichText};

use crate::doc::Doc;

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
    Review { method: Method, confirmed: bool },
    Running { total: usize, done: Arc<AtomicUsize>, rx: mpsc::Receiver<Vec<Outcome>> },
    Report { outcomes: Vec<Outcome> },
}

pub struct Dialog {
    pub stage: Stage,
}

/// 창이 닫혀야 하면 false. 삭제된 노드는 `removed`에 넣는다.
pub fn show(ctx: &egui::Context, dlg: &mut Dialog, doc: &mut Doc, mode: SizeMode, removed: &mut Vec<NodeId>) -> bool {
    let mut open = true;
    let mut next: Option<Stage> = None;
    let modal = egui::Modal::new(egui::Id::new("delete-review")).show(ctx, |ui| {
        ui.set_width(640.0);
        match &mut dlg.stage {
            Stage::Review { method, confirmed } => {
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
                                ui.colored_label(Color32::from_rgb(230, 120, 60), format!("⚠ {r}"));
                            }
                        });
                    }
                });
                if let Some(u) = unmark {
                    let _ = doc.toggle_mark(u);
                }
                ui.separator();
                ui.radio_value(method, Method::Recycle, "휴지통으로 이동 (되돌릴 수 있음)");
                ui.radio_value(method, Method::Permanent, "영구 삭제 (되돌릴 수 없음)");
                if *method == Method::Permanent {
                    ui.checkbox(confirmed, RichText::new("되돌릴 수 없다는 것을 이해했습니다").color(Color32::from_rgb(230, 90, 80)));
                }
                ui.add_space(6.0);
                let any_protected = items.iter().any(|&i| protected_reason(&doc.tree, i).is_some());
                let can_run = !items.is_empty() && !any_protected && (*method == Method::Recycle || *confirmed);
                ui.horizontal(|ui| {
                    let label = match method {
                        Method::Recycle => format!("🗑 휴지통으로 이동 ({})", human_size(total)),
                        Method::Permanent => format!("⚠ 영구 삭제 ({})", human_size(total)),
                    };
                    let btn = egui::Button::new(RichText::new(label).strong()).fill(Color32::from_rgb(170, 50, 45));
                    if ui.add_enabled(can_run, btn).clicked() {
                        let jobs: Vec<Item> = items
                            .iter()
                            .map(|&id| {
                                let n = doc.tree.node(id);
                                Item { node: id, path: doc.tree.path(id), is_dir: n.kind == Kind::Dir, size: n.size(mode) }
                            })
                            .collect();
                        next = Some(start(jobs, *method));
                    }
                    if ui.button("취소").clicked() {
                        open = false;
                    }
                });
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
                ui.label(format!("성공 {}개 · {} 확보", ok.len(), human_size(freed)));
                if !failed.is_empty() {
                    ui.colored_label(Color32::from_rgb(230, 90, 80), format!("실패 {}개", failed.len()));
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
