use std::io;
use std::os::windows::process::CommandExt;
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use darak_core::remove::protected_reason;
use darak_core::scan::{Drive, disk_space, is_elevated, list_drives, normalize_root, relaunch_as_admin, shell_open};
use darak_core::{
    Kind, NodeId, ScanMethod, ScanOptions, ScanProgress, ScanResult, SizeMode, Tree, flags, format_time, human_size,
    scan, thousands,
};
use eframe::egui::{self, Color32, Key, PointerButton, RichText};

use crate::delete::{self, Dialog, Method, Stage};
use crate::doc::{Doc, RemovalApply};
use crate::preview_worker::{PreviewController, PreviewPacket, PreviewRequest};
use crate::search_worker::{SearchController, SearchKey, SearchRequest, ReleaseFence, SearchControlEvent};
use darak_core::search::{Query, now_secs2000};
use crate::search_panel;
use crate::treemap::{self, Detail, Highlights, Item, ItemKind, Layout, LayoutSource, Morph, MorphKey, Style, ZoomAnim};

pub struct App {
    preview: PreviewController,
    next_scan_id: u64,
    search: SearchController,
    search_key: Option<SearchKey>,
    search_generation: u64,
    search_submitted: bool,
    search_failed: Option<String>,
    pending_removal: Option<PendingRemoval>,
    removal_released: bool,
    next_fence: u64,
    drives: Vec<Drive>,
    root_input: String,
    scanning: Option<Scanning>,
    doc: Option<Doc>,
    error: Option<String>,
    /// 잠깐 보여 줄 안내 (메시지, 표시 시각)
    toast: Option<(String, Instant)>,
    mode: SizeMode,
    layout: Layout,
    wheel_acc: f32,
    context_node: Option<NodeId>,
    next_doc_id: u64,
    elevated: bool,
    delete_dialog: Option<Dialog>,
    focus_search: bool,
    detail: Detail,
    anim: Option<ZoomAnim>,
    /// 애니메이션 판단용: 직전 프레임의 (문서 id, focus)
    last_focus: Option<(u64, NodeId)>,
    /// 미리 보기와 스캔 완료 직후의 모양 변화 보간
    morph: Morph,
    morph_items: Vec<Item>,
    morph_until: Option<Instant>,
    #[cfg(feature = "perf")]
    perf_trace: Option<crate::perf_trace::PerfTrace>,
    #[cfg(feature = "perf")]
    perf_fixture_id: Option<String>,
    #[cfg(feature = "perf")]
    perf_measurement: Option<crate::perf_trace::Measurement>,
    #[cfg(feature = "perf")]
    perf_highlight_draw: Option<crate::perf_trace::DrawIdentity>,
}

struct PendingRemoval {
    fence: ReleaseFence,
    ids: Vec<NodeId>,
    /// 작업자가 트리를 반환했다고 알린 시각
    released_at: Option<Instant>,
}

/// 반환 알림 뒤에도 트리를 다른 소유자가 붙잡고 있으면, 이 시간이 지난 뒤 복제해서 반영한다.
const REMOVAL_DETACH_AFTER: Duration = Duration::from_secs(2);

struct Scanning {
    scan_id: u64,
    root: String,
    progress: Arc<ScanProgress>,
    rx: mpsc::Receiver<io::Result<ScanResult>>,
    started: Instant,
    /// 드라이브 사용량 (진행률 표시용)
    used: Option<u64>,
    /// 부드럽게 만든 남은 시간 (초, 계산한 시각)
    eta: Option<(f64, Instant)>,
    /// 일반 순회 중 미리 보기
    preview: Option<Tree>,
    preview_version: u64,
    preview_layout: Layout,
    last_preview: Instant,
}

impl App {
    pub fn new(cc: &eframe::CreationContext, mut search: SearchController, mut preview: PreviewController,
        #[cfg(feature = "perf")] fixture: Option<(Doc, String)>,
        #[cfg(feature = "perf")] trace: Option<crate::perf_trace::PerfTrace>,
    ) -> Self {
        crate::fonts::install(&cc.egui_ctx);
        search.set_waker(cc.egui_ctx.clone());
        preview.set_waker(cc.egui_ctx.clone());
        #[cfg(feature = "perf")]
        if let Some(trace) = &trace { search.set_release_observer(trace.release_observer()); }
        let drives = list_drives();
        let root_input = drives.first().map(|d| d.root.clone()).unwrap_or_else(|| "C:\\".into());
        let mut app = Self {
            preview, next_scan_id: 1,
            search, search_key: None, search_generation: 0, search_submitted: false,
            search_failed: None, pending_removal: None, removal_released: false, next_fence: 0,
            drives,
            root_input,
            scanning: None,
            doc: None,
            error: None,
            toast: None,
            mode: SizeMode::Allocated,
            layout: Layout::default(),
            wheel_acc: 0.0,
            context_node: None,
            next_doc_id: 1,
            elevated: is_elevated(),
            delete_dialog: None,
            focus_search: false,
            detail: Detail::default(),
            anim: None,
            last_focus: None,
            morph: Morph::default(),
            morph_items: Vec::new(),
            morph_until: None,
            #[cfg(feature = "perf")] perf_trace: trace,
            #[cfg(feature = "perf")] perf_fixture_id: None,
            #[cfg(feature = "perf")] perf_measurement: None,
            #[cfg(feature = "perf")] perf_highlight_draw: None,
        };
        #[cfg(feature = "perf")]
        if let Some((doc, id)) = fixture {
            app.root_input = doc.tree.name(Tree::ROOT).into();
            app.next_doc_id = 2;
            app.doc = Some(doc);
            app.perf_fixture_id = Some(id);
            return app;
        }
        // `darak.exe D:\` 처럼 경로를 주면 바로 스캔
        if let Some(arg) = std::env::args().nth(1) {
            app.start_scan(arg);
        }
        app
    }

    fn start_scan(&mut self, root: String) {
        #[cfg(feature = "perf")]
        if self.perf_fixture_id.is_some() { return; }
        if self.delete_dialog.is_some() || self.pending_removal.is_some() {
            return;
        }
        let root = normalize_root(&root);
        let progress = Arc::new(ScanProgress::default());
        let (tx, rx) = mpsc::channel();
        {
            let progress = progress.clone();
            let root = root.clone();
            std::thread::Builder::new()
                .name("scan".into())
                .spawn(move || {
                    let res = scan(&root, &ScanOptions::default(), &progress);
                    let _ = tx.send(res);
                })
                .expect("스캔 스레드 생성 실패");
        }
        let is_drive_root = root.len() == 3 && root.ends_with(":\\");
        let used = is_drive_root.then(|| disk_space(&root)).flatten().map(|(t, f)| t - f);
        self.begin_scan(root, progress, rx, used);
    }

    fn begin_scan(&mut self, root: String, progress: Arc<ScanProgress>, rx: mpsc::Receiver<io::Result<ScanResult>>, used: Option<u64>) {
        self.cancel_scan();
        self.suspend_search();
        self.root_input = root.clone();
        self.morph.reset();
        self.morph_items.clear();
        self.morph_until = None;
        self.error = None;
        let scan_id = self.next_scan_id;
        self.next_scan_id = scan_id.checked_add(1).expect("scan ID exhausted");
        self.preview.activate(scan_id);
        self.scanning = Some(Scanning {
            scan_id,
            root,
            progress,
            rx,
            started: Instant::now(),
            used,
            eta: None,
            preview: None,
            preview_version: 0,
            preview_layout: Layout::stable(),
            last_preview: Instant::now(),
        });
    }

    fn cancel_scan(&mut self) {
        if let Some(scan) = &self.scanning {
            scan.progress.cancel();
            self.preview.invalidate(scan.scan_id);
        }
    }

    fn accept_preview(&mut self, packet: PreviewPacket) -> bool {
        let Some(scan) = &mut self.scanning else { return false };
        // Validate identity and lifecycle before accessing any packet tree node.
        if scan.scan_id != packet.scan_id || scan.progress.is_cancelled()
            || packet.version <= scan.preview_version {
            return false;
        }
        #[cfg(feature = "perf")]
        if let Some(trace) = &mut self.perf_trace {
            trace.record("preview", packet.scan_id, packet.version, "capture", packet.capture_time);
            trace.record("preview", packet.scan_id, packet.version, "build", packet.build_time);
        }
        scan.preview = Some(packet.tree);
        scan.preview_version = packet.version;
        true
    }

    fn poll_preview(&mut self) {
        if let Some(scan) = &self.scanning && scan.progress.is_cancelled() {
            self.preview.invalidate(scan.scan_id);
        }
        if let Some(packet) = self.preview.try_take_latest() {
            self.accept_preview(packet);
        }
        let Some(scan) = &mut self.scanning else { return };
        if !scan.progress.is_cancelled() && scan.last_preview.elapsed() >= Duration::from_millis(1200) {
            scan.last_preview = Instant::now();
            self.preview.request_latest(PreviewRequest {
                scan_id: scan.scan_id,
                progress: Arc::clone(&scan.progress),
                after_version: scan.preview_version,
            });
        }
    }

    fn open_delete_review(&mut self) {
        if self.doc.as_ref().is_some_and(|doc| !filesystem_actions_enabled(doc)) { return; }
        if self.scanning.is_some() || self.delete_dialog.is_some() || self.pending_removal.is_some() {
            return;
        }
        if let Some(doc) = &self.doc
            && !doc.marks.is_empty()
        {
            self.delete_dialog = Some(Dialog {
                doc_id: doc.id,
                stage: Stage::Review {
                    method: Method::Recycle,
                    confirmed: false,
                    error: None,
                },
            });
        }
    }

    fn poll_scan(&mut self) {
        let Some(s) = &self.scanning else { return };
        let result = s.rx.try_recv();
        let resumed_at = Instant::now();
        if !matches!(result, Err(mpsc::TryRecvError::Empty)) {
            self.preview.invalidate(s.scan_id);
            self.morph.invalidate();
        }
        match result {
            Ok(Ok(res)) => {
                self.suspend_search();
                self.context_node = None;
                self.anim = None;
                self.last_focus = None;
                self.doc = Some(Doc::new(self.next_doc_id, res.tree, res.stats));
                self.morph_until = Some(Instant::now() + Duration::from_millis(1500));
                self.next_doc_id += 1;
                self.scanning = None;
                self.layout.invalidate();
                self.drives = list_drives();
            }
            Ok(Err(e)) => {
                self.error = Some(format!("{}: {e}", s.root));
                self.scanning = None;
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => {
                self.error = Some("스캔 스레드가 비정상 종료했습니다".into());
                self.scanning = None;
            }
        }
        // Fresh resume event excludes suspension but includes subsequent UI work.
        if self.scanning.is_none() && let Some(doc) = &mut self.doc {
            doc.search.note_edit(resumed_at);
        }
    }

    fn toast(&mut self, msg: impl Into<String>) {
        self.toast = Some((msg.into(), Instant::now()));
    }

    fn toggle_mark(&mut self, id: NodeId) {
        if self.pending_removal.is_some() { return; }
        if let Some(doc) = &mut self.doc
            && let Err(e) = doc.toggle_mark(id) {
                self.toast = Some((e, Instant::now()));
            }
    }
}

impl App {
    fn suspend_search(&mut self) {
        #[cfg(feature = "perf")]
        self.perf_supersede();
        self.search.invalidate();
        self.search_submitted = false;
        self.search_key = None;
        if let Some(doc) = &mut self.doc {
            doc.clear_search();
            doc.search.pending_edit_at = None;
        }
    }

    fn observe_search_input(&mut self) {
        // A suspended query becomes a new request at resume, excluding scan time.
        if self.scanning.is_some() {
            return;
        }
        let observed_at = Instant::now();
        let edited_at = self.doc.as_mut()
            .and_then(|doc| doc.search.pending_edit_at.take())
            .unwrap_or(observed_at);
        let key = self.doc.as_ref().map(|doc| doc.search_key(self.mode));
        if self.search_key == key {
            return;
        }
        self.search.invalidate();
        self.search_generation = self.search_generation.wrapping_add(1);
        self.search_submitted = false;
        self.search_key = key;
        if let Some(doc) = &mut self.doc {
            doc.clear_search();
            doc.search.edited_at = Some(edited_at);
        }
        #[cfg(feature = "perf")]
        self.perf_observe_input();
    }

    fn update_search(&mut self) {
        self.search_pass(true);
    }

    fn search_pass(&mut self, accept_completion: bool) {
        // Invalidation must precede consuming completions in the same frame.
        self.observe_search_input();
        while let Some(event) = self.search.try_take_control() {
            self.search_control(event);
        }
        self.finish_removal();
        self.observe_search_input();
        if self.scanning.is_some() || self.pending_removal.is_some() {
            return;
        }
        if let Some(error) = &self.search_failed {
            if let Some(doc) = &mut self.doc {
                doc.search.error = Some(error.clone());
            }
            return;
        }
        if accept_completion
            && let Some(completion) = self.search.try_take_completion()
            && let (Some(doc), Some(key)) = (&mut self.doc, &self.search_key)
        {
            #[cfg(feature = "perf")]
            let timings = completion.timings;
            if doc.accept_search(completion, key, self.search_generation) {
                #[cfg(feature = "perf")]
                if let Some(trace) = &mut self.perf_trace {
                    for (phase, elapsed) in [
                        ("queued", timings.queued),
                        ("search", timings.search),
                        ("highlight", timings.highlight),
                    ] {
                        trace.record("search", doc.id, self.search_generation, phase, elapsed);
                    }
                }
            }
        }
        if self.search_submitted {
            return;
        }
        let (Some(doc), Some(key)) = (&mut self.doc, &self.search_key) else {
            return;
        };
        self.search_submitted = true;
        if key.query.trim().is_empty() {
            return;
        }
        let now = now_secs2000();
        #[cfg(feature = "perf")]
        let now = if self.perf_fixture_id.is_some() {
            darak_core::perf::FIXTURE_NOW_SECS2000
        } else {
            now
        };
        match Query::parse(&key.query, now) {
            Ok(query) if !query.is_empty() => self.search.submit_latest(SearchRequest {
                generation: self.search_generation,
                key: key.clone(),
                tree: Arc::clone(&doc.tree),
                query,
                edited_at: doc.search.edited_at.unwrap_or_else(Instant::now),
            }),
            Ok(_) => {}
            Err(error) => doc.search.error = Some(error),
        }
    }

    fn begin_removal(&mut self, doc_id: u64, ids: Vec<NodeId>) {
        if self.pending_removal.is_some() || ids.is_empty() {
            return;
        }
        let Some(doc) = &mut self.doc else { return };
        if doc.id != doc_id || ids.iter().any(|&id| id as usize >= doc.tree.len()) {
            return;
        }
        self.next_fence = self.next_fence.wrapping_add(1);
        let fence = ReleaseFence {
            token: self.next_fence,
            doc_id,
            tree_version: doc.tree.version,
        };
        doc.clear_search();
        self.search_submitted = false;
        self.removal_released = false;
        self.pending_removal = Some(PendingRemoval { fence, ids, released_at: None });
        self.search.release_for_mutation(fence);
    }

    fn search_control(&mut self, event: SearchControlEvent) {
        match event {
            SearchControlEvent::Released(fence) => {
                if let Some(pending) = &mut self.pending_removal
                    && pending.fence == fence
                {
                    self.removal_released = true;
                    pending.released_at.get_or_insert_with(Instant::now);
                }
            }
            SearchControlEvent::WorkerFailed(error) => {
                self.search_failed = Some(error);
                if let Some(doc) = &mut self.doc {
                    doc.clear_search();
                }
            }
        }
    }

    fn finish_removal(&mut self) {
        if !self.removal_released {
            return;
        }
        let Some(pending) = &self.pending_removal else {
            return;
        };
        let detach = pending.released_at.is_some_and(|at| at.elapsed() >= REMOVAL_DETACH_AFTER);
        let applied = match &mut self.doc {
            Some(doc) if doc.tree.version == pending.fence.tree_version => {
                let applied = doc.apply_removed_for(pending.fence.doc_id, &pending.ids);
                if applied == RemovalApply::Busy && detach {
                    // 파일은 이미 지워졌으므로 화면 반영을 무기한 미루지 않는다.
                    doc.detach_tree();
                    doc.apply_removed_for(pending.fence.doc_id, &pending.ids)
                } else {
                    applied
                }
            }
            _ => RemovalApply::Rejected,
        };
        if applied == RemovalApply::Busy {
            return;
        }
        let fence = pending.fence;
        if applied == RemovalApply::Applied {
            self.layout.invalidate();
            self.anim = None;
            self.morph.reset();
            self.morph_until = None;
            self.last_focus = None;
            self.context_node = None;
        }
        self.pending_removal = None;
        self.removal_released = false;
        self.search.resume_after_mutation(fence);
        self.search_submitted = false;
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        #[cfg(feature = "perf")]
        let frame_started = Instant::now();
        let ctx = ui.ctx().clone();
        #[cfg(feature = "perf")]
        { self.perf_highlight_draw = None; }
        #[cfg(feature = "perf")]
        let mut table_draw = None;
        self.poll_scan();
        self.poll_preview();
        if self.scanning.is_some() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }

        egui::Panel::top("toolbar").show(ui, |ui| self.toolbar(ui));
        egui::Panel::bottom("status").show(ui, |ui| self.status_bar(ui));

        let show_results = self.scanning.is_none()
            && self.doc.as_ref().is_some_and(|d| !d.search.query.trim().is_empty());
        if show_results {
            let mut action = search_panel::Action::None;
            egui::Panel::right("results").resizable(true).default_size(440.0).min_size(260.0).show(ui, |ui| {
                if let Some(doc) = &mut self.doc { search_panel::controls(ui, doc); }
                self.update_search();
                if let Some(doc) = &mut self.doc {
                    action = search_panel::results(ui, doc, self.mode);
                }
                #[cfg(feature = "perf")]
                { table_draw = self.perf_draw_identity(); }
            });
            if let Some(doc) = &mut self.doc {
                match action {
                    search_panel::Action::Reveal(id) => doc.reveal(id),
                    search_panel::Action::Context(id) => doc.reveal(id),
                    search_panel::Action::Open(id) => {
                        doc.reveal(id);
                        if doc.tree.node(id).kind == Kind::File {
                            open_item(doc, id);
                        }
                    }
                    search_panel::Action::None => {}
                }
            }
        }

        // Result clicks can change a focus-only scope before the treemap draws.
        if show_results { self.search_pass(false); } else { self.update_search(); }
        let bg = ui.style().visuals.extreme_bg_color;
        egui::CentralPanel::default().frame(egui::Frame::NONE.fill(bg)).show(ui, |ui| {
            if self.scanning.is_some() {
                self.scan_view(ui);
            } else if self.doc.is_some() {
                ui.add_enabled_ui(self.pending_removal.is_none(), |ui| self.treemap_view(ui));
            } else {
                self.welcome(ui);
            }
        });

        #[cfg(feature = "perf")]
        self.perf_record_draw(table_draw.as_ref());
        self.delete_dialog_ui(&ctx);
        self.toast_ui(&ctx);
        self.keyboard(&ctx);
        // Treemap/context/keyboard navigation happens after the results panel.
        let generation = self.search_generation;
        self.search_pass(false);
        if self.search_generation != generation { ctx.request_repaint(); }
        #[cfg(feature = "perf")]
        {
            if let Some(trace) = &mut self.perf_trace {
                trace.record("frame", self.doc.as_ref().map_or(0, |d| d.id), self.search_generation, "cpu", frame_started.elapsed());
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 화면 구성

impl App {
    fn toolbar(&mut self, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        let mut review = false;
        ui.horizontal(|ui| {
            ui.add_enabled_ui(self.delete_dialog.is_none() && self.pending_removal.is_none(), |ui| egui::ComboBox::from_id_salt("drive").selected_text("드라이브").width(90.0).show_ui(ui, |ui| {
                let mut chosen = None;
                for d in &self.drives {
                    let txt = format!(
                        "{}  {}  ({} 여유 / {})",
                        d.root,
                        d.kind_label(),
                        human_size(d.free),
                        human_size(d.total)
                    );
                    if ui.selectable_label(false, txt).clicked() {
                        chosen = Some(d.root.clone());
                    }
                }
                if let Some(r) = chosen {
                    self.start_scan(r);
                }
            }));

            let edit = ui.add_enabled(self.delete_dialog.is_none() && self.pending_removal.is_none(), egui::TextEdit::singleline(&mut self.root_input).desired_width(260.0).hint_text("스캔할 경로"));
            let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));

            if ui.add_enabled(self.delete_dialog.is_none() && self.pending_removal.is_none(), egui::Button::new("📁 폴더…")).clicked()
                && let Some(dir) = rfd::FileDialog::new().pick_folder() {
                    self.start_scan(dir.display().to_string());
                }
            if self.scanning.is_some() {
                if ui.button("⏹ 중지").clicked() {
                    self.cancel_scan();
                }
            } else if ui.add_enabled(self.delete_dialog.is_none() && self.pending_removal.is_none(), egui::Button::new("🔍 스캔")).clicked() || enter {
                self.start_scan(self.root_input.clone());
            }

            if !self.elevated
                && self.doc.as_ref().is_none_or(filesystem_actions_enabled)
                && ui
                    .button("🛡 관리자로 실행")
                    .on_hover_text("관리자 권한이면 NTFS의 $MFT를 직접 읽어 드라이브 전체를 몇 초 안에 스캔합니다")
                    .clicked()
            {
                if relaunch_as_admin(&format!("\"{}\"", self.root_input)) {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                } else {
                    self.toast("관리자 권한으로 다시 실행하지 못했습니다");
                }
            }

            ui.separator();
            let before = self.mode;
            ui.selectable_value(&mut self.mode, SizeMode::Allocated, "할당 크기")
                .on_hover_text("디스크에서 실제로 차지하는 크기 (삭제하면 비는 공간)");
            ui.selectable_value(&mut self.mode, SizeMode::Logical, "논리 크기").on_hover_text("파일 내용의 바이트 수");
            if before != self.mode {
                if let Some(doc) = &mut self.doc { doc.search.note_edit(Instant::now()); }
                self.layout.invalidate();
            }
            ui.separator();
            if ui
                .add_enabled(self.detail.level() > Detail::MIN, egui::Button::new("－"))
                .on_hover_text("덜 자세히: 큰 박스만 (-)")
                .clicked()
            {
                self.detail = self.detail.less();
            }
            ui.label(format!("상세도 {}/{}", self.detail.level(), Detail::MAX));
            if ui
                .add_enabled(self.detail.level() < Detail::MAX, egui::Button::new("＋"))
                .on_hover_text("더 자세히: 작은 폴더와 파일까지 (+)")
                .clicked()
            {
                self.detail = self.detail.more();
            }

            if let Some(doc) = &mut self.doc {
                ui.separator();
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut doc.search.query)
                        .desired_width(240.0)
                        .hint_text("🔎 검색  (예: *.mp4 size:>1gb)"),
                );
                if self.focus_search {
                    resp.request_focus();
                    self.focus_search = false;
                }
                if resp.changed() {
                    doc.search.note_edit(Instant::now());
                }
                if resp.has_focus() && ui.input(|i| i.key_pressed(Key::Escape)) && !doc.search.query.is_empty() {
                    doc.search.note_edit(Instant::now());
                    doc.search.query.clear();
                }
                resp.on_hover_text(
                    "이름 일부, *.확장자, ext:mp4;mkv, size:>1gb, size:<10mb,\ndm:<7d (최근 7일 수정), dm:>1y, path:단어, file:, folder:, !제외어",
                );

                if !doc.marks.is_empty() {
                    ui.separator();
                    let txt = format!("🗑 표시 {}개 · {}  검토…", doc.marks.len(), human_size(doc.marked_size(self.mode)));
                    if ui.add_enabled(filesystem_actions_enabled(doc) && self.scanning.is_none() && self.delete_dialog.is_none() && self.pending_removal.is_none(), egui::Button::new(RichText::new(txt).color(Color32::WHITE)).fill(Color32::from_rgb(170, 50, 45))).clicked() {
                        review = true;
                    }
                    if ui.add_enabled(self.pending_removal.is_none(), egui::Button::new("표시 모두 해제").small()).clicked() {
                        doc.clear_marks();
                    }
                }
            }
        });
        if review {
            self.open_delete_review();
        }

        if let Some(doc) = &mut self.doc {
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                if ui.add_enabled(!doc.back.is_empty(), egui::Button::new("⬅")).on_hover_text("뒤로 (Alt+←)").clicked() {
                    doc.go_back();
                }
                if ui.add_enabled(!doc.forward.is_empty(), egui::Button::new("➡")).on_hover_text("앞으로 (Alt+→)").clicked()
                {
                    doc.go_forward();
                }
                if ui
                    .add_enabled(doc.focus != Tree::ROOT, egui::Button::new("⬆"))
                    .on_hover_text("상위 폴더 (Backspace)")
                    .clicked()
                {
                    doc.go_up();
                }
                ui.separator();
                let chain = doc.tree.ancestry(doc.focus);
                let mut go = None;
                for (i, &n) in chain.iter().enumerate() {
                    if i > 0 {
                        ui.label(RichText::new("›").weak());
                    }
                    let name = doc.tree.name(n).trim_end_matches('\\').to_string();
                    let last = i + 1 == chain.len();
                    let txt = if last { RichText::new(name).strong() } else { RichText::new(name) };
                    if ui.add(egui::Button::new(txt).frame(false)).clicked() && !last {
                        go = Some(n);
                    }
                }
                if let Some(n) = go {
                    doc.navigate(n);
                }
                let f = doc.tree.node(doc.focus);
                ui.label(
                    RichText::new(format!("  {}  ·  {} 파일", human_size(f.size(self.mode)), thousands(f.files as u64))).weak(),
                );
            });
        }
        ui.add_space(4.0);
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        if self.pending_removal.is_some() {
            ui.ctx().request_repaint_after(Duration::from_millis(50));
        }
        ui.horizontal(|ui| {
            if let Some(e) = &self.error {
                ui.colored_label(Color32::from_rgb(230, 90, 80), format!("⚠ {e}"));
                return;
            }
            if self.pending_removal.is_some() {
                ui.label("삭제 결과를 반영하는 중입니다…");
                return;
            }
            if let Some(s) = &self.scanning {
                ui.spinner();
                ui.label(format!("{} 스캔 중… {:.1}s", s.root, s.started.elapsed().as_secs_f32()));
                return;
            }
            let Some(doc) = &self.doc else {
                ui.label("드라이브나 폴더를 골라 스캔하세요.");
                return;
            };
            let st = &doc.stats;
            let method = match st.method {
                ScanMethod::Mft => "⚡ MFT",
                ScanMethod::Walk => "일반 스캔",
            };
            let r = ui.label(format!(
                "{method} · {} 파일 · {} 폴더 · {:.2}s",
                thousands(st.files),
                thousands(st.dirs),
                st.elapsed.as_secs_f64()
            ));
            if let Some(reason) = &st.fallback_reason {
                r.on_hover_text(reason);
            }
            if st.errors > 0 {
                let label = if st.method == ScanMethod::Mft { "연결 끊긴 레코드" } else { "접근 실패" };
                ui.label(format!("· {label} {}", thousands(st.errors)));
            }
            if st.cancelled {
                ui.label("· 중지됨(일부 결과)");
            }
            if let Some((total, free)) = disk_space(doc.tree.name(Tree::ROOT)) {
                ui.separator();
                ui.label(format!("여유 공간 {} / {}", human_size(free), human_size(total)));
            }
            if let Some(h) = doc.hovered {
                ui.separator();
                ui.label(RichText::new(doc.tree.path(h)).weak());
            }
        });
    }

    fn welcome(&mut self, ui: &mut egui::Ui) {
        ui.add_space(40.0);
        ui.vertical_centered(|ui| {
            ui.heading("어느 드라이브를 살펴볼까요?");
            if !self.elevated {
                ui.label(RichText::new("관리자로 실행하면 드라이브 전체를 훨씬 빠르게 스캔합니다 (⚡ MFT)").weak());
            }
            ui.add_space(16.0);
            let mut chosen = None;
            for d in &self.drives {
                let used = d.total.saturating_sub(d.free);
                let frac = if d.total > 0 { used as f32 / d.total as f32 } else { 0.0 };
                // 카드 전체가 버튼이다
                let frame = egui::Frame::group(ui.style()).inner_margin(12.0).show(ui, |ui| {
                    ui.set_width(420.0);
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(&d.root).size(18.0).strong());
                        ui.label(RichText::new(d.kind_label()).weak());
                    });
                    ui.add(egui::ProgressBar::new(frac).desired_height(8.0));
                    ui.label(
                        RichText::new(format!("{} 사용 / {} · {} 여유", human_size(used), human_size(d.total), human_size(d.free)))
                            .weak(),
                    );
                });
                let rect = frame.response.rect;
                let resp = ui
                    .interact(rect, ui.id().with(("drive-card", &d.root)), egui::Sense::click())
                    .on_hover_cursor(egui::CursorIcon::PointingHand);
                if resp.hovered() {
                    let v = ui.visuals();
                    ui.painter().rect_filled(rect, 4.0, v.selection.bg_fill.gamma_multiply(0.15));
                    ui.painter().rect_stroke(rect, 4.0, egui::Stroke::new(1.5, v.selection.bg_fill), egui::StrokeKind::Inside);
                }
                if resp.clicked() {
                    chosen = Some(d.root.clone());
                }
                ui.add_space(6.0);
            }
            if let Some(r) = chosen {
                self.start_scan(r);
            }
        });
    }

    fn scan_view(&mut self, ui: &mut egui::Ui) {
        let (mode, detail, dark) = (self.mode, self.detail, ui.visuals().dark_mode);
        let Some(s) = &mut self.scanning else { return };
        let p = s.progress.clone();
        let (total_hint, done_hint) = (p.total_hint.load(Relaxed), p.done_hint.load(Relaxed));
        // 진행률: MFT는 읽은 레코드 비율, 일반 스캔은 드라이브 사용량 대비 스캔한 크기
        let frac = if total_hint > 0 {
            Some(done_hint as f64 / total_hint as f64)
        } else {
            s.used.filter(|&u| u > 0).map(|u| p.bytes.load(Relaxed) as f64 / u as f64)
        }
        .map(|f| f.clamp(0.0, 1.0));

        let elapsed = s.started.elapsed().as_secs_f64();
        let remaining = match frac {
            Some(f) if f >= 0.999 => Some(0.0),
            Some(f) if f > 0.02 && elapsed > 1.0 => Some(elapsed * (1.0 - f) / f),
            _ => None,
        };
        // 남은 시간이 매 프레임 튀지 않도록 부드럽게
        if let Some(r) = remaining {
            let prev = s.eta.map(|(v, at): (f64, Instant)| v - at.elapsed().as_secs_f64()).unwrap_or(r);
            s.eta = Some((prev * 0.9 + r * 0.1, Instant::now()));
        }
        let eta_text = match (frac, s.eta) {
            (Some(f), _) if f >= 0.999 => "마무리하는 중…".to_string(),
            (Some(_), Some((v, at))) => format!("남은 시간 약 {}", fmt_duration((v - at.elapsed().as_secs_f64()).max(1.0))),
            (Some(_), None) => "남은 시간 계산 중…".to_string(),
            (None, _) => "남은 시간을 알 수 없습니다".to_string(),
        };
        let bar = || match frac {
            Some(f) => egui::ProgressBar::new(f as f32).show_percentage(),
            None => egui::ProgressBar::new(0.0).animate(true),
        };

        let Some(tree) = &s.preview else {
            ui.add_space(80.0);
            ui.vertical_centered(|ui| {
                ui.heading(format!("{} 스캔 중", s.root));
                ui.add_space(16.0);
                ui.add(bar().desired_width(420.0));
                ui.add_space(8.0);
                ui.label(RichText::new(&eta_text).size(15.0));
                ui.add_space(16.0);
                if ui.button("중지").clicked() {
                    p.cancel();
                    self.preview.invalidate(s.scan_id);
                }
            });
            return;
        };

        ui.horizontal(|ui| {
            ui.add_space(6.0);
            ui.label(RichText::new(format!("{} 스캔 중", s.root)).strong());
            ui.add(bar().desired_width(260.0));
            ui.label(&eta_text);
            if ui.small_button("중지").clicked() {
                p.cancel();
                self.preview.invalidate(s.scan_id);
            }
        });
        let (resp, painter) = treemap::canvas(ui);
        let area = resp.rect.shrink(2.0);
        #[cfg(feature = "perf")]
        let layout_started = Instant::now();
        s.preview_layout.update(tree, s.scan_id, Tree::ROOT, area, mode, detail);
        #[cfg(feature = "perf")]
        if let Some(trace) = &mut self.perf_trace {
            trace.record("preview", s.scan_id, s.preview_layout.generation(), "layout", layout_started.elapsed());
        }
        let key = MorphKey { layout_id: LayoutSource::Preview(s.scan_id), generation: s.preview_layout.generation() };
        #[cfg(feature = "perf")]
        let morph_started = Instant::now();
        let (shown, moving) = morph_items(&mut self.morph, &s.preview_layout.items, key, &mut self.morph_items);
        #[cfg(feature = "perf")]
        if let Some(trace) = &mut self.perf_trace {
            trace.record("preview", s.scan_id, key.generation, "morph", morph_started.elapsed());
        }
        if moving {
            ui.ctx().request_repaint();
        }
        let hit = resp.hover_pos().and_then(|pos| shown.iter().rev().find(|it| it.rect.contains(pos))).copied();
        let hovered = hit.map(|it| it.node);
        let chain = hovered.map(|h| tree.ancestry(h)).unwrap_or_default();
        treemap::paint(
            &painter,
            tree,
            shown,
            None,
            &Style { dark },
            mode,
            &Highlights { hovered, hover_chain: &chain, selected: None, marked: &|_| false, filter: None },
        );
        if let Some(it) = hit {
            resp.on_hover_ui_at_pointer(|ui| {
                ui.set_min_width(220.0);
                ui.label(RichText::new(tree.name(it.node)).strong());
                let n = tree.node(it.node);
                ui.label(format!("{} · 파일 {}개 (스캔 중)", human_size(n.size(mode)), thousands(n.files as u64)));
            });
        }
    }

    fn treemap_view(&mut self, ui: &mut egui::Ui) {
        let (mode, detail) = (self.mode, self.detail);
        let dark = ui.visuals().dark_mode;
        let Some(doc) = &mut self.doc else { return };
        let (resp, painter) = treemap::canvas(ui);
        let area = resp.rect.shrink(2.0);

        // focus가 바뀌었으면 줌 애니메이션 준비 (줌 인은 이전 레이아웃에서 목표 위치를 찾는다)
        let mut zoom_out_from = None;
        if let Some((pid, pf)) = self.last_focus
            && pid == doc.id
            && pf != doc.focus
            && !self.layout.items.is_empty()
        {
            self.anim = None;
            if doc.tree.is_ancestor(pf, doc.focus) {
                if let Some(r) = self.layout.deepest_rect(&doc.tree.ancestry(doc.focus)) {
                    self.anim = Some(ZoomAnim::zoom_in(r, self.layout.items.clone()));
                }
            } else if doc.tree.is_ancestor(doc.focus, pf) {
                zoom_out_from = Some(pf);
            }
        }
        self.last_focus = Some((doc.id, doc.focus));
        #[cfg(feature = "perf")]
        let layout_started = Instant::now();
        self.layout.update(&doc.tree, doc.id, doc.focus, area, mode, detail);
        #[cfg(feature = "perf")]
        if let Some(trace) = &mut self.perf_trace {
            trace.record("document", doc.id, self.layout.generation(), "layout", layout_started.elapsed());
        }
        if let Some(pf) = zoom_out_from
            && let Some(r) = self.layout.deepest_rect(&doc.tree.ancestry(pf))
        {
            self.anim = Some(ZoomAnim::zoom_out(r));
        }
        let camera = self.anim.as_ref().and_then(|a| a.camera(area));
        if camera.is_some() {
            ui.ctx().request_repaint();
        } else {
            self.anim = None;
        }

        // 스캔이 막 끝났으면 미리 보기 모양에서 최종 모양으로 부드럽게 바꾼다
        #[cfg(feature = "perf")]
        let morph_started = Instant::now();
        let shown = if camera.is_none() && self.morph_until.is_some_and(|until| Instant::now() < until) {
            let key = MorphKey { layout_id: LayoutSource::Document(doc.id), generation: self.layout.generation() };
            let (shown, moving) = morph_items(&mut self.morph, &self.layout.items, key, &mut self.morph_items);
            if moving { ui.ctx().request_repaint(); }
            shown
        } else {
            if self.morph_until.take().is_some() { self.morph.invalidate(); }
            &self.layout.items
        };
        #[cfg(feature = "perf")]
        if let Some(trace) = &mut self.perf_trace {
            trace.record("document", doc.id, self.layout.generation(), "morph", morph_started.elapsed());
        }

        let hit = if camera.is_some() { None } else {
            resp.hover_pos().and_then(|pos| {
                if std::ptr::eq(shown, self.layout.items.as_slice()) { self.layout.hit(pos) }
                else { shown.iter().rev().find(|it| it.rect.contains(pos)) }
            }).copied()
        };
        doc.hovered = hit.map(|it| it.node);

        {
            let d: &Doc = doc;
            let marked = |id: NodeId| d.is_marked(id);
            let lit = |id: NodeId| d.search_lit(id);
            let filter: Option<&dyn Fn(NodeId) -> bool> = if d.search_active() { Some(&lit) } else { None };
            let chain = d.hovered.map(|h| d.tree.ancestry(h)).unwrap_or_default();
            let style = Style { dark };
            if let Some(a) = &self.anim
                && a.is_zoom_in()
            {
                let none = Highlights { hovered: None, hover_chain: &[], selected: None, marked: &marked, filter: None };
                treemap::paint(&painter, &d.tree, &a.old_items, None, &style, mode, &none);
            }
            treemap::paint(
                &painter,
                &d.tree,
                shown,
                camera,
                &style,
                mode,
                &Highlights { hovered: d.hovered, hover_chain: &chain, selected: d.selected, marked: &marked, filter },
            );
            #[cfg(feature = "perf")]
            { self.perf_highlight_draw = d.search.applied.as_ref().map(|key| crate::perf_trace::DrawIdentity {
                key: key.clone(), generation: self.search_generation,
            }); }

        }

        if resp.clicked() {
            doc.selected = selectable_node(hit.as_ref());
        }
        if resp.double_clicked()
            && let Some(it) = hit
        {
            match it.kind {
                ItemKind::OpenDir | ItemKind::ClosedDir => doc.navigate(it.node),
                ItemKind::File => open_item(doc, it.node),
                ItemKind::Link if filesystem_actions_enabled(doc) => reveal_in_explorer(&doc.tree.path(it.node)),
                ItemKind::Link => {},
                ItemKind::Group { .. } => {}
            }
        }
        if resp.secondary_clicked() {
            self.context_node = selectable_node(hit.as_ref());
            doc.selected = self.context_node;
        }

        // 휠: 위로 = 가리킨 곳으로 한 단계 확대, 아래로 = 상위로
        if resp.hovered() && camera.is_none() {
            let dy = ui.input(wheel_delta);
            self.wheel_acc += dy;
            if self.wheel_acc > 60.0 {
                self.wheel_acc = 0.0;
                if let Some(c) = hit.and_then(|it| doc.child_of_focus_toward(it.node))
                    && doc.tree.node(c).kind == Kind::Dir
                {
                    doc.navigate(c);
                }
            } else if self.wheel_acc < -60.0 {
                self.wheel_acc = 0.0;
                doc.go_up();
            }
            if dy == 0.0 {
                self.wheel_acc *= 0.9;
            }
        }

        let mut toggle = None;
        let ctx_node = self.context_node;
        resp.context_menu(|ui| {
            let Some(n) = ctx_node else {
                ui.close();
                return;
            };
            if context_menu(ui, doc, n) {
                toggle = Some(n);
            }
        });
        if let Some(n) = toggle {
            self.toggle_mark(n);
        }

        let Some(doc) = &self.doc else { return };
        if let Some(it) = hit {
            resp.on_hover_ui_at_pointer(|ui| tooltip(ui, doc, it, mode));
        }
    }

    fn delete_dialog_ui(&mut self, ctx: &egui::Context) {
        let Some(dlg) = &mut self.delete_dialog else { return };
        let mut removed = Vec::new();
        let open = delete::show(ctx, dlg, self.doc.as_mut(), self.mode, &mut removed);
        let source_id = dlg.doc_id;
        if !removed.is_empty() { self.begin_removal(source_id, removed); }
        if !open {
            self.delete_dialog = None;
        }
    }

    fn toast_ui(&mut self, ctx: &egui::Context) {
        let Some((msg, at)) = &self.toast else { return };
        if at.elapsed() > Duration::from_secs(3) {
            self.toast = None;
            return;
        }
        egui::Area::new(egui::Id::new("toast"))
            .anchor(egui::Align2::CENTER_BOTTOM, egui::vec2(0.0, -48.0))
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| ui.label(msg));
            });
        ctx.request_repaint_after(Duration::from_millis(200));
    }

    fn keyboard(&mut self, ctx: &egui::Context) {
        let ctrl_f = ctx.input(|i| i.modifiers.command && i.key_pressed(Key::F));
        if ctrl_f || (!ctx.egui_wants_keyboard_input() && ctx.input(|i| i.key_pressed(Key::Slash))) {
            self.focus_search = true;
        }
        if ctx.egui_wants_keyboard_input() || self.delete_dialog.is_some() || self.scanning.is_some() || self.pending_removal.is_some() {
            return;
        }
        let mut toggle = None;
        let mut detail = None;
        let Some(doc) = &mut self.doc else { return };
        ctx.input(|i| {
            if i.modifiers.alt && i.key_pressed(Key::ArrowLeft) || i.pointer.button_pressed(PointerButton::Extra1) {
                doc.go_back();
            } else if i.modifiers.alt && i.key_pressed(Key::ArrowRight)
                || i.pointer.button_pressed(PointerButton::Extra2)
            {
                doc.go_forward();
            } else if i.key_pressed(Key::Backspace) {
                doc.go_up();
            } else if i.key_pressed(Key::Enter) {
                if let Some(s) = doc.selected {
                    doc.navigate(s);
                }
            } else if i.key_pressed(Key::Escape) {
                doc.selected = None;
            } else if i.key_pressed(Key::Space) || i.key_pressed(Key::X) || i.key_pressed(Key::Delete) {
                toggle = doc.selected;
            } else if i.key_pressed(Key::Minus) {
                detail = Some(false);
            } else if i.key_pressed(Key::Plus) || i.key_pressed(Key::Equals) {
                detail = Some(true);
            }
        });
        if let Some(n) = toggle {
            self.toggle_mark(n);
        }
        match detail {
            Some(true) => self.detail = self.detail.more(),
            Some(false) => self.detail = self.detail.less(),
            None => {}
        }
    }
}

/// 묶음은 소유 폴더를 가리키지만 실제 선택 가능한 노드는 아니다.
fn selectable_node(item: Option<&treemap::Item>) -> Option<NodeId> {
    item.filter(|it| !matches!(it.kind, ItemKind::Group { .. }))
        .map(|it| it.node)
}

/// 오른쪽 클릭 메뉴. 삭제 표시를 바꿔야 하면 true.
fn context_menu(ui: &mut egui::Ui, doc: &mut Doc, n: NodeId) -> bool {
    let mut toggle = false;
    let path = doc.tree.path(n);
    ui.label(RichText::new(doc.tree.name(n)).strong());
    ui.separator();
    if doc.tree.node(n).kind == Kind::Dir && ui.button("🔍 여기로 확대").clicked() {
        doc.navigate(n);
        ui.close();
    }
    match doc.tree.node(n).kind {
        Kind::File => {
            if ui.add_enabled(filesystem_actions_enabled(doc), egui::Button::new("📄 열기")).clicked() {
                open_item(doc, n);
                ui.close();
            }
        }
        Kind::Dir => {
            if ui.add_enabled(filesystem_actions_enabled(doc), egui::Button::new("📂 폴더 열기")).clicked() {
                shell_open(&path);
                ui.close();
            }
        }
        Kind::Link => {}
    }
    if ui.add_enabled(filesystem_actions_enabled(doc), egui::Button::new("🔎 탐색기에서 위치 보기")).clicked() {
        reveal_in_explorer(&path);
        ui.close();
    }
    if ui.button("📋 경로 복사").clicked() {
        ui.ctx().copy_text(path.clone());
        ui.close();
    }
    ui.separator();
    let state = doc.mark_state(n);
    let protected = protected_reason(&doc.tree, n);
    if state == 1 {
        if ui.button("↩ 삭제 표시 해제").clicked() {
            toggle = true;
            ui.close();
        }
    } else if state == 2 {
        ui.add_enabled(false, egui::Button::new("🗑 상위 폴더가 이미 표시됨"));
    } else if let Some(r) = protected {
        ui.add_enabled(false, egui::Button::new("🗑 삭제 표시")).on_disabled_hover_text(format!("보호된 항목: {r}"));
    } else if ui.button("🗑 삭제 표시  (Space)").clicked() {
        toggle = true;
        ui.close();
    }
    toggle
}

fn tooltip(ui: &mut egui::Ui, doc: &Doc, it: treemap::Item, mode: SizeMode) {
    let tree = &doc.tree;
    let n = tree.node(it.node);
    if let ItemKind::Group { count, size } = it.kind {
        ui.label(RichText::new(format!("작은 항목 {count}개")).strong());
        ui.label(format!("{} · {}", human_size(size), tree.path(it.node)));
        ui.label(RichText::new("폴더를 확대하면 보입니다").weak());
        return;
    }
    ui.label(RichText::new(tree.name(it.node)).strong());
    ui.label(RichText::new(tree.path(it.node)).weak().small());
    ui.separator();
    let size = n.size(mode);
    let pct = |of: NodeId| {
        let t = tree.node(of).size(mode);
        if t > 0 { size as f64 * 100.0 / t as f64 } else { 0.0 }
    };
    egui::Grid::new("tip").num_columns(2).spacing([12.0, 2.0]).show(ui, |ui| {
        ui.label("크기");
        ui.label(format!("{} ({} B)", human_size(size), thousands(size)));
        ui.end_row();
        let other = match mode {
            SizeMode::Allocated => ("논리 크기", n.logical),
            SizeMode::Logical => ("할당 크기", n.alloc),
        };
        ui.label(other.0);
        ui.label(human_size(other.1));
        ui.end_row();
        if let Some(p) = tree.parent(it.node) {
            ui.label("비율");
            ui.label(format!("상위 폴더의 {:.1}% · 현재 보기의 {:.1}%", pct(p), pct(doc.focus)));
            ui.end_row();
        }
        if n.kind == Kind::Dir {
            ui.label("파일 수");
            ui.label(thousands(n.files as u64));
            ui.end_row();
        }
        if n.mtime > 0 {
            ui.label("수정");
            ui.label(format_time(n.mtime));
            ui.end_row();
        }
    });
    match doc.mark_state(it.node) {
        1 => {
            ui.colored_label(Color32::from_rgb(230, 90, 80), "🗑 삭제 표시됨");
        }
        2 => {
            ui.colored_label(Color32::from_rgb(230, 90, 80), "🗑 상위 폴더가 삭제 표시됨");
        }
        _ => {}
    }
    if n.has(flags::DENIED) {
        ui.colored_label(Color32::from_rgb(230, 160, 60), "⚠ 접근이 거부되어 일부 크기를 모릅니다");
    }
    if n.has(flags::HARDLINK_DUP) {
        ui.label(RichText::new("하드링크: 다른 경로에서 이미 세었으므로 0으로 계산").weak());
    }
    if n.kind == Kind::Link {
        ui.label(RichText::new("junction/심볼릭 링크: 따라가지 않음").weak());
    }
}

fn morph_items<'a>(morph: &mut Morph, items: &'a [Item], key: MorphKey, out: &'a mut Vec<Item>) -> (&'a [Item], bool) {
    if morph.is_settled(key) { return (items, false); }
    let moving = morph.step_into(items, key, out);
    (if morph.is_settled(key) { items } else { out }, moving)
}

fn fmt_duration(secs: f64) -> String {
    let s = secs.round() as u64;
    if s < 60 { format!("{s}초") } else { format!("{}분 {:02}초", s / 60, s % 60) }
}

/// 이번 프레임의 휠 이동량 (위로 굴리면 양수, 점 단위)
fn wheel_delta(i: &egui::InputState) -> f32 {
    i.raw
        .events
        .iter()
        .map(|e| match e {
            egui::Event::MouseWheel { unit, delta, .. } => {
                delta.y
                    * match unit {
                        egui::MouseWheelUnit::Point => 1.0,
                        egui::MouseWheelUnit::Line => 40.0,
                        egui::MouseWheelUnit::Page => 400.0,
                    }
            }
            _ => 0.0,
        })
        .sum()
}

/// 파일을 기본 앱으로 연다
fn open_item(doc: &Doc, id: NodeId) {
    if !filesystem_actions_enabled(doc) { return; }
    let path = doc.tree.path(id);
    if !shell_open(&path) {
        reveal_in_explorer(&path);
    }
}

fn reveal_in_explorer(path: &str) {
    let _ = std::process::Command::new("explorer.exe").raw_arg(format!("/select,\"{path}\"")).spawn();
}

// The marker is carried by the synthetic tree, so leaf handlers cannot bypass it.
fn filesystem_actions_enabled(doc: &Doc) -> bool {
    #[cfg(feature = "perf")]
    if doc.tree.node(Tree::ROOT).has(flags::SYNTHETIC) { return false; }
    #[cfg(not(feature = "perf"))]
    let _ = doc;
    true
}

impl Drop for App {
    fn drop(&mut self) {
        self.cancel_scan();
        self.preview.shutdown();
        self.search.shutdown();
        #[cfg(feature = "perf")]
        if let Some(trace) = &mut self.perf_trace { trace.finish(); }
    }
}

#[cfg(feature = "perf")]
impl App {
    fn perf_supersede(&mut self) {
        if let Some(previous) = self.perf_measurement.take()
            && !previous.drawn
            && let Some(trace) = &mut self.perf_trace
        {
            trace.record(
                "search",
                previous.key.doc_id,
                previous.generation,
                "superseded",
                previous.input_at.elapsed(),
            );
        }
    }

    fn perf_observe_input(&mut self) {
        if self.perf_trace.is_none() {
            return;
        }
        let Some(key) = self.search_key.clone() else {
            return;
        };
        if self
            .perf_measurement
            .as_ref()
            .is_some_and(|m| m.generation == self.search_generation)
        {
            return;
        }
        self.perf_supersede();
        let input_at = self
            .doc
            .as_ref()
            .and_then(|doc| doc.search.edited_at)
            .unwrap_or_else(Instant::now);
        let fixture = self.perf_fixture_id.as_deref().unwrap_or("real-corpus");
        let mode = match key.mode {
            SizeMode::Allocated => "allocated",
            SizeMode::Logical => "logical",
        };
        let trace = self.perf_trace.as_mut().unwrap();
        trace.map_query(
            key.doc_id,
            self.search_generation,
            fixture,
            &key.query,
            key.scope,
            mode,
        );
        trace.record(
            "search",
            key.doc_id,
            self.search_generation,
            "input",
            Duration::ZERO,
        );
        self.perf_measurement = Some(crate::perf_trace::Measurement {
            key,
            generation: self.search_generation,
            input_at,
            drawn: false,
        });
    }

    fn perf_draw_identity(&self) -> Option<crate::perf_trace::DrawIdentity> {
        let doc = self.doc.as_ref()?;
        let measurement = self.perf_measurement.as_ref()?;
        if doc.search.result.is_none()
            || doc.search.error.is_some()
            || measurement.generation != self.search_generation
            || doc.search.applied.as_ref() != Some(&measurement.key)
            || doc.search_key(self.mode) != measurement.key
        {
            return None;
        }
        Some(crate::perf_trace::DrawIdentity {
            key: measurement.key.clone(),
            generation: measurement.generation,
        })
    }

    fn perf_record_draw(&mut self, table: Option<&crate::perf_trace::DrawIdentity>) {
        let Some(table) = table else { return };
        if self.perf_highlight_draw.as_ref() != Some(table) {
            return;
        }
        let (Some(trace), Some(measurement)) = (&mut self.perf_trace, &mut self.perf_measurement)
        else {
            return;
        };
        if measurement.drawn
            || measurement.generation != table.generation
            || measurement.key != table.key
        {
            return;
        }
        trace.record(
            "search",
            table.key.doc_id,
            table.generation,
            "first_accepted_draw",
            measurement.input_at.elapsed(),
        );
        measurement.drawn = true;
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::tests::{doc, fixture, node};
    use darak_core::NO_NODE;

    fn app(doc: Doc) -> App {
        let (search, runtime) = crate::search_worker::SearchRuntime::new();
        runtime.shutdown_and_join();
        let (preview, runtime) = crate::preview_worker::PreviewRuntime::new();
        runtime.shutdown_and_join();
        App {
            preview, next_scan_id: 1,
            search, search_key: None, search_generation: 0, search_submitted: false,
            search_failed: None, pending_removal: None, removal_released: false, next_fence: 0,
            drives: Vec::new(),
            root_input: "C:\\Data".into(),
            scanning: None,
            doc: Some(doc),
            error: None,
            toast: None,
            mode: SizeMode::Allocated,
            layout: Layout::default(),
            wheel_acc: 0.0,
            context_node: None,
            next_doc_id: 2,
            elevated: false,
            delete_dialog: None,
            focus_search: false,
            detail: Detail::default(),
            anim: None,
            last_focus: None,
            morph: Morph::default(),
            morph_items: Vec::new(),
            morph_until: None,
            #[cfg(feature = "perf")] perf_trace: None,
            #[cfg(feature = "perf")] perf_fixture_id: None,
            #[cfg(feature = "perf")] perf_measurement: None,
            #[cfg(feature = "perf")] perf_highlight_draw: None,
        }
    }

    #[cfg(feature = "perf")]
    #[test]
    fn synthetic_fixture_disables_filesystem_actions_and_delete_review() {
        let tree = darak_core::perf::make_fixture(100, darak_core::perf::Shape::Balanced, 25);
        let mut app = app(doc(1, tree));
        assert!(!filesystem_actions_enabled(app.doc.as_ref().unwrap()));
        app.doc.as_mut().unwrap().marks.insert(1);
        app.open_delete_review();
        assert!(app.delete_dialog.is_none());
        assert!(filesystem_actions_enabled(&fixture(2)));
    }

    #[cfg(feature = "perf")]
    #[test]
    fn perf_generation_tracks_query_mode_and_focus_scope() {
        let mut app = app(fixture(1));
        let (trace, _runtime) = crate::perf_trace::PerfTraceRuntime::new("unused.csv".into());
        app.perf_trace = Some(trace);
        app.observe_search_input();
        let generation = app.search_generation;
        app.observe_search_input();
        assert_eq!(app.search_generation, generation);
        app.doc.as_mut().unwrap().search.query = "file:".into();
        app.observe_search_input();
        assert_eq!(app.search_generation, generation + 1);
        app.doc.as_mut().unwrap().focus = 1;
        app.observe_search_input();
        assert_eq!(app.search_generation, generation + 1);
        app.doc.as_mut().unwrap().search.in_focus_only = true;
        app.observe_search_input();
        assert_eq!(app.search_generation, generation + 2);
        app.mode = SizeMode::Logical;
        app.observe_search_input();
        assert_eq!(app.search_generation, generation + 3);
    }

    fn group_fixture() -> Doc {
        let mut tree = Tree::default();
        node(&mut tree, "C:\\Data", NO_NODE, Kind::Dir, 11_000_000, 1, 1);
        node(&mut tree, "owner", 0, Kind::Dir, 11_000_000, 2, 1001);
        node(&mut tree, "large", 1, Kind::File, 10_000_000, 0, 0);
        for i in 0..1000 {
            node(&mut tree, &format!("small{i}"), 1, Kind::File, 1000, 0, 0);
        }
        doc(1, tree)
    }

    fn frame(ctx: &egui::Context, app: &mut App, events: Vec<egui::Event>, time: f64) {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(800.0, 600.0),
            )),
            events,
            time: Some(time),
            ..Default::default()
        };
        let mut output = ctx.run_ui(input, |ui| {
            egui::CentralPanel::default().show(ui, |ui| app.treemap_view(ui));
            app.keyboard(ui.ctx());
        });
        output.textures_delta.clear();
    }

    #[test]
    fn group_click_then_delete_does_not_mark_owner() {
        for key in [Key::Delete, Key::Space, Key::X] {
            let ctx = egui::Context::default();
            let mut app = app(group_fixture());
            app.doc.as_mut().unwrap().selected = Some(2);
            frame(&ctx, &mut app, vec![], 0.0);
            let group = app
                .layout
                .items
                .iter()
                .find(|it| matches!(it.kind, ItemKind::Group { .. }))
                .unwrap();
            assert_eq!(group.node, 1);
            let pos = group.rect.center();
            frame(
                &ctx,
                &mut app,
                vec![
                    egui::Event::PointerMoved(pos),
                    egui::Event::PointerButton {
                        pos,
                        button: PointerButton::Primary,
                        pressed: true,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
                0.1,
            );
            frame(
                &ctx,
                &mut app,
                vec![egui::Event::PointerButton {
                    pos,
                    button: PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::NONE,
                }],
                0.2,
            );
            frame(
                &ctx,
                &mut app,
                vec![egui::Event::Key {
                    key,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                }],
                0.3,
            );
            let doc = app.doc.as_ref().unwrap();
            assert_eq!(
                doc.selected, None,
                "group click selected owner before {key:?}"
            );
            assert!(
                doc.marks.is_empty(),
                "group click marked owner with {key:?}"
            );
            assert_eq!(doc.marked_size(SizeMode::Allocated), 0);
        }
    }

    fn click(ctx: &egui::Context, app: &mut App, pos: egui::Pos2, button: PointerButton) {
        frame(
            ctx,
            app,
            vec![
                egui::Event::PointerMoved(pos),
                egui::Event::PointerButton {
                    pos,
                    button,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
            0.1,
        );
        frame(
            ctx,
            app,
            vec![egui::Event::PointerButton {
                pos,
                button,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            }],
            0.2,
        );
    }

    #[test]
    fn excludes_groups_from_node_selection() {
        let ctx = egui::Context::default();
        let mut app = app(group_fixture());
        app.doc.as_mut().unwrap().selected = Some(2);
        frame(&ctx, &mut app, vec![], 0.0);
        let group = app
            .layout
            .items
            .iter()
            .find(|it| matches!(it.kind, ItemKind::Group { .. }))
            .unwrap();
        assert_eq!(selectable_node(app.layout.hit(group.rect.center())), None);
        for id in [1, 2] {
            let item = app
                .layout
                .items
                .iter()
                .find(|it| it.node == id && !matches!(it.kind, ItemKind::Group { .. }))
                .unwrap();
            assert_eq!(selectable_node(Some(item)), Some(id));
        }
        let pos = group.rect.center();
        click(&ctx, &mut app, pos, PointerButton::Secondary);
        assert_eq!(app.context_node, None);
        assert_eq!(app.doc.as_ref().unwrap().selected, None);
    }

    #[test]
    fn real_file_and_folder_clicks_still_mark() {
        for id in [1, 2] {
            let ctx = egui::Context::default();
            let mut app = app(group_fixture());
            frame(&ctx, &mut app, vec![], 0.0);
            let rect = app.layout.rect_of(id).unwrap();
            let pos = if id == 1 {
                egui::pos2(rect.center().x, rect.min.y + 8.0)
            } else {
                rect.center()
            };
            click(&ctx, &mut app, pos, PointerButton::Primary);
            frame(
                &ctx,
                &mut app,
                vec![egui::Event::Key {
                    key: Key::Space,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                }],
                0.3,
            );
            let doc = app.doc.as_ref().unwrap();
            assert_eq!(doc.selected, Some(id));
            assert_eq!(doc.marks.iter().copied().collect::<Vec<_>>(), vec![id]);
            assert_eq!(
                doc.marked_size(SizeMode::Allocated),
                doc.tree.node(id).alloc
            );
        }
    }

    #[test]
    fn blocks_conflicting_operation_starts() {
        let mut app = app(fixture(1));
        app.doc.as_mut().unwrap().toggle_mark(3).unwrap();
        let (_tx, rx) = mpsc::channel();
        app.scanning = Some(Scanning {
            scan_id: 1,
            root: "C:\\Data".into(),
            progress: Arc::new(ScanProgress::default()),
            rx,
            started: Instant::now(),
            used: None,
            eta: None,
            preview: None,
            preview_version: 0,
            preview_layout: Layout::stable(),
            last_preview: Instant::now(),
        });
        let progress = app.scanning.as_ref().unwrap().progress.clone();
        app.open_delete_review();
        assert!(app.delete_dialog.is_none());
        assert!(Arc::ptr_eq(
            &progress,
            &app.scanning.as_ref().unwrap().progress
        ));
        assert!(!progress.is_cancelled());
        app.scanning = None;
        app.open_delete_review();
        assert_eq!(app.delete_dialog.as_ref().unwrap().doc_id, 1);
        let original = app.root_input.clone();
        app.start_scan("C:\\does-not-exist".into());
        assert!(app.scanning.is_none());
        assert_eq!(app.root_input, original);
        let (_tx, rx) = mpsc::channel();
        app.delete_dialog.as_mut().unwrap().stage = Stage::Running {
            total: 1,
            done: Arc::default(),
            rx,
        };
        app.start_scan("C:\\does-not-exist".into());
        assert!(app.scanning.is_none());
        assert_eq!(app.root_input, original);
        assert_eq!(app.doc.as_ref().unwrap().id, 1);
    }

    #[test]
    fn handles_document_change_during_review() {
        let mut app = app(fixture(1));
        app.doc.as_mut().unwrap().toggle_mark(3).unwrap();
        app.open_delete_review();
        app.doc = Some(fixture(2));
        let ctx = egui::Context::default();
        app.delete_dialog_ui(&ctx);
        assert!(app.delete_dialog.is_none());
        assert_eq!(app.doc.as_ref().unwrap().tree.version, 0);
        assert!(app.doc.as_ref().unwrap().marks.is_empty());
    }

    #[test]
    fn preserves_foreign_running_outcomes_without_reading_new_nodes() {
        for no_doc in [false, true] {
            let mut app = app(fixture(2));
            let (tx, rx) = mpsc::channel();
            tx.send(vec![delete::Outcome {
                node: u32::MAX,
                path: "C:\\original-source".into(),
                size: 60,
                error: None,
            }])
            .unwrap();
            app.delete_dialog = Some(Dialog {
                doc_id: 1,
                stage: Stage::Running {
                    total: 1,
                    done: Arc::default(),
                    rx,
                },
            });
            if no_doc {
                app.doc = None;
            }
            let ctx = egui::Context::default();
            let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
                app.delete_dialog_ui(ui.ctx())
            });
            output.textures_delta.clear();
            let dlg = app.delete_dialog.as_ref().unwrap();
            assert_eq!(dlg.doc_id, 1);
            let Stage::Report { outcomes } = &dlg.stage else {
                panic!("source outcome report missing")
            };
            assert_eq!(outcomes[0].path, "C:\\original-source");
            assert_eq!(outcomes[0].node, u32::MAX);
            if let Some(doc) = &app.doc {
                assert_eq!(doc.id, 2);
                assert_eq!(doc.tree.version, 0);
                assert_eq!(doc.tree.node(0).alloc, 100);
                assert!(doc.marks.is_empty());
            }
            let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
                app.delete_dialog_ui(ui.ctx())
            });
            output.textures_delta.clear();
            assert!(matches!(
                app.delete_dialog.as_ref().unwrap().stage,
                Stage::Report { .. }
            ));
        }
    }
    struct TestWake {
        ctx: egui::Context,
        rx: mpsc::Receiver<()>,
    }

    fn running_app(
        hook: impl Fn(u64) + Send + Sync + 'static,
    ) -> (App, crate::search_worker::SearchRuntime, TestWake) {
        let mut app = app(fixture(1));
        let (mut controller, runtime) =
            crate::search_worker::SearchRuntime::with_hook(Box::new(hook));
        let (tx, wake) = mpsc::channel();
        let ctx = egui::Context::default();
        ctx.set_request_repaint_callback(move |_| {
            let _ = tx.send(());
        });
        controller.set_waker(ctx.clone());
        app.search = controller;
        (app, runtime, TestWake { ctx, rx: wake })
    }

    fn pump_until(app: &mut App, wake: &TestWake, done: impl Fn(&App) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let mut output = wake
                .ctx
                .run_ui(egui::RawInput::default(), |_| app.update_search());
            output.textures_delta.clear();
            if done(app) {
                return;
            }
            wake.rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("worker repaint deadline");
        }
    }

    fn shutdown(runtime: crate::search_worker::SearchRuntime) {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            runtime.shutdown_and_join();
            tx.send(()).unwrap();
        });
        rx.recv_timeout(Duration::from_secs(10))
            .expect("worker join deadline");
    }

    #[test]
    fn mutation_fence_blocks_resubmission() {
        let (started_tx, started) = mpsc::channel();
        let (release_tx, release) = mpsc::channel();
        let release = std::sync::Mutex::new(release);
        let (mut app, runtime, wake) = running_app(move |generation| {
            started_tx.send(generation).unwrap();
            if generation == 1 {
                release
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(10))
                    .unwrap();
            }
        });
        app.doc.as_mut().unwrap().search.query = "file:".into();
        app.update_search();
        assert_eq!(started.recv_timeout(Duration::from_secs(10)).unwrap(), 1);
        app.doc.as_mut().unwrap().search.query = "folder:".into();
        app.update_search(); // One active and one pending lease.
        assert_eq!(Arc::strong_count(&app.doc.as_ref().unwrap().tree), 3);
        app.begin_removal(1, vec![1]);
        app.doc.as_mut().unwrap().search.query = "sibling".into();
        app.mode = SizeMode::Logical;
        for _ in 0..3 {
            app.update_search();
        }
        assert!(!app.search_submitted);
        assert_eq!(app.doc.as_ref().unwrap().tree.version, 0);
        assert_eq!(Arc::strong_count(&app.doc.as_ref().unwrap().tree), 2);
        assert!(started.try_recv().is_err());
        app.start_scan("C:\\blocked".into());
        app.open_delete_review();
        app.toggle_mark(3);
        assert!(app.scanning.is_none());
        assert!(app.delete_dialog.is_none());
        assert!(app.doc.as_ref().unwrap().marks.is_empty());
        release_tx.send(()).unwrap();
        pump_until(&mut app, &wake, |app| {
            app.doc.as_ref().unwrap().search.result.is_some()
        });
        let doc = app.doc.as_mut().unwrap();
        assert_eq!(doc.tree.version, 1);
        assert!(Arc::get_mut(&mut doc.tree).is_some());
        assert_eq!(doc.search.result.as_ref().unwrap().hits, [2]);
        assert_eq!(
            started.recv_timeout(Duration::from_secs(10)).unwrap(),
            app.search_generation
        );
        assert!(started.try_recv().is_err());
        assert!(app.pending_removal.is_none());
        shutdown(runtime);
    }

    const REMOVAL_WAIT_TEXT: &str = "삭제 결과를 반영하는 중입니다…";

    fn removal_status_frame(ctx: &egui::Context, app: &mut App, time: f64) -> egui::FullOutput {
        let mut output = ctx.run_ui(egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1000.0, 400.0))),
            time: Some(time),
            ..Default::default()
        }, |ui| {
            egui::Panel::bottom("removal-status-test").show(ui, |ui| app.status_bar(ui));
            app.update_search();
        });
        output.textures_delta.clear();
        output
    }

    fn removal_shape_has_text(shape: &egui::epaint::Shape, text: &str) -> bool {
        match shape {
            egui::epaint::Shape::Text(shape) => shape.galley.text().contains(text),
            egui::epaint::Shape::Vec(shapes) => shapes.iter().any(|shape| removal_shape_has_text(shape, text)),
            _ => false,
        }
    }

    fn has_removal_text(output: &egui::FullOutput, text: &str) -> bool {
        output.shapes.iter().any(|shape| removal_shape_has_text(&shape.shape, text))
    }

    #[test]
    fn removal_wait_repaints_until_applied() {
        let ctx = egui::Context::default();
        let mut app = app(fixture(1));
        let mut time = 0.0;
        for _ in 0..4 {
            removal_status_frame(&ctx, &mut app, time);
            time += 0.1;
        }
        let lease = Arc::clone(&app.doc.as_ref().unwrap().tree);
        app.begin_removal(1, vec![1]);
        let fence = app.pending_removal.as_ref().unwrap().fence;
        app.search_control(SearchControlEvent::Released(fence));
        let output = removal_status_frame(&ctx, &mut app, time);
        assert!(has_removal_text(&output, REMOVAL_WAIT_TEXT));
        let delay = output.viewport_output[&egui::ViewportId::ROOT].repaint_delay;
        assert!(delay <= Duration::from_millis(50), "retry repaint not scheduled: {delay:?}");
        assert!(app.pending_removal.is_some());
        assert_eq!(app.doc.as_ref().unwrap().tree.version, 0);
        assert!(app.error.is_none());
        time += delay.as_secs_f64();
        drop(lease);
        let output = removal_status_frame(&ctx, &mut app, time);
        assert!(app.pending_removal.is_none());
        assert!(!app.removal_released);
        assert_eq!(app.doc.as_ref().unwrap().tree.version, 1);
        // The status drawn before this frame's apply needs one last repaint.
        let delay = output.viewport_output[&egui::ViewportId::ROOT].repaint_delay;
        assert!(delay <= Duration::from_millis(50));
        time += delay.as_secs_f64();
        let mut output = removal_status_frame(&ctx, &mut app, time);
        for _ in 0..3 {
            time += 0.1;
            output = removal_status_frame(&ctx, &mut app, time);
        }
        assert!(!has_removal_text(&output, REMOVAL_WAIT_TEXT));
        assert!(output.viewport_output[&egui::ViewportId::ROOT].repaint_delay > Duration::from_millis(50));
        assert!(app.error.is_none());
        assert_eq!(app.doc.as_ref().unwrap().tree.version, 1);
    }

    #[test]
    fn removal_wait_preserves_scan_error() {
        for error in [None, Some("기존 스캔 오류".to_owned())] {
            let ctx = egui::Context::default();
            let mut app = app(fixture(1));
            app.error = error.clone();
            for frame in 0..4 {
                removal_status_frame(&ctx, &mut app, frame as f64 * 0.1);
            }
            let lease = Arc::clone(&app.doc.as_ref().unwrap().tree);
            app.begin_removal(1, vec![1]);
            let fence = app.pending_removal.as_ref().unwrap().fence;
            app.search_control(SearchControlEvent::Released(fence));
            let output = removal_status_frame(&ctx, &mut app, 0.4);
            assert_eq!(app.error, error);
            assert!(output.viewport_output[&egui::ViewportId::ROOT].repaint_delay <= Duration::from_millis(50));
            if let Some(error) = &error {
                assert!(has_removal_text(&output, error));
            }
            drop(lease);
            app.finish_removal();
            assert_eq!(app.error, error);
            assert!(app.pending_removal.is_none());
        }
    }

    #[test]
    fn rejected_removal_clears_wait_state() {
        for replacement in [Some((2, 0)), Some((1, 1)), None] {
            let mut app = app(fixture(1));
            let lease = Arc::clone(&app.doc.as_ref().unwrap().tree);
            app.begin_removal(1, vec![1]);
            let fence = app.pending_removal.as_ref().unwrap().fence;
            app.search_control(SearchControlEvent::Released(fence));
            app.finish_removal();
            assert!(app.pending_removal.is_some());
            app.doc = replacement.map(|(id, version)| {
                let mut doc = fixture(id);
                Arc::get_mut(&mut doc.tree).unwrap().version = version;
                doc
            });
            app.finish_removal();
            assert!(app.pending_removal.is_none());
            assert!(!app.removal_released);
            assert!(app.error.is_none());
            if let Some((_, version)) = replacement {
                let tree = &app.doc.as_ref().unwrap().tree;
                assert_eq!(tree.version, version);
                assert!(!tree.node(1).has(flags::REMOVED));
            }
            let ctx = egui::Context::default();
            let output = removal_status_frame(&ctx, &mut app, 0.0);
            assert!(!has_removal_text(&output, REMOVAL_WAIT_TEXT));
            drop(lease);
        }
    }

    #[test]
    fn stuck_lease_is_detached_after_timeout() {
        let mut app = app(fixture(1));
        let lease = Arc::clone(&app.doc.as_ref().unwrap().tree);
        app.begin_removal(1, vec![1]);
        let fence = app.pending_removal.as_ref().unwrap().fence;
        app.search_control(SearchControlEvent::Released(fence));
        app.finish_removal();
        assert!(app.pending_removal.is_some(), "waits before the timeout");
        let released_at = app.pending_removal.as_ref().unwrap().released_at.unwrap();
        // 같은 fence가 다시 와도 기다리기 시작한 시각은 바뀌지 않는다.
        app.search_control(SearchControlEvent::Released(fence));
        assert_eq!(app.pending_removal.as_ref().unwrap().released_at, Some(released_at));
        app.pending_removal.as_mut().unwrap().released_at =
            Some(Instant::now() - REMOVAL_DETACH_AFTER);
        app.finish_removal();
        assert!(app.pending_removal.is_none());
        assert!(!app.removal_released);
        assert!(app.error.is_none());
        let doc = app.doc.as_ref().unwrap();
        assert!(!Arc::ptr_eq(&doc.tree, &lease), "the document owns a detached copy");
        assert_eq!(doc.tree.version, 1);
        assert!(doc.tree.node(1).has(flags::REMOVED));
        assert_eq!(doc.tree.node(0).alloc, 40);
        // 붙잡고 있던 쪽의 트리는 바뀌지 않는다.
        assert_eq!(lease.version, 0);
        assert!(!lease.node(1).has(flags::REMOVED));
    }

    #[test]
    fn wrong_fence_cannot_apply_removal() {
        let mut app = app(fixture(1));
        app.begin_removal(1, vec![1]);
        let fence = app.pending_removal.as_ref().unwrap().fence;
        for wrong in [
            ReleaseFence {
                token: fence.token + 1,
                ..fence
            },
            ReleaseFence { doc_id: 2, ..fence },
            ReleaseFence {
                tree_version: 1,
                ..fence
            },
        ] {
            app.search_control(SearchControlEvent::Released(wrong));
            app.finish_removal();
            assert_eq!(app.doc.as_ref().unwrap().tree.version, 0);
            assert!(!app.removal_released);
        }
        let lease = Arc::clone(&app.doc.as_ref().unwrap().tree);
        app.search_control(SearchControlEvent::Released(fence));
        app.finish_removal();
        assert!(app.pending_removal.is_some());
        assert!(app.removal_released);
        assert!(app.error.is_none());
        assert_eq!(app.doc.as_ref().unwrap().tree.version, 0);
        drop(lease);
        app.finish_removal();
        assert_eq!(app.doc.as_ref().unwrap().tree.version, 1);
        assert!(app.pending_removal.is_none());
        assert!(!app.removal_released);
    }

    #[test]
    fn worker_failure_does_not_strand_pending_removal() {
        for fail_before_fence in [false, true] {
            let (started_tx, started) = mpsc::channel();
            let (release_tx, release) = mpsc::channel();
            let release = std::sync::Mutex::new(release);
            let (mut app, runtime, wake) = running_app(move |_| {
                started_tx.send(()).unwrap();
                release
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(10))
                    .unwrap();
                panic!("injected search failure");
            });
            app.doc.as_mut().unwrap().search.query = "file:".into();
            app.update_search();
            started.recv_timeout(Duration::from_secs(10)).unwrap();
            if !fail_before_fence {
                app.begin_removal(1, vec![1]);
            }
            release_tx.send(()).unwrap();
            pump_until(&mut app, &wake, |app| app.search_failed.is_some());
            if fail_before_fence {
                app.begin_removal(1, vec![1]);
            }
            pump_until(&mut app, &wake, |app| app.pending_removal.is_none());
            assert_eq!(app.doc.as_ref().unwrap().tree.version, 1);
            app.doc.as_mut().unwrap().search.query = "sibling".into();
            app.update_search();
            assert!(!app.search_submitted);
            assert!(app.doc.as_ref().unwrap().search.error.is_some());
            assert!(Arc::get_mut(&mut app.doc.as_mut().unwrap().tree).is_some());
            shutdown(runtime);
        }
    }

    #[test]
    fn empty_invalid_query_clears_old_presentation() {
        let (mut app, runtime, wake) = running_app(|_| {});
        for query in ["", "size:>nonsense"] {
            app.doc.as_mut().unwrap().search.query = "file:".into();
            pump_until(&mut app, &wake, |app| {
                app.doc.as_ref().unwrap().search.result.is_some()
            });
            app.doc.as_mut().unwrap().search.query = query.into();
            app.update_search();
            let doc = app.doc.as_ref().unwrap();
            assert!(doc.search.result.is_none());
            assert!(doc.search.bits.is_empty());
            assert!(doc.search.applied.is_none());
            assert_eq!(doc.search.error.is_some(), !query.is_empty());
        }
        shutdown(runtime);
    }

    #[test]
    fn scope_and_mode_changes_invalidate_same_frame() {
        let (mut app, runtime, wake) = running_app(|_| {});
        app.doc.as_mut().unwrap().search.query = "file:".into();
        pump_until(&mut app, &wake, |app| {
            app.doc.as_ref().unwrap().search.result.is_some()
        });
        let generation = app.search_generation;
        let edited = app.doc.as_ref().unwrap().search.edited_at;
        let bits = app.doc.as_ref().unwrap().search.bits.as_ptr();
        app.doc.as_mut().unwrap().navigate(1);
        app.doc.as_mut().unwrap().search.highlight = false;
        app.update_search();
        assert_eq!(app.search_generation, generation);
        assert_eq!(app.doc.as_ref().unwrap().search.edited_at, edited);
        assert_eq!(app.doc.as_ref().unwrap().search.bits.as_ptr(), bits);
        app.doc.as_mut().unwrap().search.in_focus_only = true;
        app.observe_search_input();
        assert_eq!(app.search_generation, generation + 1);
        assert!(app.doc.as_ref().unwrap().search.result.is_none());
        pump_until(&mut app, &wake, |app| {
            app.doc.as_ref().unwrap().search.result.is_some()
        });
        app.mode = SizeMode::Logical;
        app.observe_search_input();
        assert!(app.doc.as_ref().unwrap().search.result.is_none());
        assert_eq!(app.search_generation, generation + 2);
        app.doc.as_mut().unwrap().go_up();
        app.search_pass(false);
        assert_eq!(app.search_generation, generation + 3);
        assert!(app.doc.as_ref().unwrap().search.bits.is_empty());
        shutdown(runtime);
    }

    fn install_scan(app: &mut App) -> mpsc::Sender<io::Result<ScanResult>> {
        app.suspend_search();
        let (tx, rx) = mpsc::channel();
        app.scanning = Some(Scanning {
            scan_id: 1,
            root: "C:\\Data".into(),
            progress: Arc::default(),
            rx,
            started: Instant::now(),
            used: None,
            eta: None,
            preview: None,
            preview_version: 0,
            preview_layout: Layout::stable(),
            last_preview: Instant::now(),
        });
        tx
    }

    #[test]
    fn scan_start_suspends_old_document_search() {
        let (started_tx, started) = mpsc::channel();
        let (release_tx, release) = mpsc::channel();
        let release = std::sync::Mutex::new(release);
        let (mut app, runtime, _wake) = running_app(move |_| {
            started_tx.send(()).unwrap();
            release
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(10))
                .unwrap();
        });
        app.doc.as_mut().unwrap().search.query = "file:".into();
        app.update_search();
        started.recv_timeout(Duration::from_secs(10)).unwrap();
        let _scan = install_scan(&mut app);
        app.doc.as_mut().unwrap().search.query = "sibling".into();
        app.update_search();
        assert!(!app.search_submitted);
        assert!(app.doc.as_ref().unwrap().search.result.is_none());
        release_tx.send(()).unwrap();
        shutdown(runtime);
        app.update_search();
        assert!(app.doc.as_ref().unwrap().search.result.is_none());
        assert!(!app.search_submitted);
    }

    #[test]
    fn scan_failure_resumes_current_query_once() {
        for disconnected in [false, true] {
            let (started_tx, started) = mpsc::channel();
            let (mut app, runtime, wake) = running_app(move |generation| {
                started_tx.send(generation).unwrap();
            });
            let scan = install_scan(&mut app);
            app.doc.as_mut().unwrap().search.query = "sibling".into();
            app.update_search();
            assert!(!app.search_submitted);
            if !disconnected {
                scan.send(Err(io::Error::other("injected scan failure")))
                    .unwrap();
            }
            drop(scan);
            app.poll_scan();
            pump_until(&mut app, &wake, |app| {
                app.doc.as_ref().unwrap().search.result.is_some()
            });
            assert_eq!(
                app.doc
                    .as_ref()
                    .unwrap()
                    .search
                    .result
                    .as_ref()
                    .unwrap()
                    .hits,
                [2]
            );
            let generation = started.recv_timeout(Duration::from_secs(10)).unwrap();
            for _ in 0..3 {
                app.update_search();
            }
            assert_eq!(app.search_generation, generation);
            assert!(started.try_recv().is_err());
            shutdown(runtime);
        }
    }
    #[cfg(feature = "perf")]
    fn enable_trace(app: &mut App) {
        let (trace, _runtime) = crate::perf_trace::PerfTraceRuntime::new("unused.csv".into());
        app.search.set_release_observer(trace.release_observer());
        app.perf_trace = Some(trace);
    }

    #[cfg(feature = "perf")]
    fn draw_search_frame(app: &mut App, reveal: Option<NodeId>) {
        let ctx = egui::Context::default();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1280.0, 820.0),
            )),
            ..Default::default()
        };
        app.perf_highlight_draw = None;
        let mut output = ctx.run_ui(input, |ui| {
            egui::Panel::right("trace-results").show(ui, |ui| {
                search_panel::results(ui, app.doc.as_mut().unwrap(), app.mode);
            });
            let table = app.perf_draw_identity();
            let count_before = app
                .perf_trace
                .as_ref()
                .unwrap()
                .samples("first_accepted_draw")
                .len();
            app.perf_record_draw(table.as_ref());
            assert_eq!(
                app.perf_trace
                    .as_ref()
                    .unwrap()
                    .samples("first_accepted_draw")
                    .len(),
                count_before,
                "table alone was counted before highlights painted"
            );
            if let Some(id) = reveal {
                app.doc.as_mut().unwrap().reveal(id);
            }
            app.search_pass(false);
            egui::CentralPanel::default().show(ui, |ui| app.treemap_view(ui));
            app.perf_record_draw(table.as_ref());
        });
        output.textures_delta.clear();
    }

    #[cfg(feature = "perf")]
    #[test]
    fn first_draw_requires_matching_table_and_painted_highlights() {
        for reveal in [None, Some(3)] {
            let (mut app, runtime, wake) = running_app(|_| {});
            enable_trace(&mut app);
            let doc = app.doc.as_mut().unwrap();
            doc.search.query = "file:".into();
            doc.search.in_focus_only = true;
            pump_until(&mut app, &wake, |app| {
                app.doc.as_ref().unwrap().search.result.is_some()
            });
            let generation = app.search_generation;
            draw_search_frame(&mut app, reveal);
            let samples = app
                .perf_trace
                .as_ref()
                .unwrap()
                .samples("first_accepted_draw");
            if reveal.is_none() {
                assert_eq!(samples.len(), 1);
                assert_eq!(samples[0].1, generation);
                assert!(app.perf_measurement.as_ref().unwrap().drawn);
                draw_search_frame(&mut app, None);
                assert_eq!(
                    app.perf_trace
                        .as_ref()
                        .unwrap()
                        .samples("first_accepted_draw")
                        .len(),
                    1
                );
            } else {
                assert!(
                    samples.is_empty(),
                    "scope-changed result never painted its highlights"
                );
                assert_eq!(app.search_generation, generation + 1);
                assert!(
                    app.perf_trace
                        .as_ref()
                        .unwrap()
                        .samples("superseded")
                        .iter()
                        .any(|sample| sample.1 == generation)
                );
            }
            shutdown(runtime);
        }
    }

    #[cfg(feature = "perf")]
    #[test]
    fn unchanged_query_after_scan_failure_has_new_trace_generation() {
        for drawn_before_scan in [false, true] {
            let (mut app, runtime, wake) = running_app(|_| {});
            enable_trace(&mut app);
            app.doc.as_mut().unwrap().search.query = "file:".into();
            pump_until(&mut app, &wake, |app| {
                app.doc.as_ref().unwrap().search.result.is_some()
            });
            let old_generation = app.search_generation;
            let old_key = app.search_key.clone().unwrap();
            if drawn_before_scan {
                draw_search_frame(&mut app, None);
            }
            let scan = install_scan(&mut app);
            assert!(
                app.perf_measurement.is_none(),
                "scan did not close old measurement"
            );
            app.update_search();
            assert_eq!(
                app.search_generation, old_generation,
                "suspended query acquired a request identity"
            );
            scan.send(Err(io::Error::other("injected scan failure")))
                .unwrap();
            let resumed_at = Instant::now();
            app.poll_scan();
            pump_until(&mut app, &wake, |app| {
                app.doc.as_ref().unwrap().search.result.is_some()
            });
            assert_eq!(app.search_key.as_ref(), Some(&old_key));
            let measurement = app.perf_measurement.as_ref().unwrap();
            assert_eq!(measurement.generation, app.search_generation);
            assert_eq!(measurement.generation, old_generation + 1);
            assert!(
                measurement.input_at >= resumed_at,
                "resumed sample includes scan suspension time"
            );
            assert!(!measurement.drawn);
            draw_search_frame(&mut app, None);
            let trace = app.perf_trace.as_ref().unwrap();
            let accepted = trace.samples("first_accepted_draw");
            assert_eq!(accepted.len(), if drawn_before_scan { 2 } else { 1 });
            assert_eq!(accepted.last().unwrap().1, old_generation + 1);
            assert_eq!(
                trace
                    .samples("superseded")
                    .iter()
                    .any(|sample| sample.1 == old_generation),
                !drawn_before_scan
            );
            assert_eq!(
                trace
                    .samples("input")
                    .iter()
                    .map(|sample| sample.1)
                    .collect::<Vec<_>>(),
                [old_generation, old_generation + 1]
            );
            shutdown(runtime);
        }
    }

    #[cfg(feature = "perf")]
    #[test]
    fn ordinary_invalidations_record_actual_lease_release() {
        let (started_tx, started) = mpsc::channel();
        let (release_tx, release) = mpsc::channel();
        let release = std::sync::Mutex::new(release);
        let (mut app, runtime, wake) = running_app(move |generation| {
            started_tx.send(generation).unwrap();
            release
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(10))
                .unwrap();
        });
        enable_trace(&mut app);
        app.doc.as_mut().unwrap().search.query = "file:".into();
        app.update_search();
        assert_eq!(started.recv_timeout(Duration::from_secs(10)).unwrap(), 1);
        app.mode = SizeMode::Logical;
        app.update_search();
        app.doc.as_mut().unwrap().search.in_focus_only = true;
        app.doc.as_mut().unwrap().navigate(1);
        app.update_search();
        app.doc.as_mut().unwrap().search.query = "child".into();
        app.update_search();
        let _scan = install_scan(&mut app);
        let released = app
            .perf_trace
            .as_ref()
            .unwrap()
            .samples("cancel_to_lease_release");
        assert_eq!(
            released.iter().map(|sample| sample.1).collect::<Vec<_>>(),
            [2, 3, 4]
        );
        assert_eq!(Arc::strong_count(&app.doc.as_ref().unwrap().tree), 2);
        release_tx.send(()).unwrap();
        pump_until(&mut app, &wake, |app| {
            app.perf_trace
                .as_ref()
                .unwrap()
                .samples("cancel_to_lease_release")
                .len()
                == 4
        });
        assert_eq!(Arc::strong_count(&app.doc.as_ref().unwrap().tree), 1);
        let released = app
            .perf_trace
            .as_ref()
            .unwrap()
            .samples("cancel_to_lease_release");
        assert_eq!(released.last().unwrap().1, 1);
        assert!(released.iter().all(|sample| sample.0 == 1));
        assert!(started.try_recv().is_err());
        shutdown(runtime);
    }

    fn preview_packet(scan_id: u64, version: u64, valid: bool) -> crate::preview_worker::PreviewPacket {
        crate::preview_worker::PreviewPacket {
            scan_id, version,
            tree: if valid { Arc::try_unwrap(fixture(1).tree).ok().unwrap() } else { Tree::default() },
            capture_time: Duration::from_micros(7),
            build_time: Duration::from_micros(11),
        }
    }

    fn preview_scan(app: &mut App) -> mpsc::Sender<io::Result<ScanResult>> {
        let (tx, rx) = mpsc::channel();
        app.begin_scan("C:\\Data".into(), Arc::default(), rx, None);
        tx
    }

    fn preview_live(progress: &ScanProgress, version: u64) {
        let mut live = progress.live.lock().unwrap();
        live.version = version;
        live.dirs = vec![darak_core::scan::LiveDir {
            parent: NO_NODE, depth: 0, name: "walk".into(), alloc: 100,
            logical: 100, files: 1, direct_alloc: 100, direct_logical: 100, direct_files: 1,
        }];
    }

    fn request_preview(app: &mut App) {
        app.scanning.as_mut().unwrap().last_preview = Instant::now() - Duration::from_secs(2);
        app.poll_preview();
    }

    fn gated_preview(app: &mut App) -> (crate::preview_worker::PreviewRuntime, mpsc::Receiver<()>, mpsc::Sender<()>) {
        let (started_tx, started) = mpsc::channel();
        let (release_tx, release) = mpsc::channel();
        let release = std::sync::Mutex::new(release);
        let (controller, runtime) = crate::preview_worker::PreviewRuntime::with_hook(Box::new(move |stage, _, _| {
            if stage == crate::preview_worker::Stage::Built {
                started_tx.send(()).unwrap();
                release.lock().unwrap().recv_timeout(Duration::from_secs(10)).unwrap();
            }
        }));
        app.preview = controller;
        (runtime, started, release_tx)
    }

    #[test]
    fn late_preview_cannot_replace_final_or_new_scan() {
        let mut app = app(fixture(1));
        let tx = preview_scan(&mut app);
        let first = app.scanning.as_ref().unwrap().scan_id;
        assert!(!app.accept_preview(preview_packet(first + 1, 99, false)));
        assert!(app.accept_preview(preview_packet(first, 2, true)));
        assert!(!app.accept_preview(preview_packet(first, 2, false)));
        assert!(!app.accept_preview(preview_packet(first, 1, false)));
        assert_eq!(app.scanning.as_ref().unwrap().preview_version, 2);
        let final_doc = fixture(9);
        tx.send(Ok(ScanResult { tree: Arc::try_unwrap(final_doc.tree).ok().unwrap(), stats: final_doc.stats })).unwrap();
        app.poll_scan();
        assert!(app.scanning.is_none());
        let final_id = app.doc.as_ref().unwrap().id;
        assert!(!app.accept_preview(preview_packet(first, 99, false)));
        assert_eq!(app.doc.as_ref().unwrap().id, final_id);
        let _next = preview_scan(&mut app);
        let second = app.scanning.as_ref().unwrap().scan_id;
        assert!(second > first);
        assert!(!app.accept_preview(preview_packet(first, 100, false)));
        assert!(app.scanning.as_ref().unwrap().preview.is_none());
        assert_eq!(app.scanning.as_ref().unwrap().preview_version, 0);
        assert!(app.accept_preview(preview_packet(second, 1, true)));
    }

    #[test]
    fn final_installs_while_preview_is_paused() {
        let mut app = app(fixture(1));
        let (runtime, started, release) = gated_preview(&mut app);
        let tx = preview_scan(&mut app);
        let progress = app.scanning.as_ref().unwrap().progress.clone();
        preview_live(&progress, 1);
        request_preview(&mut app);
        started.recv_timeout(Duration::from_secs(10)).unwrap();
        let final_doc = fixture(9);
        tx.send(Ok(ScanResult { tree: Arc::try_unwrap(final_doc.tree).ok().unwrap(), stats: final_doc.stats })).unwrap();
        app.poll_scan();
        app.poll_preview();
        assert!(app.scanning.is_none());
        assert_eq!(app.doc.as_ref().unwrap().id, 2);
        assert!(app.morph_until.is_some());
        release.send(()).unwrap();
        app.preview.wait_idle();
        app.poll_preview();
        assert_eq!(app.doc.as_ref().unwrap().id, 2);
        assert_eq!(Arc::strong_count(&progress), 1);
        runtime.shutdown_and_join();
    }

    #[test]
    fn empty_and_contended_preview_do_not_advance_installed_version() {
        let mut app = app(fixture(1));
        let (controller, runtime) = crate::preview_worker::PreviewRuntime::new();
        app.preview = controller;
        let _scan = preview_scan(&mut app);
        let progress = app.scanning.as_ref().unwrap().progress.clone();
        progress.total_hint.store(100, Relaxed); // MFT can have no live directories.
        progress.live.lock().unwrap().version = 5;
        request_preview(&mut app);
        app.preview.wait_idle();
        app.poll_preview();
        assert_eq!(app.scanning.as_ref().unwrap().preview_version, 0);
        preview_live(&progress, 5); // Walk fallback can publish that same version.
        let guard = progress.live.lock().unwrap();
        request_preview(&mut app);
        app.preview.wait_idle();
        app.poll_preview();
        assert_eq!(app.scanning.as_ref().unwrap().preview_version, 0);
        drop(guard);
        request_preview(&mut app);
        app.preview.wait_idle();
        assert_eq!(app.scanning.as_ref().unwrap().preview_version, 0);
        app.poll_preview();
        assert_eq!(app.scanning.as_ref().unwrap().preview_version, 5);
        assert_eq!(app.scanning.as_ref().unwrap().preview.as_ref().unwrap().name(0), "walk");
        runtime.shutdown_and_join();
    }

    #[test]
    fn cancel_stops_preview_requests_and_keeps_last_preview() {
        let mut app = app(fixture(1));
        let (runtime, started, release) = gated_preview(&mut app);
        let _scan = preview_scan(&mut app);
        let scan_id = app.scanning.as_ref().unwrap().scan_id;
        assert!(app.accept_preview(preview_packet(scan_id, 1, true)));
        let progress = app.scanning.as_ref().unwrap().progress.clone();
        preview_live(&progress, 2);
        request_preview(&mut app);
        started.recv_timeout(Duration::from_secs(10)).unwrap();
        app.cancel_scan();
        let last = Instant::now() - Duration::from_secs(2);
        app.scanning.as_mut().unwrap().last_preview = last;
        app.poll_preview();
        assert_eq!(app.scanning.as_ref().unwrap().last_preview, last);
        assert!(progress.is_cancelled());
        assert!(!app.accept_preview(preview_packet(scan_id, 3, false)));
        assert_eq!(app.scanning.as_ref().unwrap().preview_version, 1);
        assert!(app.scanning.as_ref().unwrap().preview.is_some());
        release.send(()).unwrap();
        app.preview.wait_idle();
        assert_eq!(Arc::strong_count(&progress), 2);
        runtime.shutdown_and_join();
    }

    #[test]
    fn scan_error_and_disconnect_invalidate_paused_preview() {
        for disconnected in [false, true] {
            let mut app = app(fixture(1));
            let (runtime, started, release) = gated_preview(&mut app);
            let tx = preview_scan(&mut app);
            let progress = app.scanning.as_ref().unwrap().progress.clone();
            preview_live(&progress, 1);
            request_preview(&mut app);
            started.recv_timeout(Duration::from_secs(10)).unwrap();
            if !disconnected { tx.send(Err(io::Error::other("scan failure"))).unwrap(); }
            drop(tx);
            app.poll_scan();
            assert!(app.scanning.is_none());
            assert!(app.error.is_some());
            release.send(()).unwrap();
            app.preview.wait_idle();
            assert!(app.preview.try_take_latest().is_none());
            assert_eq!(Arc::strong_count(&progress), 1);
            assert_eq!(app.doc.as_ref().unwrap().id, 1);
            runtime.shutdown_and_join();
        }
    }

    #[test]
    fn app_drop_signals_preview_without_joining_paused_worker() {
        let mut app = app(fixture(1));
        let (runtime, started, release) = gated_preview(&mut app);
        let _scan = preview_scan(&mut app);
        let progress = app.scanning.as_ref().unwrap().progress.clone();
        preview_live(&progress, 1);
        request_preview(&mut app);
        started.recv_timeout(Duration::from_secs(10)).unwrap();
        drop(app); // Must return before the explicit release, even with active work.
        assert!(progress.is_cancelled());
        release.send(()).unwrap();
        runtime.shutdown_and_join();
        assert_eq!(Arc::strong_count(&progress), 1);
    }


    #[test]
    fn both_render_paths_keep_settled_layout_and_reuse_morph_buffer() {
        for preview in [true, false] {
            let mut app = app(fixture(1));
            let ctx = egui::Context::default();
            let _scan = preview.then(|| preview_scan(&mut app));
            if preview {
                let id = app.scanning.as_ref().unwrap().scan_id;
                app.accept_preview(preview_packet(id, 1, true));
            } else {
                app.morph_until = Some(Instant::now() + Duration::from_secs(60));
            }
            let render = |app: &mut App| {
                let mut output = ctx.run_ui(egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(800.0, 600.0))),
                    ..Default::default()
                }, |ui| { egui::CentralPanel::default().show(ui, |ui| {
                    if preview { app.scan_view(ui); } else { app.treemap_view(ui); }
                }); });
                output.textures_delta.clear();
            };
            render(&mut app);
            let key = if let Some(scan) = &app.scanning {
                MorphKey { layout_id: LayoutSource::Preview(scan.scan_id), generation: scan.preview_layout.generation() }
            } else {
                MorphKey { layout_id: LayoutSource::Document(1), generation: app.layout.generation() }
            };
            assert!(app.morph.is_settled(key));
            let capacity = app.morph_items.capacity();
            assert!(capacity > 0);
            app.morph_items.clear();
            render(&mut app);
            assert!(app.morph_items.is_empty(), "settled drawing must not refill scratch output");
            assert!(app.morph.is_settled(key));
            assert_eq!(app.morph_items.capacity(), capacity);
            let _new = preview_scan(&mut app);
            assert!(!app.morph.is_settled(key));
            assert_eq!(app.morph_items.capacity(), capacity);
        }
    }

    #[test]
    fn settled_morph_borrows_fresh_layout_metadata_for_hit_testing() {
        let mut morph = Morph::default();
        let mut out = Vec::new();
        let mut layout = Layout::default();
        let doc = fixture(1);
        layout.update(&doc.tree, 1, 0, egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(800.0, 600.0)), SizeMode::Allocated, Detail::default());
        let key = MorphKey { layout_id: LayoutSource::Document(1), generation: layout.generation() };
        let (shown, moving) = morph_items(&mut morph, &layout.items, key, &mut out);
        assert!(!moving);
        assert_eq!(shown.as_ptr(), layout.items.as_ptr());
        assert!(morph.is_settled(key));
        // Same immutable layout geometry, refreshed metadata must come from Layout.
        let last = layout.items.last_mut().unwrap();
        last.node = 2;
        let pos = last.rect.center();
        out.clear();
        let (shown, moving) = morph_items(&mut morph, &layout.items, key, &mut out);
        assert!(!moving);
        assert_eq!(shown.as_ptr(), layout.items.as_ptr());
        assert_eq!(shown.iter().rev().find(|it| it.rect.contains(pos)).unwrap().node, 2);
        assert!(out.is_empty());
    }


    #[test]
    fn smaller_final_renders_only_current_nodes_after_preview_transition() {
        let mut app = app(fixture(1));
        let ctx = egui::Context::default();
        let tx = preview_scan(&mut app);
        let scan_id = app.scanning.as_ref().unwrap().scan_id;
        app.accept_preview(preview_packet(scan_id, 1, true));
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            egui::CentralPanel::default().show(ui, |ui| app.scan_view(ui));
        });
        output.textures_delta.clear();
        let preview_key = MorphKey { layout_id: LayoutSource::Preview(scan_id), generation: app.scanning.as_ref().unwrap().preview_layout.generation() };
        assert!(app.morph.is_settled(preview_key));
        let mut tree = Tree::default();
        node(&mut tree, "C:\\Data", NO_NODE, Kind::Dir, 40, 1, 1);
        node(&mut tree, "sibling", 0, Kind::File, 40, 0, 0); // Was preview NodeId 2.
        let final_doc = doc(9, tree);
        tx.send(Ok(ScanResult { tree: Arc::try_unwrap(final_doc.tree).ok().unwrap(), stats: final_doc.stats })).unwrap();
        app.poll_scan();
        assert!(!app.morph.is_settled(preview_key));
        frame(&ctx, &mut app, vec![], 0.1);
        let doc = app.doc.as_ref().unwrap();
        assert_eq!(doc.tree.len(), 2);
        for it in app.morph_items.iter().chain(&app.layout.items) {
            assert!(it.node < doc.tree.len() as u32);
            assert_eq!(doc.tree.name(it.node), if it.node == 0 { "C:\\Data" } else { "sibling" });
        }
        let pos = app.layout.items.last().unwrap().rect.center();
        frame(&ctx, &mut app, vec![egui::Event::PointerMoved(pos)], 0.2);
        assert!(app.doc.as_ref().unwrap().hovered.is_none_or(|id| id < 2));
        assert!(app.scanning.is_none());
    }


    #[cfg(feature = "perf")]
    #[test]
    fn preview_trace_records_only_accepted_capture_and_build() {
        let mut app = app(fixture(1));
        let (trace, _runtime) = crate::perf_trace::PerfTraceRuntime::new("unused-preview.csv".into());
        app.perf_trace = Some(trace);
        let _scan = preview_scan(&mut app);
        let scan_id = app.scanning.as_ref().unwrap().scan_id;
        assert!(!app.accept_preview(preview_packet(scan_id + 1, 9, false)));
        assert!(app.perf_trace.as_ref().unwrap().samples("capture").is_empty());
        assert!(app.accept_preview(preview_packet(scan_id, 3, true)));
        assert!(!app.accept_preview(preview_packet(scan_id, 3, false)));
        let trace = app.perf_trace.as_ref().unwrap();
        assert_eq!(trace.samples("capture"), vec![(scan_id, 3, Duration::from_micros(7))]);
        assert_eq!(trace.samples("build"), vec![(scan_id, 3, Duration::from_micros(11))]);
    }

    #[cfg(feature = "perf")]
    #[test]
    fn input_timestamp_survives_query_mode_scope_dispatch() {
        for input in ["query", "mode", "scope"] {
            let (mut app, runtime, wake) = running_app(|_| {});
            enable_trace(&mut app);
            app.doc.as_mut().unwrap().search.query = "file:".into();
            pump_until(&mut app, &wake, |app| {
                app.doc.as_ref().unwrap().search.result.is_some()
            });
            let generation = app.search_generation;
            // Model preceding UI/status work without sleeping. This exact event
            // timestamp must survive observation, invalidation and worker dispatch.
            let event_at = Instant::now() - Duration::from_secs(3);
            match input {
                "query" => app.doc.as_mut().unwrap().search.query = "child".into(),
                "mode" => app.mode = SizeMode::Logical,
                "scope" => {
                    let doc = app.doc.as_mut().unwrap();
                    doc.navigate(1);
                    doc.search.in_focus_only = true;
                }
                _ => unreachable!(),
            }
            app.doc.as_mut().unwrap().search.note_edit(event_at);
            app.update_search();
            assert_eq!(app.search_generation, generation + 1);
            assert_eq!(app.doc.as_ref().unwrap().search.edited_at, Some(event_at));
            assert_eq!(app.perf_measurement.as_ref().unwrap().input_at, event_at);
            pump_until(&mut app, &wake, |app| {
                app.doc.as_ref().unwrap().search.result.is_some()
            });
            // accept_search restores completion.edited_at, proving the request
            // carried the event timestamp through the actual worker as well.
            assert_eq!(app.doc.as_ref().unwrap().search.edited_at, Some(event_at));
            assert_eq!(app.perf_measurement.as_ref().unwrap().input_at, event_at);
            assert!(app.doc.as_ref().unwrap().search.pending_edit_at.is_none());
            let before_version_change = Instant::now();
            Arc::get_mut(&mut app.doc.as_mut().unwrap().tree)
                .unwrap()
                .version += 1;
            app.observe_search_input();
            assert!(
                app.doc.as_ref().unwrap().search.edited_at.unwrap() >= before_version_change,
                "non-input generation inherited a previous input timestamp"
            );
            shutdown(runtime);
        }
    }

    #[test]
    fn input_timestamp_tracks_only_effective_navigation_changes() {
        let mut app = app(fixture(1));
        app.doc.as_mut().unwrap().search.in_focus_only = true;
        app.observe_search_input();
        let navigation: [fn(&mut Doc); 5] = [
            |doc| doc.navigate(1),
            Doc::go_up,
            Doc::go_back,
            Doc::go_forward,
            |doc| doc.reveal(3),
        ];
        for navigate in navigation {
            let generation = app.search_generation;
            navigate(app.doc.as_mut().unwrap());
            let event_at = app
                .doc
                .as_ref()
                .unwrap()
                .search
                .pending_edit_at
                .expect("focus-only navigation did not stamp input");
            app.observe_search_input();
            assert_eq!(app.search_generation, generation + 1);
            assert_eq!(app.doc.as_ref().unwrap().search.edited_at, Some(event_at));
        }
        app.doc.as_mut().unwrap().search.in_focus_only = false;
        app.observe_search_input();
        let generation = app.search_generation;
        let event_at = app.doc.as_ref().unwrap().search.edited_at;
        app.doc.as_mut().unwrap().go_up();
        app.observe_search_input();
        assert_eq!(app.search_generation, generation);
        assert_eq!(app.doc.as_ref().unwrap().search.edited_at, event_at);
        assert!(app.doc.as_ref().unwrap().search.pending_edit_at.is_none());
    }

    #[cfg(feature = "perf")]
    #[test]
    fn input_timestamp_scan_resume_is_fresh_at_receipt() {
        for disconnected in [false, true] {
            let mut app = app(fixture(1));
            enable_trace(&mut app);
            let old_event = Instant::now() - Duration::from_secs(10);
            let doc = app.doc.as_mut().unwrap();
            doc.search.query = "file:".into();
            doc.search.note_edit(old_event);
            app.observe_search_input();
            let generation = app.search_generation;
            let scan = install_scan(&mut app);
            let suspended_edit = Instant::now() - Duration::from_secs(5);
            app.doc.as_mut().unwrap().search.note_edit(suspended_edit);
            if !disconnected {
                scan.send(Err(io::Error::other("injected scan failure")))
                    .unwrap();
            }
            drop(scan);
            let before_receipt = Instant::now();
            app.poll_scan();
            let resumed_at = app.doc.as_ref().unwrap().search.pending_edit_at.unwrap();
            assert!(resumed_at >= before_receipt);
            assert_ne!(resumed_at, suspended_edit);
            app.observe_search_input();
            assert_eq!(app.search_generation, generation + 1);
            assert_eq!(app.doc.as_ref().unwrap().search.edited_at, Some(resumed_at));
            assert_eq!(app.perf_measurement.as_ref().unwrap().input_at, resumed_at);
            app.observe_search_input();
            assert_eq!(app.search_generation, generation + 1);
            assert_eq!(app.perf_measurement.as_ref().unwrap().input_at, resumed_at);
        }
    }
}
