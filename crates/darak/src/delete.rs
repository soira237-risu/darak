//! 삭제 검토 창과 백그라운드 삭제 작업

use std::cmp::Reverse;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use darak_core::remove::{
    RecycleBin, RecycleBlock, delete_permanently, owner_window, recycle, recycle_bin_for, recycle_block, still_there,
    top_level_marks,
};
use darak_core::scan::disk_space;
use darak_core::{Kind, NodeId, SizeMode, Tree, flags, human_size, thousands};
use eframe::egui::{self, Color32, RichText};

use crate::doc::Doc;
use crate::theme;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    Recycle,
    Permanent,
}

/// 실행할 항목 하나. 실행 직전에 문서에서 뽑아 작업자 스레드로 넘긴다.
#[derive(Clone)]
pub struct Item {
    pub node: NodeId,
    pub path: String,
    pub is_dir: bool,
    /// 스캔에서 reparse 속성이 있던 파일 (실행 직전 재확인에서 링크로 바뀐 것으로 보지 않는다)
    pub reparse: bool,
    /// 할당 크기. '확보'는 크기 기준과 상관없이 실제로 비는 공간이다
    pub size: u64,
}

pub struct Outcome {
    pub node: NodeId,
    pub path: String,
    pub size: u64,
    pub error: Option<String>,
    /// 폴더를 지우다 실패해 일부만 지워졌을 수 있다 (크기를 믿기 어렵다)
    pub partial: bool,
}

/// 작업자 스레드가 마치며 보내는 결과
pub struct Finished {
    pub outcomes: Vec<Outcome>,
    /// 실행 전후의 여유 공간 (바이트, 읽지 못하면 None)
    pub free: Option<(u64, u64)>,
}

pub enum Stage {
    /// bin은 검토 창을 열 때 확인한 문서 볼륨의 휴지통 상태
    Review { method: Method, confirmed: bool, error: Option<String>, bin: RecycleBin },
    /// items는 실행 중인 항목의 사본이다 (현재 경로 표시, 작업자가 결과 없이 끝났을 때의 보고)
    Running {
        method: Method,
        items: Vec<Item>,
        done: Arc<AtomicUsize>,
        cancel: Arc<AtomicBool>,
        started: Instant,
        rx: mpsc::Receiver<Finished>,
    },
    Report { method: Method, finished: Finished },
}

pub struct Dialog {
    pub doc_id: u64,
    pub stage: Stage,
}

impl Dialog {
    /// 검토 창. 항상 휴지통 방식·확인 전 상태로 열고, 문서 볼륨의 휴지통 상태를 이때 한 번 확인한다.
    pub fn review(doc: &Doc) -> Self {
        let bin = recycle_bin_for(doc.tree.name(Tree::ROOT));
        Self { doc_id: doc.id, stage: Stage::Review { method: Method::Recycle, confirmed: false, error: None, bin } }
    }
}

/// 이번 프레임에 검토 창이 앱에 맡기는 일
#[derive(Default)]
pub struct Effects {
    /// 지운 노드 (트리에서 뺀다)
    pub removed: Vec<NodeId>,
    /// 일부만 지워졌을 수 있는 폴더 (Doc::stale)
    pub stale: Vec<NodeId>,
    /// 휴지통 열기 (셸 동작은 앱이 한곳에서 실행한다)
    pub open_bin: bool,
}

const CANCELLED: &str = "취소해서 실행하지 않았습니다";
const ABORTED: &str = "삭제 작업이 결과 없이 끝났습니다. 일부가 지워졌을 수 있으니 다시 스캔하세요";
const PARTIAL: &str = "일부가 이미 지워졌거나 휴지통으로 옮겨졌을 수 있습니다. 크기가 실제와 다를 수 있으니 다시 스캔하세요.";

/// 창이 닫혀야 하면 false. 앱이 할 일(지운 노드, 일부만 지워진 폴더, 휴지통 열기)은 `fx`에 넣는다.
pub fn show(ctx: &egui::Context, dlg: &mut Dialog, mut doc: Option<&mut Doc>, mode: SizeMode, fx: &mut Effects) -> bool {
    if matches!(dlg.stage, Stage::Review { .. }) && doc.as_ref().is_none_or(|doc| doc.id != dlg.doc_id) {
        return false;
    }
    let mut open = true;
    let mut next: Option<Stage> = None;
    let modal = egui::Modal::new(egui::Id::new("delete-review")).show(ctx, |ui| {
        ui.set_width(640.0);
        let t = theme::of(ui);
        match &mut dlg.stage {
            Stage::Review { method, confirmed, error, bin } => {
                let doc = doc.as_mut().expect("review document identity checked");
                let items = review_items(doc, mode);
                ui.heading("삭제 검토");
                if items.is_empty() {
                    ui.label(RichText::new("검토할 항목이 없습니다.").weak());
                    ui.add_space(6.0);
                    if ui.button("닫기").clicked() {
                        open = false;
                    }
                    return;
                }
                let total: u64 = items.iter().map(|&i| doc.tree.node(i).size(mode)).sum();
                let files: u64 = items.iter().map(|&i| doc.tree.node(i).files as u64).sum();
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
                            // 배지는 경로 앞에 둔다 (긴 경로에 밀려 창 밖으로 나가지 않게, 경로는 줄여 보이고 호버하면 전체)
                            if let Some(r) = doc.protected(id) {
                                ui.colored_label(t.warn, format!("⚠ {r}"));
                            }
                            // 볼륨 전체에 휴지통이 없으면 행마다 반복하지 않고 라디오 위에 한 번 알린다
                            if let Some(b @ (RecycleBlock::LongPath | RecycleBlock::TooLarge { .. })) =
                                recycle_block(&doc.tree, id, *bin)
                            {
                                ui.colored_label(t.warn, "⚠ 휴지통 불가").on_hover_text(b.reason());
                            }
                            let icon = if n.kind == Kind::Dir { "📁" } else { "📄" };
                            ui.add(egui::Label::new(format!("{icon} {}", doc.tree.path(id))).truncate());
                        });
                    }
                });
                if let Some(u) = unmark
                    && doc.toggle_mark(u).is_ok() {
                    *error = None;
                }
                ui.separator();
                if let RecycleBin::Unavailable(r) = *bin {
                    ui.colored_label(t.warn, format!("⚠ {r}. 영구 삭제만 가능합니다."));
                }
                let recycle = ui
                    .add_enabled_ui(matches!(bin, RecycleBin::Available { .. }), |ui| {
                        ui.radio_value(method, Method::Recycle, "휴지통으로 이동 (되돌릴 수 있음)")
                    })
                    .inner;
                let recycle = match *bin {
                    RecycleBin::Unavailable(r) => recycle.on_disabled_hover_text(r),
                    RecycleBin::Available { capacity: Some(c) } => {
                        recycle.on_hover_text(format!("이 드라이브의 휴지통 최대 크기 {}", human_size(c)))
                    }
                    RecycleBin::Available { capacity: None } => recycle,
                };
                let permanent = ui.radio_value(method, Method::Permanent, "영구 삭제 (되돌릴 수 없음)");
                // 방법을 바꾸면 영구 삭제 확인도 처음부터 다시 받는다
                if recycle.changed() || permanent.changed() {
                    *error = None;
                    *confirmed = false;
                }
                if *method == Method::Permanent
                    && ui.checkbox(confirmed, RichText::new("되돌릴 수 없다는 것을 이해했습니다").color(t.danger_text)).changed() {
                    *error = None;
                }
                ui.add_space(6.0);
                let blocker = run_blocker(doc, &items, *method, *confirmed, *bin);
                ui.horizontal(|ui| {
                    let label = match method {
                        Method::Recycle => format!("🗑 휴지통으로 이동 ({})", human_size(total)),
                        Method::Permanent => format!("⚠ 영구 삭제 ({})", human_size(total)),
                    };
                    let btn = egui::Button::new(RichText::new(label).strong().color(Color32::WHITE)).fill(t.danger_fill);
                    if ui.add_enabled(blocker.is_ok(), btn).clicked() {
                        let root = doc.tree.name(Tree::ROOT).to_owned();
                        match build_items(doc, &items, *method, *bin).and_then(|jobs| start(jobs, *method, owner_window(), root)) {
                            Ok(stage) => next = Some(stage),
                            Err(message) => *error = Some(message),
                        }
                    }
                    if ui.button("취소").clicked() {
                        open = false;
                    }
                });
                if let Err(Some(why)) = &blocker {
                    ui.label(RichText::new(why).weak());
                }
                if let Some(message) = error.as_deref() {
                    ui.colored_label(t.warn, message);
                }
            }
            Stage::Running { method, items, done, cancel, started, rx } => {
                ui.heading(match method {
                    Method::Recycle => "휴지통으로 옮기는 중…",
                    Method::Permanent => "영구 삭제 중…",
                });
                let (d, total) = (done.load(Relaxed), items.len());
                ui.add(egui::ProgressBar::new(d as f32 / total.max(1) as f32).text(format!("{d} / {total}")));
                if let Some(it) = items.get(d.min(total.saturating_sub(1))) {
                    ui.add(egui::Label::new(RichText::new(&it.path).monospace().weak()).truncate());
                }
                ui.label(RichText::new(format!("경과 {}초", started.elapsed().as_secs())).weak());
                // 지금 항목 뒤에 남은 항목이 있을 때만
                if d + 1 < total {
                    if cancel.load(Relaxed) {
                        ui.label(RichText::new("지금 항목이 끝나면 멈춥니다").weak());
                    } else if ui.button("남은 항목 취소").clicked() {
                        cancel.store(true, Relaxed);
                    }
                }
                ui.ctx().request_repaint_after(Duration::from_millis(100));
                match rx.try_recv() {
                    Ok(finished) => next = Some(report(*method, finished, fx)),
                    // 작업자가 결과 없이 끝났다. 어디까지 지웠는지 모르므로 모두 일부만 지워졌을 수 있다고 알린다
                    Err(mpsc::TryRecvError::Disconnected) => {
                        let outcomes = std::mem::take(items)
                            .into_iter()
                            .map(|it| Outcome { node: it.node, path: it.path, size: it.size, error: Some(ABORTED.into()), partial: true })
                            .collect();
                        next = Some(report(*method, Finished { outcomes, free: None }, fx));
                    }
                    Err(mpsc::TryRecvError::Empty) => {}
                }
            }
            Stage::Report { method, finished } => {
                let (ok, failed): (Vec<&Outcome>, Vec<&Outcome>) = finished.outcomes.iter().partition(|o| o.error.is_none());
                let size: u64 = ok.iter().map(|o| o.size).sum();
                ui.heading("삭제 결과");
                if !ok.is_empty() {
                    match method {
                        Method::Recycle => {
                            ui.colored_label(t.ok, format!("✔ 휴지통으로 옮김 {}개 · {}", ok.len(), human_size(size)));
                            ui.horizontal(|ui| {
                                ui.label(RichText::new("휴지통을 비우면 공간이 확보됩니다.").weak());
                                if ui.small_button("휴지통 열기").clicked() {
                                    fx.open_bin = true;
                                }
                            });
                        }
                        Method::Permanent => {
                            ui.colored_label(t.ok, format!("✔ 영구 삭제 {}개 · {} 확보", ok.len(), human_size(size)));
                            // 하드링크처럼 트리 합계와 실제가 다를 때도 실제로 빈 공간을 보여 준다
                            if let Some((before, after)) = finished.free {
                                ui.label(RichText::new(format!("여유 공간 {} → {}", human_size(before), human_size(after))).weak());
                            }
                        }
                    }
                    let verb = if *method == Method::Recycle { "옮긴" } else { "지운" };
                    egui::CollapsingHeader::new(format!("{verb} 항목 {}개", ok.len())).default_open(false).show(ui, |ui| {
                        egui::ScrollArea::vertical().id_salt("done").max_height(160.0).show(ui, |ui| {
                            for o in &ok {
                                ui.horizontal(|ui| {
                                    ui.add_sized([80.0, 18.0], egui::Label::new(RichText::new(human_size(o.size)).monospace()));
                                    ui.add(egui::Label::new(&o.path).truncate());
                                });
                            }
                        });
                    });
                }
                if !failed.is_empty() {
                    ui.colored_label(t.warn, format!("처리하지 못한 항목 {}개", failed.len()));
                    egui::ScrollArea::vertical().id_salt("failed").max_height(240.0).show(ui, |ui| {
                        for o in &failed {
                            ui.label(RichText::new(&o.path).strong());
                            ui.label(RichText::new(o.error.as_deref().unwrap_or("")).weak());
                            if o.partial {
                                ui.label(RichText::new(PARTIAL).small().color(t.warn));
                            }
                        }
                    });
                    ui.label(RichText::new("처리하지 못한 항목은 표시가 남아 있어 다시 검토할 수 있습니다.").weak());
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

/// 결과를 앱에 넘기고 결과 화면으로 간다. 표시는 지운 항목에서만 빠지므로 실패한 항목은 다시 검토할 수 있다.
fn report(method: Method, finished: Finished, fx: &mut Effects) -> Stage {
    fx.removed.extend(finished.outcomes.iter().filter(|o| o.error.is_none()).map(|o| o.node));
    fx.stale.extend(finished.outcomes.iter().filter(|o| o.partial).map(|o| o.node));
    Stage::Report { method, finished }
}

/// 검토 목록: 다른 표시 안에 든 표시를 뺀 최상위 표시를 큰 것부터
fn review_items(doc: &Doc, mode: SizeMode) -> Vec<NodeId> {
    let mut items = top_level_marks(&doc.tree, &doc.marks.iter().copied().collect::<Vec<_>>());
    items.sort_by_key(|&i| (Reverse(doc.tree.node(i).size(mode)), i));
    items
}

/// 실행 버튼을 막는 까닭. 버튼 아래에 보여 줄 문장이 있으면 Err(Some), 바로 위에서 이미 보이는 까닭
/// (볼륨에 휴지통이 없음, 영구 삭제 확인 전)이면 Err(None).
fn run_blocker(doc: &Doc, items: &[NodeId], method: Method, confirmed: bool, bin: RecycleBin) -> Result<(), Option<String>> {
    if items.is_empty() {
        return Err(None);
    }
    if items.iter().any(|&i| doc.protected(i).is_some()) {
        return Err(Some("보호된 항목이 있어 실행할 수 없습니다. ✖로 목록에서 빼세요".into()));
    }
    match method {
        Method::Recycle if matches!(bin, RecycleBin::Unavailable(_)) => Err(None),
        Method::Recycle => match items.iter().filter(|&&i| recycle_block(&doc.tree, i, bin).is_some()).count() {
            0 => Ok(()),
            n => Err(Some(format!("휴지통으로 보낼 수 없는 항목이 {n}개 있습니다. ✖로 목록에서 빼거나 영구 삭제를 고르세요"))),
        },
        Method::Permanent if !confirmed => Err(None),
        Method::Permanent => Ok(()),
    }
}

/// 실제 파일 작업 직전에 목록 전체를 다시 검사한다: 보호 정책, 그리고 휴지통 방식이면 휴지통으로 보낼 수 있는지
/// (검토 창이 버튼을 막더라도 한 번 더 막는다).
fn build_items(doc: &Doc, ids: &[NodeId], method: Method, bin: RecycleBin) -> Result<Vec<Item>, String> {
    ids.iter()
        .map(|&id| {
            if id as usize >= doc.tree.len() {
                return Err("삭제할 항목이 현재 문서에 없습니다".into());
            }
            if doc.tree.node(id).has(flags::REMOVED) {
                return Err("이미 삭제된 항목입니다".into());
            }
            if let Some(reason) = doc.protected(id) {
                return Err(format!("보호된 항목: {reason}"));
            }
            let path = doc.tree.path(id);
            if method == Method::Recycle
                && let Some(block) = recycle_block(&doc.tree, id, bin)
            {
                return Err(format!("휴지통으로 보낼 수 없는 항목: {path} ({})", block.reason()));
            }
            let n = doc.tree.node(id);
            Ok(Item { node: id, path, is_dir: n.kind == Kind::Dir, reparse: n.has(flags::REPARSE), size: n.alloc })
        })
        .collect()
}

/// 작업자 스레드에서 항목을 차례로 지운다. owner는 Windows의 영구 삭제 경고를 띄울 창(UI 스레드의 owner_window)이고,
/// root는 실행 전후 여유 공간을 잴 문서 루트다.
fn start(items: Vec<Item>, method: Method, owner: isize, root: String) -> Result<Stage, String> {
    let done = Arc::new(AtomicUsize::new(0));
    let cancel = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel();
    let (jobs, d, c) = (items.clone(), done.clone(), cancel.clone());
    std::thread::Builder::new()
        .name("delete".into())
        .spawn(move || {
            let free = || disk_space(&root).map(|(_, free)| free);
            let before = free();
            let outcomes = run_items(
                jobs,
                &c,
                &d,
                |it| still_there(&it.path, it.is_dir, it.reparse),
                |it| match method {
                    Method::Recycle => recycle(&it.path, owner),
                    Method::Permanent => delete_permanently(&it.path, it.is_dir),
                },
            );
            let _ = tx.send(Finished { outcomes, free: before.zip(free()) });
        })
        .map_err(|e| format!("삭제 작업을 시작하지 못했습니다: {e}"))?;
    Ok(Stage::Running { method, items, done, cancel, started: Instant::now(), rx })
}

/// 항목을 차례로 처리한다. check는 실행 직전 재확인, remove는 실제 삭제다 (테스트는 파일을 건드리지 않는 것을 넘긴다).
/// 취소하면 지금 항목을 마친 뒤 남은 항목은 실행하지 않는다.
fn run_items(
    items: Vec<Item>,
    cancel: &AtomicBool,
    done: &AtomicUsize,
    check: impl Fn(&Item) -> io::Result<()>,
    remove: impl Fn(&Item) -> io::Result<()>,
) -> Vec<Outcome> {
    items
        .into_iter()
        .map(|it| {
            let (error, partial) = if cancel.load(Relaxed) {
                (Some(CANCELLED.to_owned()), false)
            } else {
                match check(&it).map(|()| remove(&it)) {
                    Err(e) => (Some(e.to_string()), false),
                    Ok(Ok(())) => (None, false),
                    // 폴더는 셸도 remove_dir_all도 중간에 멈출 수 있으므로, 취소·없음이 아니면 일부 지워졌다고 본다
                    Ok(Err(e)) => {
                        let partial = it.is_dir && !matches!(e.kind(), io::ErrorKind::Interrupted | io::ErrorKind::NotFound);
                        (Some(e.to_string()), partial)
                    }
                }
            };
            done.fetch_add(1, Relaxed);
            Outcome { node: it.node, path: it.path, size: it.size, error, partial }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::tests::{doc, fixture, node};
    use darak_core::NO_NODE;

    const REVIEW_ERROR: &str = "보호된 항목: 테스트";
    const AVAILABLE: RecycleBin = RecycleBin::Available { capacity: None };
    const NO_BIN: &str = "네트워크 드라이브에는 휴지통이 없습니다";

    fn review_dialog(method: Method) -> Dialog {
        review_with(method, false, AVAILABLE)
    }

    fn review_with(method: Method, confirmed: bool, bin: RecycleBin) -> Dialog {
        Dialog { doc_id: 1, stage: Stage::Review { method, confirmed, error: Some(REVIEW_ERROR.into()), bin } }
    }

    /// 검토 창 한 프레임. 창이 열려 있는지와 앱에 맡긴 일을 함께 돌려준다
    fn render(
        ctx: &egui::Context,
        dlg: &mut Dialog,
        doc: &mut Doc,
        events: Vec<egui::Event>,
        time: f64,
    ) -> (egui::FullOutput, bool, Effects) {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1000.0, 800.0),
            )),
            events,
            time: Some(time),
            ..Default::default()
        };
        let (mut fx, mut open) = (Effects::default(), false);
        let mut output = ctx.run_ui(input, |ui| {
            open = show(ui.ctx(), dlg, Some(doc), SizeMode::Allocated, &mut fx);
        });
        output.textures_delta.clear();
        (output, open, fx)
    }

    fn render_review_frame(
        ctx: &egui::Context,
        dlg: &mut Dialog,
        doc: &mut Doc,
        events: Vec<egui::Event>,
        time: f64,
    ) -> egui::FullOutput {
        let (output, open, fx) = render(ctx, dlg, doc, events, time);
        assert!(open);
        assert!(fx.removed.is_empty() && fx.stale.is_empty() && !fx.open_bin);
        output
    }

    fn shape_has_text(shape: &egui::epaint::Shape, text: &str) -> bool {
        match shape {
            egui::epaint::Shape::Text(shape) => shape.galley.text().contains(text),
            egui::epaint::Shape::Vec(shapes) => shapes.iter().any(|shape| shape_has_text(shape, text)),
            _ => false,
        }
    }

    fn has_text(output: &egui::FullOutput, text: &str) -> bool {
        output.shapes.iter().any(|shape| shape_has_text(&shape.shape, text))
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

    /// 시각 t0부터 몇 프레임 그린 뒤 `label`을 눌렀다 뗀다.
    /// 뗀 다음 프레임의 화면과, 뗀 프레임(클릭이 처리된 프레임)의 반환값·앱에 맡긴 일을 돌려준다
    fn click_label(
        ctx: &egui::Context,
        dlg: &mut Dialog,
        doc: &mut Doc,
        label: &str,
        t0: f64,
    ) -> (egui::FullOutput, bool, Effects) {
        let mut output = render(ctx, dlg, doc, vec![], t0).0;
        for frame in 1..4 {
            output = render(ctx, dlg, doc, vec![], t0 + frame as f64 * 0.1).0;
        }
        let pos = output.shapes.iter().find_map(|shape| text_center(&shape.shape, label))
            .unwrap_or_else(|| panic!("review control missing: {label}"));
        let mut clicked = None;
        for (time, pressed) in [(0.4, true), (0.5, false)] {
            let (_, open, fx) = render(ctx, dlg, doc, vec![
                egui::Event::PointerMoved(pos),
                egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::default(),
                },
            ], t0 + time);
            clicked = Some((open, fx));
        }
        let (open, fx) = clicked.unwrap();
        (render(ctx, dlg, doc, vec![], t0 + 0.6).0, open, fx)
    }

    /// 루트 바로 아래에 이름 길이가 `len`인 파일 하나 (경로 = `C:\Data\` + 이름)
    fn long_named(len: usize) -> Doc {
        let mut tree = Tree::default();
        node(&mut tree, "C:\\Data", NO_NODE, Kind::Dir, 100, 1, 1);
        node(&mut tree, &"a".repeat(len), 0, Kind::File, 100, 0, 0);
        doc(1, tree)
    }

    fn item(node: NodeId, path: &str, is_dir: bool) -> Item {
        Item { node, path: path.into(), is_dir, reparse: false, size: 10 }
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
                assert!(has_text(&output, REVIEW_ERROR));
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
            doc.toggle_mark(2).unwrap();
            let mut dlg = review_dialog(method);
            let (output, open, fx) = click_label(&ctx, &mut dlg, &mut doc, label, 0.0);
            assert!(open && fx.removed.is_empty());
            assert!(matches!(&dlg.stage, Stage::Review { error: None, .. }), "stale error after {label}");
            assert!(!has_text(&output, REVIEW_ERROR));
            if label == "✖" {
                // 큰 것부터 보이므로 첫 ✖는 60 B짜리 3이다
                assert_eq!(doc.marks.iter().copied().collect::<Vec<_>>(), [2]);
            }
        }
    }

    #[test]
    fn changing_method_clears_permanent_confirmation() {
        let ctx = egui::Context::default();
        let mut doc = fixture(1);
        doc.toggle_mark(3).unwrap();
        let mut dlg = review_with(Method::Permanent, true, AVAILABLE);
        click_label(&ctx, &mut dlg, &mut doc, "휴지통으로 이동 (되돌릴 수 있음)", 0.0);
        assert!(matches!(dlg.stage, Stage::Review { method: Method::Recycle, confirmed: false, .. }));
        // 영구 삭제로 돌아와도 확인은 다시 받는다
        click_label(&ctx, &mut dlg, &mut doc, "영구 삭제 (되돌릴 수 없음)", 1.0);
        assert!(matches!(dlg.stage, Stage::Review { method: Method::Permanent, confirmed: false, .. }));
    }

    #[test]
    fn volume_without_bin_disables_recycle() {
        let ctx = egui::Context::default();
        let mut doc = fixture(1);
        doc.toggle_mark(3).unwrap();
        let mut dlg = review_with(Method::Permanent, false, RecycleBin::Unavailable(NO_BIN));
        let (output, ..) = click_label(&ctx, &mut dlg, &mut doc, "휴지통으로 이동 (되돌릴 수 있음)", 0.0);
        assert!(has_text(&output, &format!("⚠ {NO_BIN}. 영구 삭제만 가능합니다.")));
        assert!(matches!(dlg.stage, Stage::Review { method: Method::Permanent, .. }));
        // 행마다 반복하지 않는다
        assert!(!has_text(&output, "휴지통 불가"));
    }

    #[test]
    fn empty_review_shows_only_close() {
        let ctx = egui::Context::default();
        let mut doc = fixture(1);
        let mut dlg = review_dialog(Method::Recycle);
        render(&ctx, &mut dlg, &mut doc, vec![], 0.0);
        let (output, open, _) = render(&ctx, &mut dlg, &mut doc, vec![], 0.1);
        assert!(open);
        assert!(has_text(&output, "검토할 항목이 없습니다."));
        assert!(!has_text(&output, "휴지통으로 이동"));
        let (_, open, _) = click_label(&ctx, &mut dlg, &mut doc, "닫기", 1.0);
        assert!(!open);
    }

    #[test]
    fn long_path_items_are_badged_and_block_recycle() {
        let ctx = egui::Context::default();
        let mut doc = long_named(252);
        doc.toggle_mark(1).unwrap();
        let mut dlg = review_dialog(Method::Recycle);
        let mut output = render_review_frame(&ctx, &mut dlg, &mut doc, vec![], 0.0);
        for frame in 1..3 {
            output = render_review_frame(&ctx, &mut dlg, &mut doc, vec![], frame as f64 * 0.1);
        }
        assert!(has_text(&output, "⚠ 휴지통 불가"));
        assert!(has_text(&output, "휴지통으로 보낼 수 없는 항목이 1개 있습니다"));
    }

    #[test]
    fn review_lists_largest_first() {
        let mut doc = fixture(1);
        doc.toggle_mark(2).unwrap();
        doc.toggle_mark(3).unwrap();
        assert_eq!(review_items(&doc, SizeMode::Allocated), [3, 2]);
        // 크기가 같으면 노드 순서
        Arc::get_mut(&mut doc.tree).unwrap().nodes[3].alloc = 40;
        assert_eq!(review_items(&doc, SizeMode::Allocated), [2, 3]);
    }

    #[test]
    fn run_blocker_explains_what_stops_the_run() {
        let doc = fixture(1);
        let no_bin = RecycleBin::Unavailable(NO_BIN);
        assert_eq!(run_blocker(&doc, &[], Method::Recycle, false, AVAILABLE), Err(None));
        let protected = run_blocker(&doc, &[2, Tree::ROOT], Method::Permanent, true, AVAILABLE);
        assert!(matches!(protected, Err(Some(m)) if m.contains("보호된 항목")));
        assert_eq!(run_blocker(&doc, &[2, 3], Method::Recycle, false, AVAILABLE), Ok(()));
        // 볼륨 안내는 라디오 위에 이미 있다
        assert_eq!(run_blocker(&doc, &[2], Method::Recycle, false, no_bin), Err(None));
        assert_eq!(run_blocker(&doc, &[2], Method::Permanent, false, no_bin), Err(None));
        assert_eq!(run_blocker(&doc, &[2], Method::Permanent, true, no_bin), Ok(()));
        // 휴지통 최대 크기(50 B)보다 큰 3만 막힌다
        let small = RecycleBin::Available { capacity: Some(50) };
        let blocked = run_blocker(&doc, &[2, 3], Method::Recycle, false, small);
        assert!(matches!(blocked, Err(Some(m)) if m.starts_with("휴지통으로 보낼 수 없는 항목이 1개")));
        assert_eq!(run_blocker(&doc, &[2, 3], Method::Permanent, true, small), Ok(()));
        // 경로 259자는 되고 260자는 막힌다 (`C:\Data\` 8자 + 이름)
        assert_eq!(run_blocker(&long_named(251), &[1], Method::Recycle, false, AVAILABLE), Ok(()));
        assert!(run_blocker(&long_named(252), &[1], Method::Recycle, false, AVAILABLE).is_err());
    }

    #[test]
    fn refuses_protected_jobs() {
        let mut doc = fixture(1);
        let jobs = build_items(&doc, &[2, 1], Method::Recycle, AVAILABLE).unwrap();
        assert_eq!(jobs.len(), 2);
        assert!(!jobs[0].is_dir && jobs[1].is_dir);
        assert!(build_items(&doc, &[2, Tree::ROOT], Method::Recycle, AVAILABLE).is_err());
        assert!(build_items(&doc, &[2, u32::MAX], Method::Recycle, AVAILABLE).is_err());
        Arc::get_mut(&mut doc.tree).unwrap().nodes[1].flags |= flags::LOSSY_NAME;
        assert!(build_items(&doc, &[2, 3], Method::Recycle, AVAILABLE).is_err());
        Arc::get_mut(&mut doc.tree).unwrap().nodes[1].flags &= !flags::LOSSY_NAME;
        Arc::get_mut(&mut doc.tree).unwrap().nodes[3].flags |= flags::REMOVED;
        assert!(build_items(&doc, &[2, 3], Method::Recycle, AVAILABLE).is_err());
    }

    #[test]
    fn build_items_rechecks_recycle_blocks() {
        let mut doc = fixture(1);
        let small = RecycleBin::Available { capacity: Some(50) };
        let error = build_items(&doc, &[2, 3], Method::Recycle, small).err().unwrap();
        assert_eq!(error, "휴지통으로 보낼 수 없는 항목: C:\\Data\\folder\\child (휴지통 최대 크기(50 B)보다 큽니다)");
        assert!(build_items(&doc, &[2], Method::Recycle, RecycleBin::Unavailable(NO_BIN)).is_err());
        // 영구 삭제는 휴지통과 상관없다. 크기는 크기 기준과 상관없이 할당 크기, reparse 파일 표시는 그대로 넘긴다
        let tree = Arc::get_mut(&mut doc.tree).unwrap();
        tree.nodes[3].logical = 7;
        tree.nodes[3].flags |= flags::REPARSE;
        let jobs = build_items(&doc, &[3], Method::Permanent, RecycleBin::Unavailable(NO_BIN)).unwrap();
        assert_eq!((jobs[0].size, jobs[0].reparse), (60, true));
    }

    #[test]
    fn run_items_cancels_rest_and_flags_partial_folders() {
        let (cancel, done) = (AtomicBool::new(false), AtomicUsize::new(0));
        let items = vec![
            item(1, "gone", false),
            item(2, "busy-dir", true),
            item(3, "declined-dir", true),
            item(4, "busy-file", false),
            item(5, "ok", false),
            item(6, "after-cancel", false),
        ];
        let outcomes = run_items(
            items,
            &cancel,
            &done,
            |it| match it.path.as_str() {
                "gone" => Err(io::Error::new(io::ErrorKind::NotFound, "이미 없는 항목입니다")),
                _ => Ok(()),
            },
            |it| match it.path.as_str() {
                "busy-dir" | "busy-file" => Err(io::Error::other("다른 프로그램이 사용 중인 파일이 있습니다")),
                "declined-dir" => Err(io::Error::new(io::ErrorKind::Interrupted, "취소")),
                _ => {
                    // 이 항목을 마치는 동안 '남은 항목 취소'를 누른 셈
                    cancel.store(true, Relaxed);
                    Ok(())
                }
            },
        );
        let summary: Vec<(&str, Option<&str>, bool)> =
            outcomes.iter().map(|o| (o.path.as_str(), o.error.as_deref(), o.partial)).collect();
        assert_eq!(summary, [
            ("gone", Some("이미 없는 항목입니다"), false),
            ("busy-dir", Some("다른 프로그램이 사용 중인 파일이 있습니다"), true),
            ("declined-dir", Some("취소"), false),
            ("busy-file", Some("다른 프로그램이 사용 중인 파일이 있습니다"), false),
            ("ok", None, false),
            ("after-cancel", Some(CANCELLED), false),
        ]);
        assert_eq!(done.load(Relaxed), 6);
    }

    fn running(method: Method, items: Vec<Item>) -> (Dialog, mpsc::Sender<Finished>, Arc<AtomicBool>) {
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let stage = Stage::Running { method, items, done: Arc::default(), cancel: cancel.clone(), started: Instant::now(), rx };
        (Dialog { doc_id: 1, stage }, tx, cancel)
    }

    fn outcome(node: NodeId, path: &str, error: Option<&str>, partial: bool) -> Outcome {
        Outcome { node, path: path.into(), size: 60, error: error.map(Into::into), partial }
    }

    #[test]
    fn running_cancels_remaining_items() {
        let ctx = egui::Context::default();
        let mut doc = fixture(1);
        let (mut dlg, _tx, cancel) = running(Method::Permanent, vec![item(1, "C:\\Data\\folder", true), item(2, "C:\\Data\\sibling", false)]);
        let (output, ..) = click_label(&ctx, &mut dlg, &mut doc, "남은 항목 취소", 0.0);
        assert!(cancel.load(Relaxed));
        assert!(has_text(&output, "지금 항목이 끝나면 멈춥니다"));
        assert!(has_text(&output, "C:\\Data\\folder"));
        assert!(matches!(dlg.stage, Stage::Running { .. }));
    }

    #[test]
    fn finished_run_reports_removed_and_partial_nodes() {
        let ctx = egui::Context::default();
        let mut doc = fixture(1);
        let (mut dlg, tx, _) = running(Method::Recycle, vec![]);
        tx.send(Finished {
            outcomes: vec![outcome(2, "C:\\Data\\sibling", None, false), outcome(1, "C:\\Data\\folder", Some("사용 중"), true)],
            free: None,
        })
        .unwrap();
        let (_, open, fx) = render(&ctx, &mut dlg, &mut doc, vec![], 0.0);
        assert!(open);
        assert_eq!((fx.removed, fx.stale), (vec![2], vec![1]));
        assert!(matches!(dlg.stage, Stage::Report { method: Method::Recycle, .. }));
        // 작업자가 결과 없이 끝나면 모든 항목을 일부만 지워졌을 수 있다고 알린다
        let (mut dlg, tx, _) = running(Method::Permanent, vec![item(1, "C:\\Data\\folder", true)]);
        drop(tx);
        let (_, _, fx) = render(&ctx, &mut dlg, &mut doc, vec![], 0.0);
        assert!(fx.removed.is_empty());
        assert_eq!(fx.stale, [1]);
        let Stage::Report { finished, .. } = &dlg.stage else { panic!("report missing") };
        assert_eq!(finished.outcomes[0].error.as_deref(), Some(ABORTED));
    }

    #[test]
    fn report_wording_follows_method() {
        for method in [Method::Recycle, Method::Permanent] {
            let ctx = egui::Context::default();
            let mut doc = fixture(1);
            let finished = Finished {
                outcomes: vec![outcome(2, "C:\\Data\\sibling", None, false), outcome(1, "C:\\Data\\folder", Some("사용 중"), true)],
                free: Some((1 << 20, 3 << 20)),
            };
            let mut dlg = Dialog { doc_id: 1, stage: Stage::Report { method, finished } };
            render(&ctx, &mut dlg, &mut doc, vec![], 0.0);
            let output = render(&ctx, &mut dlg, &mut doc, vec![], 0.1).0;
            let recycle = method == Method::Recycle;
            assert_eq!(has_text(&output, "✔ 휴지통으로 옮김 1개 · 60 B"), recycle);
            assert_eq!(has_text(&output, "휴지통을 비우면 공간이 확보됩니다."), recycle);
            // 휴지통으로 옮긴 공간은 비지 않았으므로 '확보'라고 하지 않는다
            assert_eq!(has_text(&output, "60 B 확보"), !recycle);
            assert_eq!(has_text(&output, "여유 공간 1.00 MB → 3.00 MB"), !recycle);
            assert!(has_text(&output, if recycle { "옮긴 항목 1개" } else { "지운 항목 1개" }));
            assert!(has_text(&output, "처리하지 못한 항목 1개"));
            assert!(has_text(&output, "일부가 이미 지워졌거나"));
            if recycle {
                let (_, open, fx) = click_label(&ctx, &mut dlg, &mut doc, "휴지통 열기", 1.0);
                assert!(open && fx.open_bin && fx.removed.is_empty());
            }
        }
    }
}
