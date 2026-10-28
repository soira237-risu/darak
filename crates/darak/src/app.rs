use std::collections::BTreeSet;
use std::io;
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use darak_core::category::runs_code;
use darak_core::remove::Guard;
use darak_core::scan::{Drive, is_drive_root, is_elevated, list_drives, normalize_root};
use darak_core::sys::{DRIVE_FIXED, DRIVE_REMOTE, DRIVE_REMOVABLE};
use darak_core::{
    Fallback, Kind, NodeId, ScanMethod, ScanOptions, ScanProgress, ScanResult, ScanStats, SizeMode, Tree, flags,
    format_time, human_size, scan, thousands,
};
use eframe::egui::{self, Color32, Key, PointerButton, RichText};

use crate::delete::{self, Dialog, Stage};
use crate::doc::{Doc, RemovalApply};
use crate::drives::{Space, Volumes};
use crate::preview_worker::{PreviewController, PreviewPacket, PreviewRequest};
use crate::search_worker::{SearchController, SearchKey, SearchRequest, ReleaseFence, SearchControlEvent};
use darak_core::search::{Query, now_secs2000};
use crate::search_panel;
use crate::settings::{self, LangPref};
use crate::theme;
use darak_core::i18n::{plural, set_lang, tr};
use darak_core::tr;
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
    /// 드라이브 목록을 읽은 시각
    drives_at: Instant,
    /// 용량·파일 시스템 캐시 (UI 스레드는 장치에 묻지 않는다)
    volumes: Volumes,
    root_input: String,
    scanning: Option<Scanning>,
    doc: Option<Doc>,
    error: Option<String>,
    /// 잠깐 보여 줄 안내
    toast: Option<Toast>,
    mode: SizeMode,
    layout: Layout,
    wheel_acc: f32,
    context_node: Option<NodeId>,
    next_doc_id: u64,
    elevated: bool,
    delete_dialog: Option<Dialog>,
    /// 진행하기 전에 확인을 받는 중 (모달 하나를 같이 쓴다)
    confirm: Option<Confirm>,
    focus_search: bool,
    detail: Detail,
    anim: Option<ZoomAnim>,
    /// 애니메이션 판단용: 직전 프레임의 (문서 id, focus)
    last_focus: Option<(u64, NodeId)>,
    /// 미리 보기와 스캔 완료 직후의 모양 변화 보간
    morph: Morph,
    morph_items: Vec<Item>,
    morph_until: Option<Instant>,
    /// 화면 언어 고르기 (Windows 언어·한국어·영어)
    lang_pref: LangPref,
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

/// 환영 화면이 보이는 동안 드라이브 목록을 다시 읽는 간격 (나중에 꽂은 USB)
const DRIVE_LIST_TTL: Duration = Duration::from_secs(3);

/// 잠깐 보여 줄 안내. 되돌릴 수 있는 동작이면 '되돌리기' 버튼이 붙고 더 오래 보인다.
struct Toast {
    msg: String,
    at: Instant,
    undo: Option<ClearedMarks>,
}

/// 안내를 보여 주는 시간
const TOAST_FOR: Duration = Duration::from_millis(2500);
/// 되돌리기 버튼이 있는 안내를 보여 주는 시간
const UNDO_FOR: Duration = Duration::from_secs(6);

/// '표시 모두 해제'로 지운 표시. 같은 문서·같은 트리 버전이고 그 사이 새 표시가 없을 때만 되돌린다.
struct ClearedMarks {
    doc_id: u64,
    version: u64,
    marks: BTreeSet<NodeId>,
}

impl ClearedMarks {
    fn applies_to(&self, doc: &Doc) -> bool {
        doc.id == self.doc_id && doc.tree.version == self.version && doc.marks.is_empty()
    }
}

/// 삭제 표시를 잃는 동작을 하기 전에 묻는 내용. 모두 `confirm_ui` 하나로 그린다.
#[derive(Debug, PartialEq)]
enum Confirm {
    /// 관리자 권한으로 다시 열기 (새 창이 스캔할 루트, None이면 처음 화면)
    Relaunch(Option<String>),
    /// 새 스캔·다시 스캔 (스캔할 루트, normalize_root를 거친 값)
    Scan(String),
}

/// '관리자로 실행'이 지금 할 수 있는 일
#[derive(Debug, PartialEq)]
enum AdminRelaunch {
    /// 이미 관리자이거나 합성 문서라 보이지 않는다
    Hidden,
    /// 보이지만 누를 수 없다 (까닭)
    Blocked(&'static str),
    /// 누를 수 있다 (새 창이 스캔할 문서 루트, 문서가 없으면 None)
    Ready(Option<String>),
}

struct Scanning {
    scan_id: u64,
    root: String,
    progress: Arc<ScanProgress>,
    rx: mpsc::Receiver<io::Result<ScanResult>>,
    started: Instant,
    /// 진행률 기준 바이트: 같은 경로의 지난 일반 순회 크기, 드라이브 루트면 볼륨 사용량 (모르면 None)
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
    pub fn new(cc: &eframe::CreationContext, mut search: SearchController, mut preview: PreviewController, lang_pref: LangPref,
        #[cfg(feature = "perf")] fixture: Option<(Doc, String)>,
        #[cfg(feature = "perf")] trace: Option<crate::perf_trace::PerfTrace>,
    ) -> Self {
        crate::fonts::install(&cc.egui_ctx);
        crate::theme::install(&cc.egui_ctx);
        search.set_waker(cc.egui_ctx.clone());
        preview.set_waker(cc.egui_ctx.clone());
        #[cfg(feature = "perf")]
        if let Some(trace) = &trace { search.set_release_observer(trace.release_observer()); }
        let drives = list_drives();
        // 매체 없는 A:\나 카드 리더가 목록 앞에 올 수 있으므로 첫 고정 디스크
        let root_input = drives.iter().find(|d| d.kind == DRIVE_FIXED).map_or_else(|| "C:\\".into(), |d| d.root.clone());
        let mut app = Self {
            preview, next_scan_id: 1,
            search, search_key: None, search_generation: 0, search_submitted: false,
            search_failed: None, pending_removal: None, removal_released: false, next_fence: 0,
            drives,
            drives_at: Instant::now(),
            volumes: Volumes::new(Some(cc.egui_ctx.clone())),
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
            confirm: None,
            focus_search: false,
            detail: Detail::default(),
            anim: None,
            last_focus: None,
            morph: Morph::default(),
            morph_items: Vec::new(),
            morph_until: None,
            lang_pref,
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
        // 경로를 주면 바로 스캔한다: `darak.exe D:\`, 따옴표로 감싸면 끝의 \를 두 번 (`"D:\\"`, quote_arg).
        // UTF-16으로 온전하지 않은 인자에도 패닉하지 않는다.
        if let Some(arg) = std::env::args_os().nth(1) {
            app.start_scan(arg.to_string_lossy().into_owned());
        }
        app
    }

    /// 새 스캔을 시작할 수 없는 까닭 (스캔 시작 컨트롤의 툴팁). 시작할 수 있으면 None.
    fn scan_blocked(&self) -> Option<&'static str> {
        if self.scanning.is_some() {
            Some(tr("스캔 중에는 새로 시작할 수 없습니다 · 스캔 화면에서 먼저 중지하세요", "Can't start a new scan while scanning · Stop it on the scan screen first"))
        } else if self.delete_dialog.is_some() || self.pending_removal.is_some() {
            Some(tr("삭제를 마친 뒤에 스캔할 수 있습니다", "You can scan after the deletion finishes"))
        } else if self.confirm.is_some() {
            Some(tr("확인 창에 먼저 답하세요", "Answer the confirmation first"))
        } else {
            None
        }
    }

    fn can_start_scan(&self) -> bool {
        self.scan_blocked().is_none()
    }

    /// UI의 모든 스캔 진입점(드라이브·경로·폴더·환영 카드·다시 스캔). 삭제 표시가 있으면 먼저 묻는다.
    fn request_scan(&mut self, root: String) {
        if !self.can_start_scan() { return; }
        let root = normalize_root(&root);
        if self.doc.as_ref().is_some_and(|doc| !doc.marks.is_empty()) {
            self.confirm = Some(Confirm::Scan(root));
        } else {
            self.start_scan(root);
        }
    }

    /// 지금 문서의 루트를 다시 스캔한다 (↻·F5). 같은 루트라 끝나면 위치와 검색어를 이어 간다 (Doc::carry).
    fn rescan(&mut self) {
        if let Some(doc) = &self.doc {
            let root = doc.tree.name(Tree::ROOT).to_owned();
            self.request_scan(root);
        }
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
                    // 패닉하면 원인을 오류로 남긴다 (그냥 끝나면 '비정상 종료'만 알 수 있다)
                    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        scan(&root, &ScanOptions::default(), &progress)
                    }))
                    .unwrap_or_else(|p| {
                        let p = panic_text(&*p);
                        Err(io::Error::other(tr!("스캔 중 내부 오류: {p}", "Internal error during scan: {p}")))
                    });
                    let _ = tx.send(res);
                })
                .expect("스캔 스레드 생성 실패");
        }
        // 드라이브 루트의 볼륨 사용량은 장치에 물어야 하므로 fill_progress_basis가 캐시에서 채운다
        let used = previous_walk_size(self.doc.as_ref(), &root);
        self.begin_scan(root, progress, rx, used);
    }

    /// 드라이브 목록을 다시 읽는다 (장치에 묻지 않아 가볍다)
    fn refresh_drives(&mut self) {
        self.drives = list_drives();
        self.drives_at = Instant::now();
    }

    /// 드라이브 루트 스캔의 진행률 기준(볼륨 사용량)을 용량 캐시에서 채운다. 답이 오기 전에는 기준 없이 그린다.
    fn fill_progress_basis(&mut self) {
        let Some(s) = &mut self.scanning else { return };
        if s.used.is_none()
            && is_drive_root(&s.root)
            && let Space::Known(total, free) = self.volumes.space(&s.root)
        {
            s.used = Some(total - free);
        }
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
        // 중지한 스캔의 일부 결과는 크기가 실제보다 작으므로 검토하지 않는다
        if self.doc.as_ref().is_some_and(|doc| !filesystem_actions_enabled(doc) || doc.stats.cancelled) { return; }
        if self.scanning.is_some() || self.delete_dialog.is_some() || self.pending_removal.is_some() {
            return;
        }
        if let Some(doc) = &self.doc
            && !doc.marks.is_empty()
        {
            self.delete_dialog = Some(Dialog::review(doc));
        }
    }

    /// 스캔 결과를 받는다. 사용자가 중지하지 않은 스캔이 이번 프레임에 끝났으면(성공·실패) true.
    ///
    /// 중지했고 이전 문서가 있으면 결과를 기다리지 않고 바로 이전 문서(위치·삭제 표시 그대로)로 돌아간다.
    /// 받는 쪽(rx)을 버리므로 스캔 스레드가 나중에 보내는 부분 결과는 들어올 수 없다. 이 복원 경로는 이전 문서를
    /// 그대로 쓰므로 보호 수준(Guard)을 다시 정하지 않고, 최근 경로에도 기록하지 않으며, false를 돌려준다.
    /// 검색 재개는 다른 경로와 같이 끝의 공용 처리가 맡는다.
    ///
    /// 이전 문서가 없으면(첫 스캔) 읽은 데까지의 결과를 문서로 만든다(stats.cancelled, 배너·⚠, 삭제 표시 불가).
    /// 하나도 읽지 못했거나 MFT처럼 부분 결과가 없으면 환영 화면으로 돌아가고 안내만 띄운다.
    fn poll_scan(&mut self) -> bool {
        let Some(s) = &self.scanning else { return false };
        let stopped = s.progress.is_cancelled();
        let mut finished = false;
        let resumed_at = Instant::now();
        if stopped && let Some(doc) = &self.doc {
            self.preview.invalidate(s.scan_id);
            self.morph.invalidate();
            self.root_input = doc.tree.name(Tree::ROOT).to_owned();
            self.scanning = None;
            self.toast(tr("스캔을 중지했습니다 · 이전 결과를 그대로 보여 줍니다", "Scan stopped · Showing the previous results"));
        } else {
            let result = s.rx.try_recv();
            if !matches!(result, Err(mpsc::TryRecvError::Empty)) {
                self.preview.invalidate(s.scan_id);
                self.morph.invalidate();
            }
            match result {
                Ok(Ok(res)) if stopped && res.tree.node(Tree::ROOT).child_count == 0 => {
                    self.scanning = None;
                    self.toast(tr("스캔을 중지했습니다", "Scan stopped"));
                }
                Ok(Err(_)) if stopped => {
                    self.scanning = None;
                    self.toast(tr("스캔을 중지했습니다", "Scan stopped"));
                }
                Ok(Ok(res)) => {
                    // 같은 루트를 다시 스캔했으면 보던 위치와 검색 설정을 이어 간다
                    let carry = self.doc.as_ref()
                        .filter(|d| d.tree.name(Tree::ROOT).eq_ignore_ascii_case(res.tree.name(Tree::ROOT)))
                        .map(Doc::carry);
                    self.suspend_search();
                    self.context_node = None;
                    self.anim = None;
                    self.last_focus = None;
                    let mut doc = Doc::new(self.next_doc_id, res.tree, res.stats);
                    if self.elevated {
                        doc.guard = Guard::Admin;
                    }
                    if let Some(c) = carry {
                        doc.apply_carry(c);
                    }
                    // 미리 보기는 루트 기준이라 하위 폴더로 돌아가면 모양 보간이 어색하다
                    self.morph_until = (doc.focus == Tree::ROOT).then(|| Instant::now() + Duration::from_millis(1500));
                    self.doc = Some(doc);
                    self.next_doc_id += 1;
                    self.scanning = None;
                    self.layout.invalidate();
                    finished = !stopped;
                    // 스캔이 쓴 만큼 여유 공간도 바뀌었을 수 있다
                    self.volumes.mark_stale();
                }
                Ok(Err(e)) => {
                    self.error = Some(format!("{}: {e}", s.root));
                    self.scanning = None;
                    finished = true;
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.error = Some(tr("스캔 스레드가 비정상 종료했습니다", "The scan thread exited unexpectedly").into());
                    self.scanning = None;
                    finished = true;
                }
            }
            // 스캔하는 동안 드라이브를 꽂거나 뺐을 수 있다 (목록은 장치에 묻지 않아 가볍다)
            if self.scanning.is_none() {
                self.refresh_drives();
            }
        }
        // Fresh resume event excludes suspension but includes subsequent UI work.
        if self.scanning.is_none() && let Some(doc) = &mut self.doc {
            doc.search.note_edit(resumed_at);
        }
        finished
    }

    fn toast(&mut self, msg: impl Into<String>) {
        self.toast = Some(Toast { msg: msg.into(), at: Instant::now(), undo: None });
    }

    fn toggle_mark(&mut self, id: NodeId) {
        if self.pending_removal.is_some() { return; }
        let Some(doc) = &mut self.doc else { return };
        match doc.toggle_mark(id) {
            Ok(0) => {}
            // 상위 폴더 표시가 안쪽 표시를 흡수했다 (상위 표시를 풀어도 돌아오지 않으므로 알린다)
            Ok(n) => {
                let (k, marks) = (thousands(n as u64), plural(n as u64, "mark", "marks"));
                self.toast(tr!("안쪽 표시 {k}개를 이 폴더 표시에 합쳤습니다", "Merged {k} inner {marks} into this folder's mark"));
            }
            Err(e) => self.toast(e),
        }
    }

    /// 표시를 모두 지우고, 6초 동안 되돌릴 수 있는 안내를 띄운다.
    // TODO: 검토 창 왼쪽 아래의 '목록 비우기'도 이 함수를 부른다
    fn clear_marks(&mut self) {
        if self.pending_removal.is_some() { return; }
        let Some(doc) = &mut self.doc else { return };
        if doc.marks.is_empty() { return; }
        let undo = ClearedMarks { doc_id: doc.id, version: doc.tree.version, marks: std::mem::take(&mut doc.marks) };
        doc.clear_marks();
        let (k, marks) = (thousands(undo.marks.len() as u64), plural(undo.marks.len() as u64, "mark", "marks"));
        self.toast = Some(Toast {
            msg: tr!("표시 {k}개를 모두 해제했습니다", "Cleared {k} {marks}"),
            at: Instant::now(),
            undo: Some(undo),
        });
    }

    /// 안내의 '되돌리기': 지운 표시를 다시 놓는다. 문서나 트리가 바뀌었거나 새 표시가 있으면 하지 않는다.
    /// 스캔 중에도 하지 않는다 (숨은 문서에 표시가 생기면 스캔이 끝날 때 묻지 않고 사라진다).
    fn undo_clear(&mut self) {
        if self.scanning.is_some() { return; }
        let Some(Toast { undo: Some(cleared), .. }) = self.toast.take() else { return };
        if self.pending_removal.is_none()
            && let Some(doc) = &mut self.doc
            && cleared.applies_to(doc)
        {
            doc.restore_marks(cleared.marks);
        }
    }

    /// 파일을 연다. 열지 않는 파일이면 탐색기에서 위치를 보여 주고 까닭을 알린다 (더블클릭·Enter·검색 결과).
    fn open_file(&mut self, id: NodeId) {
        let Some(doc) = &self.doc else { return };
        if let Some(note) = open_item(doc, id, self.elevated) {
            self.toast(note);
        }
    }

    /// '관리자로 실행'을 보일지, 누를 수 있는지, 새 창에 무엇을 넘길지.
    /// 넘기는 루트는 경로 칸이 아니라 화면에 있는 문서의 루트다.
    fn admin_relaunch(&self) -> AdminRelaunch {
        if self.elevated || self.doc.as_ref().is_some_and(|doc| !filesystem_actions_enabled(doc)) {
            return AdminRelaunch::Hidden;
        }
        if self.scanning.is_some() {
            return AdminRelaunch::Blocked(tr("스캔을 중지하거나 끝난 뒤에 다시 실행할 수 있습니다", "You can relaunch after the scan stops or finishes"));
        }
        if self.delete_dialog.is_some() || self.pending_removal.is_some() {
            return AdminRelaunch::Blocked(tr("삭제를 마친 뒤에 다시 실행할 수 있습니다", "You can relaunch after the deletion finishes"));
        }
        let Some(doc) = &self.doc else { return AdminRelaunch::Ready(None) };
        let root = doc.tree.name(Tree::ROOT);
        if is_network(root, &self.drives) {
            return AdminRelaunch::Blocked(
                tr(
                    "네트워크 위치는 관리자 권한으로 빨라지지 않고, 관리자 창에서는 연결된 드라이브가 보이지 않을 수 있습니다",
                    "Administrator rights don't speed up network locations, and mapped drives may not show in an administrator window",
                ),
            );
        }
        AdminRelaunch::Ready(Some(root.to_owned()))
    }

    /// 관리자 권한으로 다시 연다. 삭제 표시가 있으면 먼저 묻고, 없으면 UAC 창이 확인 역할을 한다.
    fn request_relaunch(&mut self, ctx: &egui::Context, root: Option<String>) {
        if self.doc.as_ref().is_some_and(|doc| !doc.marks.is_empty()) {
            self.confirm = Some(Confirm::Relaunch(root));
        } else {
            self.relaunch(ctx, root);
        }
    }

    fn relaunch(&mut self, ctx: &egui::Context, root: Option<String>) {
        if shell(Shell::Elevate(root)) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        } else {
            // UAC 창에서 취소한 경우도 여기로 온다
            self.toast(tr("관리자 권한으로 다시 열지 않았습니다", "Didn't relaunch as administrator"));
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
            // 상태 바·카드의 여유 공간을 다시 묻는다
            self.volumes.mark_stale();
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
    /// ui보다 먼저, 그리고 창이 최소화되거나 가려져 ui를 그리지 않을 때도 불린다
    /// (작업 표시줄에서 닫기를 눌러도 삭제 중이면 막는다).
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.guard_close(ctx);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.show(ui);
    }
}

impl App {
    /// 한 프레임 전체. eframe::Frame을 쓰지 않으므로 헤드리스 테스트도 이 함수를 그대로 부른다.
    fn show(&mut self, ui: &mut egui::Ui) {
        #[cfg(feature = "perf")]
        let frame_started = Instant::now();
        let ctx = ui.ctx().clone();
        #[cfg(feature = "perf")]
        { self.perf_highlight_draw = None; }
        #[cfg(feature = "perf")]
        let mut table_draw = None;
        self.volumes.poll();
        // TODO: 끝났으면 창이 비활성일 때 작업 표시줄로 알리고 창 제목을 바꾼다
        self.poll_scan();
        self.poll_preview();
        self.fill_progress_basis();
        if self.scanning.is_some() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }

        // 보드 바닥줄·머리줄 여백: 툴바 12×8, 상태 바 12×6
        let panel = theme::of(ui).surface_panel;
        let bar = |x: i8, y: i8| egui::Frame::new().fill(panel).inner_margin(egui::Margin::symmetric(x, y));
        egui::Panel::top("toolbar").frame(bar(12, 8)).show(ui, |ui| self.toolbar(ui));
        egui::Panel::bottom("status").frame(bar(12, 6)).show(ui, |ui| self.status_bar(ui));

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
            let mut open = None;
            if let Some(doc) = &mut self.doc {
                match action {
                    search_panel::Action::Reveal(id) => doc.reveal(id),
                    search_panel::Action::Context(id) => doc.reveal(id),
                    search_panel::Action::Open(id) => {
                        doc.reveal(id);
                        if doc.tree.node(id).kind == Kind::File {
                            open = Some(id);
                        }
                    }
                    search_panel::Action::None => {}
                }
            }
            if let Some(id) = open {
                self.open_file(id);
            }
        }

        // Result clicks can change a focus-only scope before the treemap draws.
        if show_results { self.search_pass(false); } else { self.update_search(); }
        let bg = theme::of(ui).bg_app;
        // 트리맵 둘레 8 (캔버스가 안쪽으로 2를 더 줄인다)
        egui::CentralPanel::default().frame(egui::Frame::NONE.fill(bg).inner_margin(6)).show(ui, |ui| {
            if self.scanning.is_some() {
                self.scan_view(ui);
            } else if self.doc.is_some() {
                self.partial_notice(ui);
                ui.add_enabled_ui(self.pending_removal.is_none(), |ui| self.treemap_view(ui));
            } else {
                self.welcome(ui);
            }
        });

        #[cfg(feature = "perf")]
        self.perf_record_draw(table_draw.as_ref());
        self.delete_dialog_ui(&ctx);
        self.confirm_ui(&ctx);
        self.toast_ui(&ctx);
        self.keyboard(&ctx);
        // Tab으로 옮긴 포커스에 앰버 링 (egui 기본 위젯 몫)
        theme::paint_keyboard_focus(&ctx);
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
        // 툴바 컨트롤은 높이 28 (보드 「컨트롤 높이」)
        theme::control_height(ui, theme::H_TOOLBAR);
        let (mut review, mut clear, mut rescan) = (false, false, false);
        // 스캔 중에는 스캔 시작 컨트롤을 모두 끄고 까닭을 툴팁으로 알린다 (중지는 스캔 화면에만 있다)
        let blocked = self.scan_blocked();
        let can_scan = blocked.is_none();
        let why = |r: egui::Response| match blocked {
            Some(w) => r.on_disabled_hover_text(w),
            None => r,
        };
        // theme 버튼은 꺼져도 hover를 받으므로 on_hover_text로 붙인다
        let why_hover = |r: egui::Response| match blocked {
            Some(w) => r.on_hover_text(w),
            None => r,
        };
        let t = theme::of(ui);
        ui.horizontal(|ui| {
            let mut chosen = None;
            // 드라이브 콤보: 폭 90, 높이 28 (App 보드)
            why(ui.add_enabled_ui(can_scan, |ui| egui::ComboBox::from_id_salt("drive").selected_text(tr("드라이브", "Drive")).width(90.0).show_ui(ui, |ui| {
                for d in &self.drives {
                    // 용량은 캐시에서 읽는다 (환영 카드와 같은 규칙으로 매체 없는 이동식은 숨긴다)
                    let space = self.volumes.space(&d.root);
                    if !shows_drive(d.kind, space) {
                        continue;
                    }
                    let txt = format!("{}  {}  ({})", d.root, d.kind_label(), space_note(space));
                    if ui.selectable_label(false, txt).clicked() {
                        chosen = Some(d.root.clone());
                    }
                }
            }).response).inner);
            if let Some(r) = chosen {
                self.request_scan(r);
            }

            let hint = theme::hint(ui, tr("스캔할 경로", "Path to scan"));
            // 바깥 폭 260 = 글자 칸 240 + 좌우 10
            let path = egui::TextEdit::singleline(&mut self.root_input)
                .desired_width(240.0)
                .min_size(egui::vec2(0.0, theme::H_TOOLBAR))
                .margin(egui::Margin::symmetric(10, 4))
                .hint_text(hint);
            let edit = why(ui.add_enabled(can_scan, path));
            theme::focus_ring(ui, &edit);
            // Enter는 여기서 쓰고 지운다 (keyboard()가 선택한 항목을 열거나 확대하지 않게)
            let enter = edit.lost_focus() && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Enter));

            if why_hover(theme::button(ui, can_scan, tr("📁 폴더…", "📁 Folder…"))).clicked()
                && let Some(dir) = rfd::FileDialog::new().pick_folder() {
                    self.request_scan(dir.display().to_string());
                }
            // 화면의 강조 버튼 하나
            if why_hover(theme::primary_button(ui, can_scan, tr("🔍 스캔", "🔍 Scan"))).clicked() || (enter && can_scan) {
                self.request_scan(self.root_input.clone());
            }

            // 상태는 admin_relaunch()가 정한다
            let relaunch = tr("🛡 관리자로 실행", "🛡 Run as administrator");
            match self.admin_relaunch() {
                AdminRelaunch::Hidden => {}
                AdminRelaunch::Blocked(why) => {
                    theme::button(ui, false, relaunch).on_hover_text(why);
                }
                AdminRelaunch::Ready(root) => {
                    if theme::button(ui, true, relaunch).on_hover_text(relaunch_hint(root.as_deref())).clicked() {
                        self.request_relaunch(ui.ctx(), root);
                    }
                }
            }

            divider(ui, 1.0);
            let before = self.mode;
            theme::segments(ui, |ui| {
                let alloc = theme::segment(ui, self.mode == SizeMode::Allocated, tr("할당 크기", "Allocated size"))
                    .on_hover_text(tr("디스크에서 실제로 차지하는 크기 (삭제하면 비는 공간)", "Space actually used on disk (freed when deleted)"));
                if alloc.clicked() {
                    self.mode = SizeMode::Allocated;
                }
                let logical = theme::segment(ui, self.mode == SizeMode::Logical, tr("논리 크기", "Logical size"))
                    .on_hover_text(tr("파일 내용의 바이트 수", "Bytes of file content"));
                if logical.clicked() {
                    self.mode = SizeMode::Logical;
                }
            });
            if before != self.mode {
                if let Some(doc) = &mut self.doc { doc.search.note_edit(Instant::now()); }
                self.layout.invalidate();
            }
            divider(ui, 1.0);
            // 상세도: 24 고스트 −/+ 사이에 12px 글자, 간격 2
            ui.scope(|ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                if theme::icon_button(ui, self.detail.level() > Detail::MIN, "－", theme::H_SMALL)
                    .on_hover_text(tr("덜 자세히: 큰 박스만 ([ 또는 -)", "Less detail: large boxes only ([ or -)"))
                    .clicked()
                {
                    self.detail = self.detail.less();
                }
                let (level, max) = (self.detail.level(), Detail::MAX);
                ui.label(theme::caption(tr!("상세도 {level}/{max}", "Detail {level}/{max}")).color(t.text_secondary));
                if theme::icon_button(ui, self.detail.level() < Detail::MAX, "＋", theme::H_SMALL)
                    .on_hover_text(tr("더 자세히: 작은 폴더와 파일까지 (] 또는 +)", "More detail: down to small folders and files (] or +)"))
                    .clicked()
                {
                    self.detail = self.detail.more();
                }
            });

            // 스캔 중에는 이전 문서의 검색 칸과 표시 묶음을 숨긴다 (보이지 않는 문서를 고치지 않게)
            if self.scanning.is_none() && let Some(doc) = &mut self.doc {
                divider(ui, 1.0);
                // 바깥 폭 240 = 글자 칸 220 + 좌우 10, 본문 글꼴 (App 보드)
                let hint = theme::hint(ui, tr("🔎 검색  (예: *.mp4 size:>1gb)", "🔎 Search  (e.g. *.mp4 size:>1gb)"));
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut doc.search.query)
                        .desired_width(220.0)
                        .min_size(egui::vec2(0.0, theme::H_TOOLBAR))
                        .margin(egui::Margin::symmetric(10, 4))
                        .hint_text(hint),
                );
                theme::focus_ring(ui, &resp);
                // 비어 있고 포커스가 없으면 오른쪽에 `/` 키 (누르면 검색 칸으로 간다)
                if doc.search.query.is_empty() && !resp.has_focus() {
                    let g = ui.painter().layout_no_wrap("/".into(), egui::FontId::monospace(11.0), t.text_secondary);
                    // kbd: 11px mono, 안쪽 1×6, 1px line.strong, 모서리 3, 오른쪽 여백 10
                    let size = g.size() + egui::vec2(12.0, 2.0);
                    let r = egui::Rect::from_min_size(egui::pos2(resp.rect.right() - 10.0 - size.x, resp.rect.center().y - size.y / 2.0), size);
                    ui.painter().rect_stroke(r, theme::R_CHIP, egui::Stroke::new(1.0, t.line_strong), egui::StrokeKind::Inside);
                    ui.painter().galley(r.center() - g.size() / 2.0, g, t.text_secondary);
                }
                if self.focus_search {
                    resp.request_focus();
                    self.focus_search = false;
                }
                if resp.changed() {
                    doc.search.note_edit(Instant::now());
                }
                // 입력을 끝내는 Enter는 검색 칸 몫이다 (keyboard()가 선택한 파일을 열지 않게)
                if resp.lost_focus() {
                    ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Enter));
                }
                if resp.has_focus() && ui.input(|i| i.key_pressed(Key::Escape)) && !doc.search.query.is_empty() {
                    doc.search.note_edit(Instant::now());
                    doc.search.query.clear();
                }
                resp.on_hover_ui(search_help);

                if !doc.marks.is_empty() {
                    divider(ui, 1.0);
                    let (n, size) = (doc.marks.len(), human_size(doc.marked_size(self.mode)));
                    let txt = tr!("🗑 표시 {n}개 · {size}  검토…", "🗑 {n} marked · {size}  Review…");
                    let enabled = filesystem_actions_enabled(doc) && self.scanning.is_none() && self.delete_dialog.is_none() && self.pending_removal.is_none();
                    // 위험 버튼 (App 보드: danger.fill · on.danger 600, 높이 28)
                    if theme::danger_button(ui, enabled, txt).clicked() {
                        review = true;
                    }
                    // 표시 모두 해제: 보조, 높이 24, 좌우 8, 12px
                    let can_clear = self.pending_removal.is_none();
                    let clear_btn = ui.scope(|ui| {
                        theme::control_height(ui, theme::H_SMALL);
                        ui.spacing_mut().button_padding.x = 8.0;
                        ui.add_enabled(can_clear, egui::Button::new(theme::caption(tr("표시 모두 해제", "Clear all marks"))))
                    });
                    if clear_btn.inner.clicked() {
                        clear = true;
                    }
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| self.language_picker(ui));
        });
        if review {
            self.open_delete_review();
        }
        if clear {
            self.clear_marks();
        }

        // 둘째 줄(이동·경로)도 스캔 중에는 숨긴다. 중지하면 그대로 돌아온다
        if self.scanning.is_none() && let Some(doc) = &mut self.doc {
            ui.horizontal(|ui| {
                // 고스트 아이콘 28, 간격 2 (보드 「이동 경로」)
                ui.spacing_mut().item_spacing.x = 2.0;
                let nav = theme::H_TOOLBAR;
                if theme::icon_button(ui, !doc.back.is_empty(), "⬅", nav).on_hover_text(tr("뒤로 (Alt+←)", "Back (Alt+←)")).clicked() {
                    doc.go_back();
                }
                if theme::icon_button(ui, !doc.forward.is_empty(), "➡", nav).on_hover_text(tr("앞으로 (Alt+→)", "Forward (Alt+→)")).clicked() {
                    doc.go_forward();
                }
                if theme::icon_button(ui, doc.focus != Tree::ROOT, "⬆", nav).on_hover_text(tr("상위 폴더 (Alt+↑)", "Up one level (Alt+↑)")).clicked() {
                    doc.go_up();
                }
                rescan = theme::icon_button(ui, can_scan, "↻", nav).on_hover_text(blocked.unwrap_or(tr("다시 스캔 (F5)", "Rescan (F5)"))).clicked();
                // 구분선 좌우 6, 경로 항목 사이 4 (컴포넌트 보드 「이동 경로」)
                ui.spacing_mut().item_spacing.x = 4.0;
                divider(ui, 13.0);
                let chain = doc.tree.ancestry(doc.focus);
                let mut go = None;
                for (i, &n) in chain.iter().enumerate() {
                    if i > 0 {
                        theme::crumb_separator(ui);
                    }
                    let name = doc.tree.name(n);
                    // 현재 위치는 600 + 앰버 밑줄, 누를 수 없다 (다 세지 못했으면 아래 크기 옆에 ⚠)
                    if i + 1 == chain.len() {
                        theme::crumb_current(ui, name);
                        continue;
                    }
                    // 크기를 다 세지 못한 조상에도 ⚠
                    let crumb = match incomplete_note(doc, n) {
                        Some(note) => crumb_warn(ui, name).on_hover_text(note),
                        None => theme::crumb(ui, name),
                    };
                    if crumb.clicked() {
                        go = Some(n);
                    }
                }
                if let Some(n) = go {
                    doc.navigate(n);
                }
                // 현재 위치의 크기 (text.tertiary, ⚠만 warn.text)
                let f = doc.tree.node(doc.focus);
                let note = incomplete_note(doc, doc.focus);
                let (files, fw) = (thousands(f.files as u64), plural(f.files as u64, "file", "files"));
                let font = egui::FontId::proportional(theme::BODY);
                let mut job = egui::text::LayoutJob::default();
                job.append(&human_size(f.size(self.mode)), 0.0, egui::TextFormat::simple(font.clone(), t.text_tertiary));
                if note.is_some() {
                    job.append(" ⚠", 0.0, egui::TextFormat::simple(font.clone(), t.warn_text));
                }
                job.append(&tr!(" · {files} 파일", " · {files} {fw}"), 0.0, egui::TextFormat::simple(font, t.text_tertiary));
                ui.add_space(8.0);
                let size = ui.label(job);
                if let Some(note) = note {
                    size.on_hover_text(note);
                }
            });
        }
        if rescan {
            self.rescan();
        }
    }

    /// 툴바 오른쪽 끝의 화면 언어. 바꾸면 바로 다시 그리고, 고른 값만 settings.toml에 남긴다.
    fn language_picker(&mut self, ui: &mut egui::Ui) {
        let before = self.lang_pref;
        egui::ComboBox::from_id_salt("language")
            .selected_text(format!("🌐 {}", self.lang_pref.label()))
            .show_ui(ui, |ui| {
                for pref in LangPref::ALL {
                    ui.selectable_value(&mut self.lang_pref, pref, pref.label());
                }
            })
            .response
            .on_hover_text(tr("화면 언어", "Display language"));
        if self.lang_pref == before {
            return;
        }
        set_lang(self.lang_pref.resolve());
        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Title(window_title(self.elevated).into()));
        // 트리맵의 묶음 이름처럼 레이아웃에 들어간 문구도 새 언어로
        self.layout.invalidate();
        if let Err(e) = settings::save(self.lang_pref) {
            self.toast(tr!("언어 설정을 저장하지 못했습니다: {e}", "Couldn't save the language setting: {e}"));
        }
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        if self.pending_removal.is_some() {
            ui.ctx().request_repaint_after(Duration::from_millis(50));
        }
        let mut rescan = false;
        let t = theme::of(ui);
        ui.horizontal(|ui| {
            // 상태 바는 caption 12 · text.secondary, 항목 사이 14, 세로줄 없음 (App 보드 바닥줄)
            ui.style_mut().override_text_style = Some(egui::TextStyle::Small);
            ui.visuals_mut().override_text_color = Some(t.text_secondary);
            ui.spacing_mut().item_spacing.x = 14.0;
            // 아래의 조기 반환보다 앞이라 오류·스캔 중·문서 없음 상태에서도 보인다
            if self.elevated {
                theme::badge(ui, tr("관리자 권한", "Administrator")).on_hover_text(tr(
                    "관리자 권한으로 실행 중입니다. 파일은 직접 열지 않고 탐색기에서 위치를 보여 주며, \
                     설치된 프로그램 폴더와 다른 사용자의 프로필 기본 폴더는 삭제 표시할 수 없습니다",
                    "Running as administrator. Files are shown in File Explorer instead of being opened, and \
                     installed programs folders and other users' profile default folders can't be marked for deletion",
                ));
            }
            if let Some(e) = &self.error {
                ui.colored_label(t.danger_text, format!("⚠ {e}"));
                return;
            }
            if self.pending_removal.is_some() {
                ui.label(tr("삭제 결과를 반영하는 중입니다…", "Applying the deletion results…"));
                return;
            }
            // 진행 애니메이션은 스캔 화면의 막대 하나만 둔다
            if let Some(s) = &self.scanning {
                if s.progress.is_cancelled() {
                    ui.label(tr("중지하는 중…", "Stopping…"));
                } else {
                    let (root, secs) = (&s.root, s.started.elapsed().as_secs_f32());
                    ui.label(tr!("{root} 스캔 중… {secs:.1}s", "Scanning {root}… {secs:.1}s"));
                }
                // 스캔하는 동안 이전 문서를 들고 있다는 것을 알린다
                if let Some(doc) = &self.doc {
                    let root = doc.tree.name(Tree::ROOT);
                    ui.colored_label(t.text_tertiary, tr!("· 중지하면 {root}의 이전 결과로 돌아갑니다", "· Stopping returns to the previous results for {root}"));
                }
                return;
            }
            let Some(doc) = &self.doc else {
                ui.label(tr("드라이브나 폴더를 골라 스캔하세요.", "Pick a drive or folder to scan."));
                return;
            };
            // 첫 항목은 루트 (상태 바에서 하나만 text.primary)
            ui.label(RichText::new(doc.tree.name(Tree::ROOT)).color(t.text_primary));
            let st = &doc.stats;
            let (files, dirs, secs) = (thousands(st.files), thousands(st.dirs), st.elapsed.as_secs_f64());
            let (fw, dw) = (plural(st.files, "file", "files"), plural(st.dirs, "folder", "folders"));
            ui.label(tr!("{files} 파일 · {dirs} 폴더 · {secs:.1}s", "{files} {fw} · {dirs} {dw} · {secs:.1}s"));
            // 일반 스캔으로 바꾼 이유는 호버하지 않아도 배지에 보인다
            let (badge, hint) = method_badge(st);
            theme::badge(ui, badge).on_hover_text(hint);
            let when = scanned_label(doc.scanned_at);
            ui.colored_label(t.text_tertiary, tr!("· {when} 스캔", "· Scanned {when}"));
            if st.errors > 0 {
                let label = if st.method == ScanMethod::Mft { tr("연결 끊긴 레코드", "Orphaned records") } else { tr("접근 실패", "Access failures") };
                ui.colored_label(t.warn_text, format!("· {label} {}", thousands(st.errors)));
            }
            if st.cancelled {
                ui.label(tr("· 중지됨(일부 결과)", "· Stopped (partial results)"));
            }
            if !doc.stale.is_empty() {
                let (n, fw) = (thousands(doc.stale.len() as u64), plural(doc.stale.len() as u64, "folder", "folders"));
                ui.colored_label(
                    t.warn_text,
                    tr!("⚠ 일부만 지워진 폴더 {n}개 · 크기가 실제와 다를 수 있음", "⚠ {n} partly deleted {fw} · Sizes may be off"),
                );
                if theme::button(ui, true, tr("다시 스캔", "Rescan")).clicked() {
                    rescan = true;
                }
            }
            // 장치에 묻지 않고 캐시를 읽는다 (끊긴 네트워크 경로에서도 프레임이 멈추지 않게).
            // 가짜 문서(perf 픽스처)의 루트는 실제 경로가 아니므로 묻지 않는다
            let space = if filesystem_actions_enabled(doc) { self.volumes.space(doc.tree.name(Tree::ROOT)) } else { Space::Unavailable };
            match space {
                Space::Known(total, free) => {
                    // 90% 이상 차면 환영 카드처럼 줄 전체를 warn.text로, 글자로도 알린다
                    let full = total > 0 && (total - free) as f64 >= total as f64 * 0.9;
                    let (free, total) = (human_size(free), human_size(total));
                    let text = tr!("여유 공간 {free} / {total}", "Free space {free} / {total}");
                    if full {
                        ui.colored_label(t.warn_text, format!("{text} {}", tr("· 거의 가득 참", "· Almost full")));
                    } else {
                        ui.label(text);
                    }
                }
                Space::Unknown => {
                    ui.colored_label(t.text_tertiary, tr("여유 공간 정보 없음", "No free space info"));
                }
                // 답이 오기 전이나 읽지 못했으면 비워 둔다
                Space::Checking | Space::Unavailable => {}
            }
            // 가리킨 경로는 오른쪽 끝에, mono 12 · text.tertiary, 가운데를 줄여
            if let Some(h) = doc.hovered {
                let path = doc.tree.path(h);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let font = egui::FontId::monospace(theme::MONO);
                    let short = theme::middle_ellipsis(ui, &path, &font, ui.available_width());
                    ui.add(egui::Label::new(theme::mono(short).color(t.text_tertiary)).selectable(false));
                });
            }
        });
        if rescan {
            self.rescan();
        }
    }

    fn welcome(&mut self, ui: &mut egui::Ui) {
        // 나중에 꽂은 USB도 나타나게 목록을 자주 다시 읽는다 (장치에 묻지 않아 가볍다)
        if self.drives_at.elapsed() >= DRIVE_LIST_TTL {
            self.refresh_drives();
        }
        ui.ctx().request_repaint_after(DRIVE_LIST_TTL);
        let can_scan = self.can_start_scan();
        let t = theme::of(ui);
        // 카드에 그릴 내용 (루트, 부제, 용량 문구, 사용 비율). 용량은 캐시에서 읽는다.
        // 매체 없는 카드 리더는 답이 오기 전과 읽지 못했을 때 숨긴다 (깜빡이지 않게)
        let cards: Vec<(String, String, String, Option<f32>)> = self
            .drives
            .iter()
            .filter_map(|d| {
                let space = self.volumes.space(&d.root);
                if !shows_drive(d.kind, space) {
                    return None;
                }
                let used = match space {
                    Space::Known(total, free) => Some((total - free) as f32 / total as f32),
                    _ => None,
                };
                Some((d.root.clone(), drive_subtitle(d, self.volumes.fs(&d.root)), space_note(space), used))
            })
            .collect();
        ui.add_space(40.0);
        ui.vertical_centered(|ui| {
            let title = theme::title(ui, tr("어느 드라이브를 살펴볼까요?", "Which drive would you like to explore?"));
            ui.label(title);
            let hint = if self.elevated {
                // 인자 없이 다시 연 관리자 창에서 다음에 할 일
                tr(
                    "관리자 권한으로 실행 중 · 로컬 NTFS 드라이브는 $MFT로 몇 초 안에 스캔합니다",
                    "Running as administrator · Local NTFS drives scan in seconds via $MFT",
                )
            } else {
                tr(
                    "관리자로 실행하면 드라이브 전체를 훨씬 빠르게 스캔합니다 (⚡ MFT)",
                    "Run as administrator to scan whole drives much faster (⚡ MFT)",
                )
            };
            // 제목 아래 4 · 본문 13 text.secondary · 카드까지 16 (App 보드)
            ui.label(RichText::new(hint).color(t.text_secondary));
            ui.add_space(12.0);
            // 카드는 폭 420 한 줄 · 사이 6 · 모서리 6 · 안쪽 16 (컴포넌트 보드 「드라이브 카드」)
            const CARD_W: f32 = 420.0;
            let inner_w = CARD_W - 32.0 - 2.0;
            let mut chosen = None;
            for (root, sub, note, used) in &cards {
                let full = used.is_some_and(|f| f >= 0.9);
                // 카드 전체가 버튼이다. 호버 모양은 지난 프레임의 응답으로 정해서 내용 뒤에 깐다.
                let card_id = egui::Id::new(("drive-card", root));
                let hovered = ui.ctx().read_response(card_id).is_some_and(|r| r.hovered());
                let (fill, line) = if hovered { (t.surface_raised, t.line_hover) } else { (t.bg_app, t.line) };
                let frame = egui::Frame::new()
                    .fill(fill)
                    .stroke(egui::Stroke::new(1.0, line))
                    .corner_radius(theme::R_PANEL)
                    .inner_margin(16)
                    .show(ui, |ui| {
                        ui.vertical(|ui| {
                            ui.set_width(inner_w);
                            ui.spacing_mut().item_spacing.y = 10.0;
                            ui.horizontal(|ui| {
                                // 루트 18/600, 부제는 오른쪽 12 text.secondary
                                let name = theme::semibold(ui, root.as_str()).size(18.0);
                                ui.label(name);
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    ui.label(theme::caption(sub.as_str()).color(t.text_secondary));
                                });
                            });
                            // 사용량 막대는 회색, 90% 이상이면 warn. 믿을 수 있는 용량일 때만 그린다
                            if let Some(frac) = *used {
                                theme::usage_bar(ui, inner_w, frac, full);
                            }
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 4.0;
                                ui.label(theme::caption(note.as_str()).color(t.text_secondary));
                                // 색만으로 전하지 않게 글자도 붙인다 (이 부분만 warn.text)
                                if full {
                                    ui.label(theme::caption(tr("· 거의 가득 참", "· Almost full")).color(t.warn_text));
                                }
                            });
                        });
                    });
                let rect = frame.response.rect;
                let resp = ui
                    .interact(rect, card_id, egui::Sense::click())
                    .on_hover_cursor(egui::CursorIcon::PointingHand);
                // 화면 낭독기가 읽을 카드 이름 (Frame만으로는 이름 없는 버튼이다)
                let name = format!("{root} {sub} · {note}{}", if full { tr(" · 거의 가득 참", " · Almost full") } else { "" });
                resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, can_scan, &name));
                theme::keyboard_focus(ui, &resp);
                if resp.hovered() != hovered {
                    ui.ctx().request_repaint();
                }
                if resp.clicked() {
                    chosen = Some(root.clone());
                }
                ui.add_space(6.0 - ui.spacing().item_spacing.y);
            }
            // 드라이브 전체가 아니라 폴더 하나를 볼 때 (툴바의 '폴더…'와 같다). 카드 아래 10 (4 + 2 + 4)
            ui.add_space(4.0);
            let pick = ui.scope(|ui| {
                theme::control_height(ui, theme::H_DEFAULT);
                theme::primary_button(ui, can_scan, tr("📁 폴더 선택…", "📁 Choose folder…"))
            });
            if pick.inner.clicked()
                && let Some(dir) = rfd::FileDialog::new().pick_folder()
            {
                chosen = Some(dir.display().to_string());
            }
            if let Some(r) = chosen {
                self.request_scan(r);
            }
        });
    }

    /// 중지한 첫 스캔의 일부 결과라는 배너. 삭제 표시를 막는 까닭과 다시 스캔을 함께 둔다.
    fn partial_notice(&mut self, ui: &mut egui::Ui) {
        if !self.doc.as_ref().is_some_and(|doc| doc.stats.cancelled) { return; }
        let can_scan = self.can_start_scan();
        let t = theme::of(ui);
        let mut rescan = false;
        theme::banner_frame(ui).show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.colored_label(t.warn_text, "⚠");
                ui.label(
                    RichText::new(tr(
                        "스캔을 중지한 일부 결과입니다 · 읽지 않은 폴더는 크기에서 빠져 실제보다 작게 보이고, 삭제 표시는 할 수 없습니다",
                        "Partial results from a stopped scan · Unread folders are left out, so sizes look smaller than they are, and nothing can be marked for deletion",
                    ))
                    .color(t.text_secondary),
                );
                rescan = theme::button(ui, can_scan, tr("↻ 다시 스캔", "↻ Rescan")).clicked();
            });
        });
        ui.add_space(4.0);
        if rescan {
            self.rescan();
        }
    }

    fn scan_view(&mut self, ui: &mut egui::Ui) {
        let (mode, detail, dark) = (self.mode, self.detail, ui.visuals().dark_mode);
        let t = theme::of(ui);
        let Some(s) = &mut self.scanning else { return };
        let scan_id = s.scan_id;
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
            (Some(f), _) if f >= 0.999 => tr("마무리하는 중…", "Finishing…").to_string(),
            (Some(_), Some((v, at))) => {
                let left = fmt_duration((v - at.elapsed().as_secs_f64()).max(1.0));
                tr!("약 {left} 남음", "About {left} left")
            }
            (Some(_), None) => tr("남은 시간 계산 중…", "Estimating time left…").to_string(),
            (None, _) => tr("남은 시간을 알 수 없습니다", "Time left unknown").to_string(),
        };
        // 중지를 눌렀다: 이전 문서가 있으면 다음 프레임에 돌아가고, 없으면 스캔 스레드가 읽은 데까지 정리하는 중이다
        let stopping = p.is_cancelled();
        // 진행 문구 (caption): 읽은 파일 수와 남은 시간
        let caption = if stopping {
            tr("읽은 데까지 정리하는 중입니다", "Wrapping up what was read so far").to_owned()
        } else {
            let read = p.files.load(Relaxed);
            let (files, fw) = (thousands(read), plural(read, "file", "files"));
            tr!("파일 {files}개 · {eta_text}", "{files} {fw} · {eta_text}")
        };
        let title = if stopping {
            tr("중지하는 중…", "Stopping…").to_owned()
        } else {
            let root = &s.root;
            tr!("{root} 스캔 중… {elapsed:.1}s", "Scanning {root}… {elapsed:.1}s")
        };
        let bar = frac.map(|f| f as f32);
        let stop = |ui: &mut egui::Ui, small: bool| {
            if stop_button(ui, !stopping, small).clicked() {
                p.cancel();
                ui.ctx().request_repaint();
                true
            } else {
                false
            }
        };
        let mut stopped = false;

        let Some(tree) = &s.preview else {
            // 미리보기 전 (컴포넌트 보드 「스캔 화면」): 가운데 정렬, 간격 12, 제목 20/600, 막대 420, 문구 13, 중지 32
            ui.add_space(80.0);
            ui.vertical_centered(|ui| {
                ui.spacing_mut().item_spacing.y = 12.0;
                let title = theme::title(ui, title.as_str());
                ui.label(title);
                theme::scan_progress(ui, 420.0, bar);
                ui.label(RichText::new(caption.as_str()).color(t.text_secondary));
                stopped = stop(ui, false);
            });
            if stopped {
                self.preview.invalidate(scan_id);
            }
            return;
        };

        // 미리보기 있음: 높이 32 띠 (surface.panel, 간격 12) 제목 13/600 · 막대 260 · 12px 문구 · 중지 24
        egui::Frame::new().fill(t.surface_panel).inner_margin(egui::Margin::symmetric(8, 4)).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 12.0;
                let title = theme::semibold(ui, title.as_str());
                ui.label(title);
                theme::scan_progress(ui, 260.0, bar);
                ui.label(theme::caption(caption.as_str()).color(t.text_secondary));
                stopped = stop(ui, true);
            });
        });
        if stopped {
            self.preview.invalidate(scan_id);
        }

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
                let name = theme::semibold(ui, tree.name(it.node));
                ui.label(name);
                let n = tree.node(it.node);
                let (size, files, fw) = (human_size(n.size(mode)), thousands(n.files as u64), plural(n.files as u64, "file", "files"));
                ui.label(theme::caption(tr!("{size} · 파일 {files}개 (스캔 중)", "{size} · {files} {fw} (scanning)")).color(t.text_secondary));
            });
        }
    }

    fn treemap_view(&mut self, ui: &mut egui::Ui) {
        let (mode, detail, elevated) = (self.mode, self.detail, self.elevated);
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
        let mut open = None;
        if resp.double_clicked()
            && let Some(it) = hit
        {
            match it.kind {
                ItemKind::OpenDir | ItemKind::ClosedDir => doc.navigate(it.node),
                ItemKind::File => open = Some(it.node),
                ItemKind::Link if filesystem_actions_enabled(doc) => {
                    shell(Shell::Reveal(doc.tree.path(it.node)));
                }
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
            if context_menu(ui, doc, n, elevated) {
                toggle = Some(n);
            }
        });
        if let Some(n) = toggle {
            self.toggle_mark(n);
        }
        if let Some(id) = open {
            self.open_file(id);
        }
        if ui.ctx().data_mut(|d| d.remove_temp::<bool>(copied_id())).is_some() {
            self.toast(tr("경로를 복사했습니다", "Path copied"));
        }

        let Some(doc) = &self.doc else { return };
        if let Some(it) = hit {
            resp.on_hover_ui_at_pointer(|ui| tooltip(ui, doc, it, mode));
        }
    }

    fn delete_dialog_ui(&mut self, ctx: &egui::Context) {
        let Some(dlg) = &mut self.delete_dialog else { return };
        let mut fx = delete::Effects::default();
        let open = delete::show(ctx, dlg, self.doc.as_mut(), self.mode, &mut fx);
        let source_id = dlg.doc_id;
        // 일부만 지워진 폴더는 그 문서에만 남긴다 (실행 중에 문서가 바뀌었으면 버린다)
        if let Some(doc) = &mut self.doc
            && doc.id == source_id
        {
            doc.mark_stale(&fx.stale);
        }
        if fx.open_bin {
            shell(Shell::Explore("shell:RecycleBinFolder".into()));
        }
        if !fx.removed.is_empty() { self.begin_removal(source_id, fx.removed); }
        if !open {
            self.delete_dialog = None;
        }
    }

    /// 삭제가 진행 중이면 창 닫기를 미룬다. 닫히면 작업자 스레드가 끊겨 반쯤 지운 채 아무 기록도 남지 않는다.
    /// eframe의 logic에서 부른다. 최소화된 창이면 다시 띄워 진행 상황과 안내가 보이게 한다.
    fn guard_close(&mut self, ctx: &egui::Context) {
        if self.delete_dialog.as_ref().is_some_and(|d| matches!(d.stage, Stage::Running { .. }))
            && ctx.input(|i| i.viewport().close_requested())
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            if ctx.input(|i| i.viewport().minimized == Some(true)) {
                ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
            }
            self.toast(tr("삭제가 끝난 뒤에 닫을 수 있습니다", "You can close after the deletion finishes"));
        }
    }

    /// 공용 확인 모달. Esc·바깥 클릭은 취소다.
    fn confirm_ui(&mut self, ctx: &egui::Context) {
        let Some(confirm) = &self.confirm else { return };
        let (count, size) = self.doc.as_ref().map_or((0, 0), |doc| (doc.marks.len(), doc.marked_size(self.mode)));
        let (n, w, size) = (thousands(count as u64), plural(count as u64, "item", "items"), human_size(size));
        let marks = tr!("삭제 표시 {n}개({size})", "{n} {w} marked for deletion ({size})");
        // (제목, 본문, 흐린 안내, 진행 버튼)
        let (title, lines, note, ok) = match confirm {
            Confirm::Relaunch(root) => (
                tr("관리자 권한으로 다시 열까요?", "Relaunch as administrator?"),
                [
                    tr!("{marks}는 새 창으로 넘어가지 않습니다.", "{marks} won't carry over to the new window."),
                    match root {
                        Some(r) => tr!("새 창에서 처음부터 다시 스캔합니다: {r}", "The new window will scan again from the start: {r}"),
                        None => tr("새 창은 처음 화면에서 시작합니다.", "The new window will open on the start screen.").into(),
                    },
                ],
                None,
                tr("관리자로 다시 열기", "Relaunch as administrator"),
            ),
            Confirm::Scan(root) => {
                let same = self.doc.as_ref().is_some_and(|doc| doc.tree.name(Tree::ROOT).eq_ignore_ascii_case(root));
                (
                    if same { tr("다시 스캔할까요?", "Rescan?") } else { tr("새로 스캔할까요?", "Start a new scan?") },
                    [
                        tr!("{marks}는 스캔이 끝나면 사라집니다.", "{marks} will be cleared when the scan finishes."),
                        tr!("스캔할 위치: {root}", "Location to scan: {root}"),
                    ],
                    Some(tr("스캔을 중지하면 지금 결과와 표시가 그대로 남습니다.", "If you stop the scan, the current results and marks stay as they are.")),
                    tr("스캔", "Scan"),
                )
            }
        };
        let mut answer = None;
        // 대화상자 프레임 (surface.panel · line.strong · 모서리 8), 제목 20/600, 간격 12
        let modal = egui::Modal::new(egui::Id::new("confirm")).frame(theme::dialog_frame(ctx)).show(ctx, |ui| {
            // 바깥 폭 420 (안쪽 24×2와 테두리를 뺀 370), 줄 사이 10, 안내는 12px text.secondary
            ui.set_width(370.0);
            ui.spacing_mut().item_spacing.y = 10.0;
            let title = theme::title(ui, title);
            ui.label(title);
            for line in &lines {
                ui.label(line);
            }
            if let Some(note) = note {
                ui.label(theme::caption(note).weak());
            }
            // 오른쪽 끝에 [취소][진행]. 지우는 동작이 아니므로 빨강이 아닌 강조 버튼
            theme::dialog_footer(ui, |ui| {
                if theme::primary_button(ui, true, ok).clicked() {
                    answer = Some(true);
                }
                if theme::button(ui, true, tr("취소", "Cancel")).clicked() {
                    answer = Some(false);
                }
            });
        });
        if answer.is_none() && modal.should_close() {
            answer = Some(false);
        }
        if let Some(proceed) = answer {
            self.resolve_confirm(ctx, proceed);
        }
    }

    /// 확인 창의 답. 어느 쪽이든 창은 닫는다.
    fn resolve_confirm(&mut self, ctx: &egui::Context, proceed: bool) {
        match self.confirm.take() {
            Some(Confirm::Relaunch(root)) if proceed => self.relaunch(ctx, root),
            Some(Confirm::Scan(root)) if proceed => self.start_scan(root),
            _ => {}
        }
    }

    fn toast_ui(&mut self, ctx: &egui::Context) {
        let Some(toast) = &self.toast else { return };
        if toast.at.elapsed() > if toast.undo.is_some() { UNDO_FOR } else { TOAST_FOR } {
            self.toast = None;
            return;
        }
        // 되돌릴 수 없게 된 뒤(새 표시, 다른 문서)와 스캔 중에는 버튼을 숨긴다
        let can_undo = self.scanning.is_none()
            && toast.undo.as_ref().is_some_and(|u| self.doc.as_ref().is_some_and(|doc| u.applies_to(doc)));
        let mut undo = false;
        egui::Area::new(egui::Id::new("toast"))
            .anchor(egui::Align2::CENTER_BOTTOM, egui::vec2(0.0, -48.0))
            // 모달(검토·삭제 중 창) 위에 그린다. 모달보다 위 층이라 '되돌리기'도 누를 수 있다
            .order(egui::Order::Tooltip)
            .show(ctx, |ui| {
                // 반전 토스트 (inverse.surface · inverse.text, 모서리 6, 안쪽 14×8)
                theme::toast(ui, |ui| {
                    ui.horizontal(|ui| {
                        // 글과 되돌리기 사이 12
                        ui.spacing_mut().item_spacing.x = 12.0;
                        ui.label(&toast.msg);
                        undo = can_undo && ui.button(tr("되돌리기", "Undo")).clicked();
                    });
                });
            });
        if undo {
            self.undo_clear();
        }
        ctx.request_repaint_after(Duration::from_millis(200));
    }

    fn keyboard(&mut self, ctx: &egui::Context) {
        // 스캔 중에는 검색 칸이 숨어 있다 (끝난 뒤 포커스가 검색 칸으로 튀지 않게)
        let ctrl_f = ctx.input(|i| i.modifiers.command && i.key_pressed(Key::F));
        if self.scanning.is_none()
            && (ctrl_f || (!ctx.egui_wants_keyboard_input() && ctx.input(|i| i.key_pressed(Key::Slash))))
        {
            self.focus_search = true;
        }
        // 모달 뒤에서 Space 같은 키가 표시를 바꾸지 않게 한다
        if ctx.egui_wants_keyboard_input() || self.delete_dialog.is_some() || self.confirm.is_some()
            || self.scanning.is_some() || self.pending_removal.is_some() {
            return;
        }
        let mut toggle = None;
        let mut detail = None;
        let mut open = None;
        let mut rescan = false;
        let Some(doc) = &mut self.doc else { return };
        ctx.input(|i| {
            if i.modifiers.alt && i.key_pressed(Key::ArrowLeft) || i.pointer.button_pressed(PointerButton::Extra1) {
                doc.go_back();
            } else if i.modifiers.alt && i.key_pressed(Key::ArrowRight)
                || i.pointer.button_pressed(PointerButton::Extra2)
            {
                doc.go_forward();
            } else if i.key_pressed(Key::Backspace) || i.modifiers.alt && i.key_pressed(Key::ArrowUp) {
                doc.go_up();
            } else if i.key_pressed(Key::Enter) {
                // 폴더는 확대, 파일은 더블클릭과 같은 열기 정책
                if let Some(s) = doc.selected {
                    match doc.tree.node(s).kind {
                        Kind::Dir => doc.navigate(s),
                        Kind::File => open = Some(s),
                        Kind::Link => {}
                    }
                }
            } else if i.key_pressed(Key::Escape) {
                doc.selected = None;
            } else if i.key_pressed(Key::Space) || i.key_pressed(Key::X) || i.key_pressed(Key::Delete) {
                toggle = doc.selected;
            } else if i.key_pressed(Key::Minus) || i.key_pressed(Key::OpenBracket) {
                detail = Some(false);
            } else if i.key_pressed(Key::Plus) || i.key_pressed(Key::Equals) || i.key_pressed(Key::CloseBracket) {
                detail = Some(true);
            } else if i.key_pressed(Key::F5) {
                rescan = true;
            }
        });
        if rescan {
            self.rescan();
        }
        if let Some(n) = toggle {
            self.toggle_mark(n);
        }
        if let Some(id) = open {
            self.open_file(id);
        }
        match detail {
            Some(true) => self.detail = self.detail.more(),
            Some(false) => self.detail = self.detail.less(),
            None => {}
        }
    }
}

/// 툴바 묶음 사이 세로선 (1×16, line). `w`는 차지하는 폭이고 선은 가운데에 그린다
fn divider(ui: &mut egui::Ui, w: f32) {
    let c = theme::of(ui).line;
    let (rect, _) = ui.allocate_exact_size(egui::vec2(w, 16.0), egui::Sense::hover());
    ui.painter().vline(rect.center().x, rect.y_range(), egui::Stroke::new(1.0, c));
}

/// 크기를 다 세지 못한 조상 경로: `theme::crumb`과 같은 링크에 ⚠만 warn.text로 붙인다
fn crumb_warn(ui: &mut egui::Ui, name: &str) -> egui::Response {
    let t = theme::of(ui);
    let font = egui::FontId::proportional(theme::BODY);
    let mut job = egui::text::LayoutJob::default();
    job.append(name, 0.0, egui::TextFormat::simple(font.clone(), Color32::PLACEHOLDER));
    job.append(" ⚠", 0.0, egui::TextFormat::simple(font, t.warn_text));
    let galley = ui.painter().layout_job(job);
    let (gs, pad) = (galley.size(), egui::vec2(6.0, 4.0));
    let (rect, resp) = ui.allocate_exact_size(gs + pad * 2.0, egui::Sense::click());
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Link, ui.is_enabled(), galley.text()));
    let hovered = resp.hovered();
    let fg = if hovered { t.text_primary } else { t.text_secondary };
    let pos = rect.min + pad;
    if hovered {
        ui.painter().hline(pos.x..=pos.x + gs.x, pos.y + gs.y - 1.0, egui::Stroke::new(1.0, fg));
    }
    ui.painter().galley(pos, galley, fg);
    theme::keyboard_focus(ui, &resp);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// 검색 칸 툴팁: 검색 문법 표 (왼쪽 mono 키, 오른쪽 12px 설명)
fn search_help(ui: &mut egui::Ui) {
    let t = theme::of(ui);
    let rows = [
        ("*.mp4", tr("와일드카드 · 이름 일부만 써도 됩니다", "Wildcard · part of a name also works")),
        ("ext:mp4;mkv", tr("확장자 (; 로 여럿)", "Extension (several with ;)")),
        ("size:>1gb", tr("크기 (> < =)", "Size (> < =)")),
        ("dm:<7d", tr("수정일 (최근 7일 · dm:>1y)", "Modified (last 7 days · dm:>1y)")),
        ("path:steam", tr("경로에 포함", "Path contains")),
        (tr("file: folder: !단어", "file: folder: !word"), tr("파일만 · 폴더만 · 제외어", "Files only · folders only · exclude")),
    ];
    egui::Grid::new("search-help").num_columns(2).spacing([12.0, 6.0]).min_col_width(150.0).min_row_height(16.0).show(ui, |ui| {
        for (key, what) in rows {
            ui.label(theme::mono(key));
            ui.label(theme::caption(what).color(t.text_secondary));
            ui.end_row();
        }
    });
}

/// 스캔 화면의 '중지' (보조 버튼, 좌우 12): 미리보기 전 32, 미리보기 띠에서는 24에 12px
fn stop_button(ui: &mut egui::Ui, enabled: bool, small: bool) -> egui::Response {
    ui.scope(|ui| {
        theme::control_height(ui, if small { theme::H_SMALL } else { theme::H_DEFAULT });
        if small {
            ui.add_enabled(enabled, egui::Button::new(theme::caption(tr("중지", "Stop"))))
        } else {
            theme::button(ui, enabled, tr("중지", "Stop"))
        }
    })
    .inner
}

/// 묶음은 소유 폴더를 가리키지만 실제 선택 가능한 노드는 아니다.
fn selectable_node(item: Option<&treemap::Item>) -> Option<NodeId> {
    item.filter(|it| !matches!(it.kind, ItemKind::Group { .. }))
        .map(|it| it.node)
}

/// 메뉴에서 경로를 복사했다는 표지 (treemap_view가 토스트로 알린다)
fn copied_id() -> egui::Id {
    egui::Id::new("darak.path_copied")
}

/// 오른쪽 클릭 메뉴. 삭제 표시를 바꿔야 하면 true.
fn context_menu(ui: &mut egui::Ui, doc: &mut Doc, n: NodeId, elevated: bool) -> bool {
    let t = theme::of(ui);
    let mut toggle = false;
    let path = doc.tree.path(n);
    // 항목 높이 24 (컴포넌트 보드 「메뉴」), 구분선은 line.popup
    theme::control_height(ui, theme::H_SMALL);
    let name = theme::semibold(ui, doc.tree.name(n));
    ui.label(name);
    theme::menu_separator(ui);
    if doc.tree.node(n).kind == Kind::Dir && ui.button(tr("🔍 여기로 확대", "🔍 Zoom in here")).clicked() {
        doc.navigate(n);
        ui.close();
    }
    match doc.tree.node(n).kind {
        Kind::File => {
            // 열지 않는 파일은 아래의 '위치 보기'를 쓰게 한다
            let refusal = open_refusal(doc.tree.name(n), elevated);
            let open = ui.add_enabled(filesystem_actions_enabled(doc) && refusal.is_none(), egui::Button::new(tr("📄 열기", "📄 Open")));
            let open = match refusal {
                Some(why) => open.on_disabled_hover_text(why),
                None => open,
            };
            if open.clicked() {
                open_item(doc, n, elevated);
                ui.close();
            }
        }
        Kind::Dir => {
            if ui.add_enabled(filesystem_actions_enabled(doc), egui::Button::new(tr("📂 폴더 열기", "📂 Open folder"))).clicked() {
                // 관리자 권한이면 타사 폴더 처리기가 관리자 권한으로 뜨지 않게 탐색기로 직접 연다
                shell(if elevated { Shell::Explore(path.clone()) } else { Shell::Open(path.clone()) });
                ui.close();
            }
        }
        Kind::Link => {}
    }
    if ui.add_enabled(filesystem_actions_enabled(doc), egui::Button::new(tr("🔎 탐색기에서 위치 보기", "🔎 Show in File Explorer"))).clicked() {
        shell(Shell::Reveal(path.clone()));
        ui.close();
    }
    if ui.button(tr("📋 경로 복사", "📋 Copy path")).clicked() {
        ui.ctx().copy_text(path.clone());
        ui.ctx().data_mut(|d| d.insert_temp(copied_id(), true));
        ui.close();
    }
    theme::menu_separator(ui);
    let state = doc.mark_state(n);
    let protected = doc.protected(n);
    if state == 1 {
        if ui.button(tr("↩ 삭제 표시 해제", "↩ Unmark")).clicked() {
            toggle = true;
            ui.close();
        }
    } else if state == 2 {
        ui.add_enabled(false, egui::Button::new(tr("🗑 상위 폴더가 이미 표시됨", "🗑 Parent folder already marked")));
    } else if doc.stats.cancelled {
        ui.add_enabled(false, egui::Button::new(tr("🗑 삭제 표시", "🗑 Mark for deletion")))
            .on_disabled_hover_text(tr(
                "중지한 스캔의 일부 결과에서는 표시할 수 없습니다 · 다시 스캔하세요",
                "Can't mark items in partial results from a stopped scan · Rescan",
            ));
    } else if let Some(r) = protected {
        ui.add_enabled(false, egui::Button::new(tr("🗑 삭제 표시", "🗑 Mark for deletion")))
            .on_disabled_hover_text(tr!("보호된 항목: {r}", "Protected item: {r}"));
    } else if ui
        .button(RichText::new(tr("🗑 삭제 표시  (Space)", "🗑 Mark for deletion  (Space)")).color(t.danger_text))
        .clicked()
    {
        // 지우는 쪽 항목은 danger.text (보드 「메뉴」의 '삭제 표시')
        toggle = true;
        ui.close();
    }
    toggle
}

/// 트리맵 툴팁 (컴포넌트 보드 「툴팁」): 이름 600, 경로·표는 12px, 경고는 warn.text
fn tooltip(ui: &mut egui::Ui, doc: &Doc, it: treemap::Item, mode: SizeMode) {
    let t = theme::of(ui);
    let tree = &doc.tree;
    let n = tree.node(it.node);
    let secondary = |s: String| theme::caption(s).color(t.text_secondary);
    if let ItemKind::Group { count, size } = it.kind {
        let items = plural(count as u64, "item", "items");
        let title = theme::semibold(ui, tr!("작은 항목 {count}개", "{count} small {items}"));
        ui.label(title);
        ui.label(secondary(format!("{} · {}", human_size(size), tree.path(it.node))));
        ui.label(secondary(tr("폴더를 확대하면 보입니다", "Zoom into the folder to see them").into()));
        return;
    }
    let name = theme::semibold(ui, tree.name(it.node));
    ui.label(name);
    // 긴 경로는 가운데를 줄인다 (앞의 드라이브와 끝의 이름은 남는다)
    let path = theme::middle_ellipsis(ui, &tree.path(it.node), &egui::FontId::monospace(theme::MONO), 420.0);
    ui.label(theme::mono(path).color(t.text_secondary));
    ui.add_space(6.0);
    let size = n.size(mode);
    let pct = |of: NodeId| {
        let t = tree.node(of).size(mode);
        if t > 0 { size as f64 * 100.0 / t as f64 } else { 0.0 }
    };
    let key = |ui: &mut egui::Ui, s: &str| ui.label(theme::caption(s).color(t.text_secondary));
    let value = |ui: &mut egui::Ui, s: String| ui.label(theme::caption(s));
    egui::Grid::new("tip").num_columns(2).spacing([12.0, 4.0]).min_col_width(56.0).min_row_height(16.0).show(ui, |ui| {
        key(ui, tr("크기", "Size"));
        value(ui, format!("{} ({} B)", human_size(size), thousands(size)));
        ui.end_row();
        let other = match mode {
            SizeMode::Allocated => (tr("논리 크기", "Logical size"), n.logical),
            SizeMode::Logical => (tr("할당 크기", "Allocated size"), n.alloc),
        };
        key(ui, other.0);
        value(ui, human_size(other.1));
        ui.end_row();
        if let Some(p) = tree.parent(it.node) {
            key(ui, tr("비율", "Share"));
            let (of_parent, of_view) = (pct(p), pct(doc.focus));
            value(ui, tr!(
                "상위 폴더의 {of_parent:.1}% · 현재 보기의 {of_view:.1}%",
                "{of_parent:.1}% of parent folder · {of_view:.1}% of current view"
            ));
            ui.end_row();
        }
        if n.kind == Kind::Dir {
            key(ui, tr("파일 수", "Files"));
            value(ui, thousands(n.files as u64));
            ui.end_row();
        }
        if n.mtime > 0 {
            key(ui, tr("수정", "Modified"));
            value(ui, format_time(n.mtime));
            ui.end_row();
        }
    });
    // 표시 상태는 배지 (글리프와 글자로도 뜻을 전한다)
    match doc.mark_state(it.node) {
        1 => {
            ui.label(theme::caption(tr("🗑 삭제 표시됨", "🗑 Marked for deletion")).color(t.danger_text));
        }
        2 => {
            ui.label(theme::caption(tr("🗑 상위 폴더가 삭제 표시됨", "🗑 Parent folder marked for deletion")).color(t.danger_text));
        }
        _ => {}
    }
    let warn = |s: String| theme::caption(s).color(t.warn_text);
    if let Some(note) = incomplete_note(doc, it.node) {
        ui.add_space(2.0);
        ui.label(warn(format!("⚠ {note}")));
        if let Some(names) = unread_children(tree, it.node) {
            ui.label(secondary(names));
        }
    }
    if doc.stale_within(it.node) {
        ui.label(warn(tr("⚠ 일부만 지워진 폴더라 크기가 실제와 다를 수 있습니다 · 다시 스캔하세요", "⚠ Partly deleted folder, so sizes may be off · Rescan").into()));
    }
    if n.has(flags::HARDLINK_DUP) {
        // 컴포넌트 보드의 중립 배지 문구
        ui.add_space(2.0);
        theme::badge(ui, tr("하드링크 · 0으로 계산", "Hard link · counted as 0"));
    }
    if n.kind == Kind::Link {
        ui.label(secondary(tr("junction/심볼릭 링크: 따라가지 않음", "Junction/symbolic link: not followed").into()));
    }
}

fn morph_items<'a>(morph: &mut Morph, items: &'a [Item], key: MorphKey, out: &'a mut Vec<Item>) -> (&'a [Item], bool) {
    if morph.is_settled(key) { return (items, false); }
    let moving = morph.step_into(items, key, out);
    (if morph.is_settled(key) { items } else { out }, moving)
}

/// 크기를 다 세지 못한 까닭 (툴팁·breadcrumb). 다 셌으면 None.
fn incomplete_note(doc: &Doc, id: NodeId) -> Option<&'static str> {
    let (tree, n) = (&doc.tree, doc.tree.node(id));
    if n.has(flags::DENIED) {
        Some(tr("접근이 거부되어 일부 크기를 모릅니다", "Access denied, so some sizes are unknown"))
    } else if !n.has(flags::INCOMPLETE) {
        None
    } else if doc.stats.cancelled && !tree.live_children(id).any(|c| tree.node(c).incomplete()) {
        // 중지 때문에 이 폴더 자체를 읽지 않았다 (끝까지 한 스캔이면 읽지 못한 하위 폴더를 지운 경우라 아래 문구)
        Some(tr("스캔을 중지해 읽지 못한 폴더라 실제 크기는 더 클 수 있습니다", "Not read because the scan was stopped, so the actual size may be larger"))
    } else {
        Some(tr("읽지 못한 하위 폴더가 있어 실제 크기는 더 클 수 있습니다", "Some subfolders couldn't be read, so the actual size may be larger"))
    }
}

/// 크기를 다 세지 못한 바로 아래 폴더 (앞의 3개 이름, 나머지는 개수). 따라 내려가면 원인 폴더에 닿는다.
fn unread_children(tree: &Tree, id: NodeId) -> Option<String> {
    let mut unread = tree.live_children(id).filter(|&c| tree.node(c).incomplete());
    let names: Vec<&str> = unread.by_ref().take(3).map(|c| tree.name(c)).collect();
    if names.is_empty() {
        return None;
    }
    let names = names.join(", ");
    Some(match unread.count() {
        0 => tr!("이어지는 하위 폴더: {names}", "Incomplete subfolders: {names}"),
        rest => {
            let rest = thousands(rest as u64);
            tr!("이어지는 하위 폴더: {names} 외 {rest}개", "Incomplete subfolders: {names} and {rest} more")
        }
    })
}

/// 창 제목. 관리자 창은 Windows의 '관리자: 명령 프롬프트'와 같은 관례를 따른다.
pub fn window_title(elevated: bool) -> &'static str {
    if elevated { tr("관리자: Darak", "Administrator: Darak") } else { "Darak" }
}

fn fmt_duration(secs: f64) -> String {
    let s = secs.round() as u64;
    if s < 60 {
        tr!("{s}초", "{s}s")
    } else {
        let (m, s) = (s / 60, s % 60);
        tr!("{m}분 {s:02}초", "{m}m {s:02}s")
    }
}

/// 스캔 시각: 오늘이면 `14:32`, 아니면 `2026-10-03 16:42` (시각을 읽지 못하면 `-`)
fn scanned_label(at: u32) -> String {
    let (t, now) = (format_time(at), format_time(now_secs2000()));
    let today = t.get(..10).is_some_and(|day| now.get(..10) == Some(day));
    match t.get(11..) {
        Some(time) if today => time.to_owned(),
        _ => t,
    }
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

/// 파일을 직접 열지 않는 까닭. 열어도 되면 None.
/// 관리자 권한이면 무엇을 열든 관리자 권한을 물려주므로 모두 거부하고, 일반 권한이면 코드가 실행되는 파일만 거부한다.
fn open_refusal(name: &str, elevated: bool) -> Option<&'static str> {
    if elevated {
        Some(tr(
            "관리자 권한으로 실행 중이라 파일은 열지 않고 탐색기에서 위치를 보여 줍니다",
            "Running as administrator, so files are shown in File Explorer instead of being opened",
        ))
    } else if runs_code(name) {
        Some(tr(
            "실행 파일과 스크립트는 열지 않고 탐색기에서 위치를 보여 줍니다",
            "Executables and scripts are shown in File Explorer instead of being opened",
        ))
    } else {
        None
    }
}

/// 파일을 기본 앱으로 연다. 열지 않는 파일이거나 열지 못하면 탐색기에서 위치를 보여 준다.
/// 열지 않은 까닭(open_refusal)을 돌려준다.
fn open_item(doc: &Doc, id: NodeId, elevated: bool) -> Option<&'static str> {
    if !filesystem_actions_enabled(doc) { return None; }
    let path = doc.tree.path(id);
    let refusal = open_refusal(doc.tree.name(id), elevated);
    if refusal.is_some() || !shell(Shell::Open(path.clone())) {
        shell(Shell::Reveal(path));
    }
    refusal
}

/// 드라이브를 목록(환영 카드·콤보)에 보일지: 매체 없는 이동식(카드 리더)은 용량 답이 오기 전과 읽지 못했을 때 숨긴다
fn shows_drive(kind: u32, space: Space) -> bool {
    kind != DRIVE_REMOVABLE || matches!(space, Space::Known(..) | Space::Unknown)
}

/// 카드·콤보의 용량 문구. 믿을 수 없는 값(8192 PB 같은 표지값)은 숫자 없이 '용량 정보 없음'
fn space_note(space: Space) -> String {
    match space {
        Space::Known(total, free) => {
            let (free, total) = (human_size(free), human_size(total));
            tr!("여유 공간 {free} / {total}", "Free space {free} / {total}")
        }
        Space::Checking => tr("용량 확인 중…", "Checking capacity…").into(),
        Space::Unknown => tr("용량 정보 없음", "No capacity info").into(),
        Space::Unavailable => tr("용량을 읽을 수 없습니다", "Can't read capacity").into(),
    }
}

/// 드라이브 카드의 부제: 종류와 파일 시스템 ('로컬 디스크 · NTFS'). 네트워크는 느릴 수 있다고 알린다
fn drive_subtitle(d: &Drive, fs: Option<&str>) -> String {
    if d.kind == DRIVE_REMOTE {
        tr("네트워크 · 일반 스캔이라 느릴 수 있음", "Network · Standard scan, may be slow").into()
    } else {
        fs.map_or_else(|| d.kind_label().into(), |fs| format!("{} · {fs}", d.kind_label()))
    }
}

/// 상태 바의 스캔 방법 배지 (글자, 툴팁). 일반 스캔으로 바꾼 이유는 배지 글자에 들어간다
fn method_badge(st: &ScanStats) -> (String, String) {
    match (st.method, &st.fallback_reason) {
        (ScanMethod::Mft, _) => ("⚡ MFT".into(), tr("NTFS의 $MFT를 직접 읽어 스캔했습니다", "Scanned by reading the NTFS $MFT directly").into()),
        (ScanMethod::Walk, Some(r)) => {
            let short = r.short();
            (
                tr!("일반 스캔 · {short}", "Standard scan · {short}"),
                match r {
                    Fallback::NotElevated => tr!(
                        "{r}\n관리자로 실행하면 $MFT를 직접 읽어 몇 초 안에 끝납니다",
                        "{r}\nRun as administrator to read $MFT directly and finish in seconds"
                    ),
                    _ => r.to_string(),
                },
            )
        }
        (ScanMethod::Walk, None) => (
            tr("일반 스캔", "Standard scan").into(),
            tr("폴더를 하나씩 열어 셉니다 (MFT는 드라이브 루트 전용)", "Counts by opening folders one by one (MFT is for drive roots only)").into(),
        ),
    }
}

/// 같은 경로를 다시 스캔하면 지난 일반 순회 결과의 할당 크기가 가장 가까운 진행률 기준이다.
/// MFT 결과(볼륨 전체를 셈)와 중지한 일부 결과는 쓰지 않는다
fn previous_walk_size(doc: Option<&Doc>, root: &str) -> Option<u64> {
    let doc = doc?;
    (doc.tree.name(Tree::ROOT).eq_ignore_ascii_case(root) && doc.stats.method == ScanMethod::Walk && !doc.stats.cancelled)
        .then(|| doc.tree.node(Tree::ROOT).alloc)
}

/// 스캔 스레드 패닉의 내용 (panic!에 넘긴 문장)
fn panic_text(p: &(dyn std::any::Any + Send)) -> String {
    p.downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| p.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| tr("알 수 없는 오류", "Unknown error").into())
}

/// 네트워크 위치인지: UNC 경로(`\\server\share`, `\\?\UNC\..`)이거나 네트워크 드라이브 문자
fn is_network(root: &str, drives: &[Drive]) -> bool {
    let path = root.strip_prefix(r"\\?\").unwrap_or(root);
    if path.starts_with(r"\\") || path.get(..4).is_some_and(|p| p.eq_ignore_ascii_case(r"UNC\")) {
        return true;
    }
    let letter = |p: &str| p.get(..2).map(str::to_ascii_uppercase);
    drives.iter().any(|d| d.kind == DRIVE_REMOTE && letter(&d.root).is_some_and(|l| letter(path) == Some(l)))
}

/// '관리자로 실행' 툴팁: 새 창이 무엇을 하는지
fn relaunch_hint(root: Option<&str>) -> String {
    match root {
        None => tr(
            "관리자 권한으로 다시 엽니다. 드라이브를 고르면 $MFT를 직접 읽어 몇 초 안에 스캔합니다",
            "Relaunches as administrator. Pick a drive to scan it in seconds by reading $MFT directly",
        )
        .into(),
        Some(r) if is_drive_root(r) => tr!(
            "관리자 권한으로 다시 열어 바로 스캔합니다: {r}\n$MFT를 직접 읽어 몇 초 안에 끝납니다",
            "Relaunches as administrator and scans right away: {r}\nReads $MFT directly and finishes in seconds"
        ),
        Some(r) => tr!(
            "관리자 권한으로 다시 열어 바로 스캔합니다: {r}\n\
             접근이 거부된 폴더까지 읽습니다. $MFT 빠른 스캔은 드라이브 전체를 고를 때만 씁니다",
            "Relaunches as administrator and scans right away: {r}\n\
             Also reads folders that deny access. The fast $MFT scan is only used when you pick a whole drive"
        ),
    }
}

/// 셸에 맡기는 동작. 테스트 빌드에서는 실행하지 않고 기록만 한다.
#[derive(Debug, PartialEq)]
enum Shell {
    /// 기본 앱으로 열기
    Open(String),
    /// 탐색기 창으로 폴더 열기
    Explore(String),
    /// 탐색기에서 위치 보기 (항목을 선택한 채로)
    Reveal(String),
    /// 관리자 권한으로 다시 실행 (새 창이 스캔할 루트)
    Elevate(Option<String>),
}

/// 셸 동작을 실행한다. 성공하면 true. 탐색기는 이름이 아니라 절대 경로로 띄운다 (explorer_exe).
#[cfg(not(test))]
fn shell(action: Shell) -> bool {
    use darak_core::scan::{explorer_exe, relaunch_as_admin, shell_open};
    use std::os::windows::process::CommandExt;
    match action {
        Shell::Open(path) => shell_open(&path),
        Shell::Explore(path) => std::process::Command::new(explorer_exe()).arg(path).spawn().is_ok(),
        Shell::Reveal(path) => {
            std::process::Command::new(explorer_exe()).raw_arg(format!("/select,\"{path}\"")).spawn().is_ok()
        }
        Shell::Elevate(root) => relaunch_as_admin(root.as_deref()),
    }
}

#[cfg(test)]
thread_local! {
    /// 테스트에서 shell()이 받은 동작
    static SHELL_LOG: std::cell::RefCell<Vec<Shell>> = const { std::cell::RefCell::new(Vec::new()) };
    /// 테스트에서 shell()이 돌려줄 값
    static SHELL_OK: std::cell::Cell<bool> = const { std::cell::Cell::new(true) };
}

/// 테스트 빌드: 아무것도 실행하지 않고 기록만 한다.
#[cfg(test)]
fn shell(action: Shell) -> bool {
    SHELL_LOG.with_borrow_mut(|log| log.push(action));
    SHELL_OK.get()
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
    use crate::delete::{Finished, Method, Outcome};
    use crate::doc::tests::{doc, fixture, node};
    use crate::drives::Probe;
    use darak_core::NO_NODE;
    use darak_core::remove::RecycleBin;

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
            drives_at: Instant::now(),
            // 테스트는 장치에 묻지 않는다 (용량을 읽지 못한 것으로 답한다)
            volumes: Volumes::with_probe(None, Arc::new(|_| Probe::default())),
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
            confirm: None,
            focus_search: false,
            detail: Detail::default(),
            anim: None,
            last_focus: None,
            morph: Morph::default(),
            morph_items: Vec::new(),
            morph_until: None,
            lang_pref: LangPref::Ko,
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
        assert_eq!(app.admin_relaunch(), AdminRelaunch::Hidden);
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

    /// 앱 전체(App::show: 툴바·상태 바·모달·키보드까지)를 창 크기로 한 프레임 그린다
    fn full_frame(ctx: &egui::Context, app: &mut App, events: Vec<egui::Event>, time: f64) -> egui::FullOutput {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1280.0, 820.0))),
            events,
            time: Some(time),
            ..Default::default()
        };
        let mut output = ctx.run_ui(input, |ui| app.show(ui));
        output.textures_delta.clear();
        output
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

    /// 결과를 기다리는 실행 단계 (폴더 하나)
    fn running_stage(rx: mpsc::Receiver<Finished>) -> Stage {
        Stage::Running {
            method: Method::Recycle,
            items: vec![delete::Item { node: 1, path: "C:\\Data\\folder".into(), is_dir: true, reparse: false, size: 60 }],
            done: Arc::default(),
            cancel: Arc::default(),
            started: Instant::now(),
            rx,
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
        app.delete_dialog.as_mut().unwrap().stage = running_stage(rx);
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
            tx.send(Finished {
                outcomes: vec![
                    Outcome { node: u32::MAX, path: "C:\\original-source".into(), size: 60, error: None, partial: false },
                    // 다른 문서의 노드 번호이므로 지금 문서의 stale에 넣지 않는다
                    Outcome { node: 1, path: "C:\\original-dir".into(), size: 60, error: Some("사용 중".into()), partial: true },
                ],
                free: None,
            })
            .unwrap();
            app.delete_dialog = Some(Dialog { doc_id: 1, stage: running_stage(rx) });
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
            let Stage::Report { finished, .. } = &dlg.stage else {
                panic!("source outcome report missing")
            };
            assert_eq!(finished.outcomes[0].path, "C:\\original-source");
            assert_eq!(finished.outcomes[0].node, u32::MAX);
            if let Some(doc) = &app.doc {
                assert_eq!(doc.id, 2);
                assert_eq!(doc.tree.version, 0);
                assert_eq!(doc.tree.node(0).alloc, 100);
                assert!(doc.marks.is_empty());
                assert!(doc.stale.is_empty());
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

    /// 창의 X 버튼을 누른 입력
    fn close_input(time: f64) -> egui::RawInput {
        let mut input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1280.0, 820.0))),
            time: Some(time),
            ..Default::default()
        };
        input.viewports.get_mut(&egui::ViewportId::ROOT).unwrap().events.push(egui::ViewportEvent::Close);
        input
    }

    /// 창의 X 버튼을 누른 한 프레임 (앱 전체, eframe처럼 logic 다음 ui)
    fn close_frame(ctx: &egui::Context, app: &mut App, time: f64) -> egui::FullOutput {
        let mut output = ctx.run_ui(close_input(time), |ui| {
            app.guard_close(ui.ctx());
            app.show(ui);
        });
        output.textures_delta.clear();
        output
    }

    fn cancels_close(output: &egui::FullOutput) -> bool {
        output.viewport_output[&egui::ViewportId::ROOT].commands.contains(&egui::ViewportCommand::CancelClose)
    }

    #[test]
    fn close_waits_for_running_delete() {
        const WAIT: &str = "삭제가 끝난 뒤에 닫을 수 있습니다";
        let ctx = egui::Context::default();
        let mut app = app(fixture(1));
        assert!(!cancels_close(&close_frame(&ctx, &mut app, 0.0)));
        app.doc.as_mut().unwrap().toggle_mark(3).unwrap();
        app.delete_dialog = Some(Dialog {
            doc_id: 1,
            stage: Stage::Review { method: Method::Recycle, confirmed: false, error: None, bin: RecycleBin::Available { capacity: None } },
        });
        // 검토 중에는 아직 아무것도 지우지 않았으므로 닫혀도 된다
        assert!(!cancels_close(&close_frame(&ctx, &mut app, 0.1)));
        assert!(app.toast.is_none());
        let (tx, rx) = mpsc::channel();
        app.delete_dialog.as_mut().unwrap().stage = running_stage(rx);
        assert!(cancels_close(&close_frame(&ctx, &mut app, 0.2)));
        assert_eq!(app.toast.as_ref().map(|t| t.msg.as_str()), Some(WAIT));
        // 안내는 삭제 중 창 위에 보인다
        let output = full_frame(&ctx, &mut app, vec![], 0.3);
        assert!(has_removal_text(&output, WAIT));
        tx.send(Finished { outcomes: vec![], free: None }).unwrap();
        full_frame(&ctx, &mut app, vec![], 0.4);
        assert!(matches!(app.delete_dialog.as_ref().unwrap().stage, Stage::Report { .. }));
        assert!(!cancels_close(&close_frame(&ctx, &mut app, 0.5)));
    }

    #[test]
    fn minimized_close_waits_for_running_delete() {
        // 최소화된 창은 ui 없이 logic만 돌므로, 작업 표시줄에서 닫아도 막고 창을 다시 띄워야 한다
        let ctx = egui::Context::default();
        let mut app = app(fixture(1));
        let (_tx, rx) = mpsc::channel();
        app.delete_dialog = Some(Dialog { doc_id: 1, stage: running_stage(rx) });
        let mut input = close_input(0.0);
        input.viewports.get_mut(&egui::ViewportId::ROOT).unwrap().minimized = Some(true);
        let output = ctx.run_logic(&input, |ctx| app.guard_close(ctx));
        let commands = &output.viewport_commands[&egui::ViewportId::ROOT];
        assert!(commands.contains(&egui::ViewportCommand::CancelClose));
        assert!(commands.contains(&egui::ViewportCommand::Minimized(false)));
        assert_eq!(app.toast.as_ref().map(|t| t.msg.as_str()), Some("삭제가 끝난 뒤에 닫을 수 있습니다"));
    }

    #[test]
    fn partial_failures_mark_folders_stale() {
        let ctx = egui::Context::default();
        let mut app = app(fixture(1));
        let (tx, rx) = mpsc::channel();
        tx.send(Finished {
            outcomes: vec![
                Outcome { node: 2, path: "C:\\Data\\sibling".into(), size: 40, error: None, partial: false },
                Outcome { node: 1, path: "C:\\Data\\folder".into(), size: 60, error: Some("사용 중".into()), partial: true },
            ],
            free: None,
        })
        .unwrap();
        app.delete_dialog = Some(Dialog { doc_id: 1, stage: running_stage(rx) });
        full_frame(&ctx, &mut app, vec![], 0.0);
        // 검색 작업자가 트리를 돌려주면 반영된다 (멈춘 작업자는 같은 프레임에 돌려줄 수도 있다)
        if let Some(pending) = &app.pending_removal {
            assert_eq!(pending.ids, [2]);
            let fence = pending.fence;
            app.search_control(SearchControlEvent::Released(fence));
            app.finish_removal();
        }
        assert!(app.pending_removal.is_none());
        let doc = app.doc.as_ref().unwrap();
        assert!(doc.tree.node(2).has(flags::REMOVED));
        assert_eq!(doc.stale.iter().copied().collect::<Vec<_>>(), [1]);
        assert!(doc.stale_within(3));
        // 상태 바의 파일 수도 지운 만큼 줄고, 다시 스캔하라는 안내가 붙는다
        assert_eq!((doc.stats.files, doc.stats.dirs), (1, 2));
        removal_status_frame(&ctx, &mut app, 0.1);
        let output = removal_status_frame(&ctx, &mut app, 0.2);
        assert!(has_removal_text(&output, "⚠ 일부만 지워진 폴더 1개 · 크기가 실제와 다를 수 있음"));
        assert!(has_removal_text(&output, "다시 스캔"));
        assert!(has_removal_text(&output, "1 파일 · 2 폴더"));
        assert!(has_removal_text(&output, "일반 스캔"), "스캔 방법은 따로 배지로 보인다");
    }

    #[test]
    fn recycle_bin_opens_through_shell() {
        reset_shell(true);
        let ctx = egui::Context::default();
        let mut app = app(fixture(1));
        let finished = Finished {
            outcomes: vec![Outcome { node: 2, path: "C:\\Data\\sibling".into(), size: 40, error: None, partial: false }],
            free: None,
        };
        app.delete_dialog = Some(Dialog { doc_id: 1, stage: Stage::Report { method: Method::Recycle, finished } });
        let mut time = 0.0;
        press_text(&ctx, &mut app, "휴지통 열기", &mut time);
        assert_eq!(shell_log(), [Shell::Explore("shell:RecycleBinFolder".into())]);
    }

    #[test]
    fn absorbing_inner_marks_is_announced() {
        let mut app = app(fixture(1));
        app.toggle_mark(3);
        assert!(app.toast.is_none());
        app.toggle_mark(1);
        assert_eq!(app.toast.as_ref().map(|t| t.msg.as_str()), Some("안쪽 표시 1개를 이 폴더 표시에 합쳤습니다"));
        assert_eq!(app.doc.as_ref().unwrap().marks.iter().copied().collect::<Vec<_>>(), [1]);
    }

    #[test]
    fn cleared_marks_can_be_undone() {
        let ctx = egui::Context::default();
        let mut app = app(fixture(1));
        let doc = app.doc.as_mut().unwrap();
        doc.toggle_mark(3).unwrap();
        doc.toggle_mark(2).unwrap();
        let marks = doc.marks.clone();
        let mut time = 0.0;
        app.clear_marks();
        assert!(app.doc.as_ref().unwrap().marks.is_empty());
        assert_eq!(app.toast.as_ref().map(|t| t.msg.as_str()), Some("표시 2개를 모두 해제했습니다"));
        press_text(&ctx, &mut app, "되돌리기", &mut time);
        let doc = app.doc.as_ref().unwrap();
        assert_eq!(doc.marks, marks);
        assert_eq!(doc.mark_state(3), 1);
        assert!(app.toast.is_none());

        // 검토 창(모달)이 열려 있어도 안내가 그 위에 있어 되돌릴 수 있다
        app.clear_marks();
        app.delete_dialog = Some(Dialog {
            doc_id: 1,
            stage: Stage::Review { method: Method::Recycle, confirmed: false, error: None, bin: RecycleBin::Available { capacity: None } },
        });
        press_text(&ctx, &mut app, "되돌리기", &mut time);
        assert_eq!(app.doc.as_ref().unwrap().marks, marks);
        app.delete_dialog = None;

        // 그 사이 새 표시가 생기면 버튼을 숨기고 되돌리지 않는다
        app.clear_marks();
        app.toggle_mark(2);
        let output = full_frame(&ctx, &mut app, vec![], time + 0.1);
        assert!(has_removal_text(&output, "표시 2개를 모두 해제했습니다"));
        assert!(!has_removal_text(&output, "되돌리기"));
        app.undo_clear();
        assert_eq!(app.doc.as_ref().unwrap().marks.iter().copied().collect::<Vec<_>>(), [2]);
        // 스캔 중에는 숨은 이전 문서에 표시를 되살리지 않는다 (끝나면 묻지 않고 사라지므로)
        app.clear_marks();
        let (_tx, rx) = mpsc::channel();
        app.begin_scan("D:\\Other".into(), Arc::default(), rx, None);
        let output = full_frame(&ctx, &mut app, vec![], time + 0.2);
        assert!(has_removal_text(&output, "표시 1개를 모두 해제했습니다"));
        assert!(!has_removal_text(&output, "되돌리기"));
        app.undo_clear();
        assert!(app.doc.as_ref().unwrap().marks.is_empty());
        app.scanning = None;
        app.doc.as_mut().unwrap().toggle_mark(2).unwrap();
        // 트리가 바뀌었거나 다른 문서여도 되돌리지 않는다
        for replace in [false, true] {
            app.clear_marks();
            if replace {
                app.doc = Some(fixture(2));
            } else {
                Arc::get_mut(&mut app.doc.as_mut().unwrap().tree).unwrap().version += 1;
            }
            app.undo_clear();
            assert!(app.doc.as_ref().unwrap().marks.is_empty(), "replace={replace}");
            app.doc.as_mut().unwrap().toggle_mark(2).unwrap();
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

    // ---- 셸 동작·열기 정책·관리자 재실행 ----

    /// shell()이 돌려줄 값을 정하고 기록을 비운다
    fn reset_shell(ok: bool) {
        SHELL_OK.set(ok);
        SHELL_LOG.take();
    }

    /// 지금까지 shell()이 받은 동작을 꺼낸다
    fn shell_log() -> Vec<Shell> {
        SHELL_LOG.take()
    }

    fn pointer(pos: egui::Pos2, pressed: bool) -> Vec<egui::Event> {
        vec![
            egui::Event::PointerMoved(pos),
            egui::Event::PointerButton { pos, button: PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE },
        ]
    }

    fn key(key: Key) -> egui::Event {
        egui::Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers: egui::Modifiers::NONE }
    }

    /// 트리맵에서 같은 자리를 egui 더블클릭 간격 안에 두 번 누른다
    fn double_click(ctx: &egui::Context, app: &mut App, pos: egui::Pos2, time: f64) {
        for (dt, pressed) in [(0.0, true), (0.05, false), (0.1, true), (0.15, false)] {
            frame(ctx, app, pointer(pos, pressed), time + dt);
        }
    }

    /// 화면에 그려진 글자 `text`(정확히 일치)의 가운데
    fn text_pos(output: &egui::FullOutput, text: &str) -> Option<egui::Pos2> {
        fn find(shape: &egui::epaint::Shape, text: &str) -> Option<egui::Pos2> {
            match shape {
                egui::epaint::Shape::Text(s) if s.galley.text() == text => Some(s.pos + s.galley.size() * 0.5),
                egui::epaint::Shape::Vec(shapes) => shapes.iter().find_map(|s| find(s, text)),
                _ => None,
            }
        }
        output.shapes.iter().find_map(|s| find(&s.shape, text))
    }

    /// 이 프레임에 창을 닫으라고 했는지
    fn closes(output: &egui::FullOutput) -> bool {
        output.viewport_output[&egui::ViewportId::ROOT].commands.contains(&egui::ViewportCommand::Close)
    }

    /// 앱 전체를 그리며 `text` 버튼을 누른다. 뗀 프레임의 출력을 돌려준다.
    fn press_text(ctx: &egui::Context, app: &mut App, text: &str, time: &mut f64) -> egui::FullOutput {
        let mut output = full_frame(ctx, app, vec![], *time);
        for _ in 0..2 {
            *time += 0.1;
            output = full_frame(ctx, app, vec![], *time);
        }
        let pos = text_pos(&output, text).unwrap_or_else(|| panic!("{text} 없음"));
        *time += 0.1;
        full_frame(ctx, app, pointer(pos, true), *time);
        *time += 0.05;
        full_frame(ctx, app, pointer(pos, false), *time)
    }

    /// 루트 이름만 다른 문서
    fn rooted(root: &str) -> Doc {
        let mut tree = Tree::default();
        node(&mut tree, root, NO_NODE, Kind::Dir, 100, 1, 1);
        node(&mut tree, "child", 0, Kind::File, 100, 0, 0);
        doc(1, tree)
    }

    /// 실행 파일과 영상 파일이 나란히 있는 문서
    fn launch_fixture() -> Doc {
        let mut tree = Tree::default();
        node(&mut tree, "C:\\Data", NO_NODE, Kind::Dir, 100, 1, 2);
        node(&mut tree, "setup.exe", 0, Kind::File, 50, 0, 0);
        node(&mut tree, "movie.mkv", 0, Kind::File, 50, 0, 0);
        doc(1, tree)
    }

    #[test]
    fn open_refusal_reasons() {
        assert_eq!(open_refusal("movie.mkv", false), None);
        let code = open_refusal("setup.exe", false).unwrap();
        let admin = open_refusal("movie.mkv", true).unwrap();
        assert_ne!(code, admin);
        assert_eq!(open_refusal("setup.exe", true), Some(admin));
    }

    #[test]
    fn double_click_never_runs_code_and_only_reveals_when_elevated() {
        for (elevated, ok) in [(false, true), (false, false), (true, true)] {
            reset_shell(ok);
            let ctx = egui::Context::default();
            let mut app = app(launch_fixture());
            app.elevated = elevated;
            frame(&ctx, &mut app, vec![], 0.0);
            for (time, id) in [(1.0, 1), (2.0, 2)] {
                app.toast = None;
                let pos = app.layout.rect_of(id).unwrap().center();
                double_click(&ctx, &mut app, pos, time);
                let path = app.doc.as_ref().unwrap().tree.path(id);
                let refused = id == 1 || elevated;
                let expected = if refused {
                    vec![Shell::Reveal(path)]
                } else if ok {
                    vec![Shell::Open(path)]
                } else {
                    // 기본 앱으로 열지 못하면 위치를 보여 준다
                    vec![Shell::Open(path.clone()), Shell::Reveal(path)]
                };
                assert_eq!(shell_log(), expected, "elevated={elevated} ok={ok} id={id}");
                let note = app.toast.as_ref().map(|t| t.msg.as_str());
                let name = if id == 1 { "setup.exe" } else { "movie.mkv" };
                assert_eq!(note, open_refusal(name, elevated), "elevated={elevated} id={id}");
                assert_eq!(note.is_some(), refused);
            }
        }
        reset_shell(true);
    }

    #[test]
    fn enter_follows_the_same_open_policy() {
        for elevated in [false, true] {
            reset_shell(true);
            let ctx = egui::Context::default();
            let mut app = app(launch_fixture());
            app.elevated = elevated;
            frame(&ctx, &mut app, vec![], 0.0);
            for (time, id) in [(0.1, 1), (0.2, 2)] {
                app.doc.as_mut().unwrap().selected = Some(id);
                frame(&ctx, &mut app, vec![key(Key::Enter)], time);
                let path = app.doc.as_ref().unwrap().tree.path(id);
                let expected = if id == 1 || elevated { Shell::Reveal(path) } else { Shell::Open(path) };
                assert_eq!(shell_log(), [expected], "elevated={elevated} id={id}");
            }
        }
        // 폴더에서는 지금처럼 확대한다
        let ctx = egui::Context::default();
        let mut app = app(fixture(1));
        app.doc.as_mut().unwrap().selected = Some(1);
        frame(&ctx, &mut app, vec![key(Key::Enter)], 0.0);
        assert_eq!(app.doc.as_ref().unwrap().focus, 1);
        assert!(shell_log().is_empty());
    }

    #[test]
    fn enter_in_search_box_does_not_open_selected_file() {
        reset_shell(true);
        let ctx = egui::Context::default();
        let mut app = app(launch_fixture());
        app.doc.as_mut().unwrap().selected = Some(2);
        app.focus_search = true;
        full_frame(&ctx, &mut app, vec![], 0.0);
        full_frame(&ctx, &mut app, vec![], 0.1);
        assert!(ctx.text_edit_focused());
        // 입력을 끝내는 Enter는 검색 칸이 쓰고, 선택된 파일을 열지 않는다
        full_frame(&ctx, &mut app, vec![key(Key::Enter)], 0.2);
        assert!(!ctx.text_edit_focused());
        assert!(shell_log().is_empty());
        assert_eq!(app.doc.as_ref().unwrap().selected, Some(2));
    }

    /// 오른쪽 클릭 메뉴만 그리며 `text` 항목을 누른다. 메뉴가 삭제 표시를 바꾸라고 했으면 true
    fn press_menu(doc: &mut Doc, n: NodeId, elevated: bool, text: &str) -> bool {
        let ctx = egui::Context::default();
        let mut time = 0.0;
        let draw = |doc: &mut Doc, events: Vec<egui::Event>, time: f64| {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(800.0, 600.0))),
                events,
                time: Some(time),
                ..Default::default()
            };
            let mut toggle = false;
            let mut output = ctx.run_ui(input, |ui| {
                egui::CentralPanel::default().show(ui, |ui| toggle = context_menu(ui, doc, n, elevated));
            });
            output.textures_delta.clear();
            (output, toggle)
        };
        let mut output = draw(doc, vec![], time).0;
        for _ in 0..2 {
            time += 0.1;
            output = draw(doc, vec![], time).0;
        }
        let pos = text_pos(&output, text).unwrap_or_else(|| panic!("{text} 없음"));
        let mut toggled = false;
        for pressed in [true, false] {
            time += 0.05;
            toggled |= draw(doc, pointer(pos, pressed), time).1;
        }
        toggled
    }

    #[test]
    fn context_menu_follows_open_policy() {
        reset_shell(true);
        let mut doc = launch_fixture();
        let (exe, movie) = (doc.tree.path(1), doc.tree.path(2));
        press_menu(&mut doc, 2, false, "📄 열기");
        assert_eq!(shell_log(), [Shell::Open(movie.clone())]);
        // 열지 않는 파일은 '열기'가 꺼져 있고, 위치 보기는 쓸 수 있다
        press_menu(&mut doc, 1, false, "📄 열기");
        press_menu(&mut doc, 2, true, "📄 열기");
        assert!(shell_log().is_empty());
        press_menu(&mut doc, 1, false, "🔎 탐색기에서 위치 보기");
        assert_eq!(shell_log(), [Shell::Reveal(exe)]);
        // 관리자 권한이면 폴더는 탐색기로 직접 연다
        let mut doc = fixture(1);
        let folder = doc.tree.path(1);
        press_menu(&mut doc, 1, false, "📂 폴더 열기");
        press_menu(&mut doc, 1, true, "📂 폴더 열기");
        assert_eq!(shell_log(), [Shell::Open(folder.clone()), Shell::Explore(folder)]);
    }

    #[test]
    fn admin_relaunch_targets_document_root_and_blocks_busy_states() {
        let mut app = app(fixture(1));
        // 경로 칸이 아니라 화면에 있는 문서의 루트를 넘긴다
        app.root_input = "D:\\Other".into();
        assert_eq!(app.admin_relaunch(), AdminRelaunch::Ready(Some("C:\\Data".into())));
        let _scan = install_scan(&mut app);
        assert!(matches!(app.admin_relaunch(), AdminRelaunch::Blocked(_)));
        app.scanning = None;
        app.delete_dialog = Some(Dialog {
            doc_id: 1,
            stage: Stage::Review { method: Method::Recycle, confirmed: false, error: None, bin: RecycleBin::Unavailable("테스트") },
        });
        assert!(matches!(app.admin_relaunch(), AdminRelaunch::Blocked(_)));
        app.delete_dialog = None;
        app.begin_removal(1, vec![1]);
        assert!(matches!(app.admin_relaunch(), AdminRelaunch::Blocked(_)));
        app.pending_removal = None;
        app.elevated = true;
        assert_eq!(app.admin_relaunch(), AdminRelaunch::Hidden);
        app.elevated = false;
        app.doc = None;
        assert_eq!(app.admin_relaunch(), AdminRelaunch::Ready(None));

        app.drives = vec![
            Drive { root: "Z:\\".into(), kind: DRIVE_REMOTE },
            Drive { root: "Y:\\".into(), kind: DRIVE_FIXED },
        ];
        for (root, network) in [
            (r"\\server\share", true),
            (r"\\?\UNC\server\share", true),
            (r"Z:\Data", true),
            (r"z:\", true),
            (r"\\?\Z:\Data", true),
            (r"Y:\Data", false),
            (r"\\?\C:\Data", false),
        ] {
            app.doc = Some(rooted(root));
            let state = app.admin_relaunch();
            if network {
                assert!(matches!(state, AdminRelaunch::Blocked(_)), "{root}: {state:?}");
            } else {
                assert_eq!(state, AdminRelaunch::Ready(Some(root.into())), "{root}");
            }
        }

        assert!(relaunch_hint(None).contains("드라이브를 고르면"));
        assert!(relaunch_hint(Some("C:\\")).contains("$MFT를 직접 읽어"));
        assert!(relaunch_hint(Some("C:\\Data")).contains("드라이브 전체를 고를 때만"));
    }

    #[test]
    fn relaunch_without_marks_elevates_and_closes() {
        for ok in [true, false] {
            reset_shell(ok);
            let ctx = egui::Context::default();
            let mut app = app(fixture(1));
            let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
                app.request_relaunch(ui.ctx(), Some("C:\\Data".into()));
            });
            output.textures_delta.clear();
            assert_eq!(shell_log(), [Shell::Elevate(Some("C:\\Data".into()))]);
            assert_eq!(closes(&output), ok);
            assert!(app.confirm.is_none());
            // UAC 취소나 실패는 창을 그대로 두고 알린다
            assert_eq!(app.toast.is_some(), !ok);
        }
        reset_shell(true);
    }

    #[test]
    fn toolbar_relaunch_passes_document_root_and_waits_for_scan() {
        reset_shell(true);
        let ctx = egui::Context::default();
        let mut app = app(fixture(1));
        app.root_input = "D:\\Other".into();
        let mut time = 0.0;
        let scan = install_scan(&mut app);
        let output = press_text(&ctx, &mut app, "🛡 관리자로 실행", &mut time);
        assert!(shell_log().is_empty());
        assert!(!closes(&output));
        drop(scan);
        app.scanning = None;
        let output = press_text(&ctx, &mut app, "🛡 관리자로 실행", &mut time);
        assert_eq!(shell_log(), [Shell::Elevate(Some("C:\\Data".into()))]);
        assert!(closes(&output));
    }

    #[test]
    fn relaunch_with_marks_asks_before_closing() {
        reset_shell(true);
        let ctx = egui::Context::default();
        let mut app = app(fixture(1));
        let doc = app.doc.as_mut().unwrap();
        doc.toggle_mark(3).unwrap();
        doc.selected = Some(3);
        let marks = doc.marks.clone();
        let root = Some("C:\\Data".to_owned());
        let mut time = 0.0;
        for answer in [Some("취소"), None, Some("관리자로 다시 열기")] {
            app.request_relaunch(&ctx, root.clone());
            assert_eq!(app.confirm, Some(Confirm::Relaunch(root.clone())));
            assert!(shell_log().is_empty());
            // 모달이 열린 동안에는 Space가 표시를 바꾸지 않는다
            let mut output = full_frame(&ctx, &mut app, vec![key(Key::Space)], time);
            for _ in 0..3 {
                time += 0.1;
                output = full_frame(&ctx, &mut app, vec![], time);
            }
            assert_eq!(app.doc.as_ref().unwrap().marks, marks);
            assert!(has_removal_text(&output, "삭제 표시 1개(60 B)는 새 창으로 넘어가지 않습니다."));
            assert!(has_removal_text(&output, "새 창에서 처음부터 다시 스캔합니다: C:\\Data"));
            let output = match answer {
                Some(text) => press_text(&ctx, &mut app, text, &mut time),
                // Esc는 취소이고, 모달이 먹으므로 선택도 그대로다
                None => {
                    time += 0.1;
                    full_frame(&ctx, &mut app, vec![key(Key::Escape)], time)
                }
            };
            let proceed = answer == Some("관리자로 다시 열기");
            assert!(app.confirm.is_none(), "{answer:?}");
            assert_eq!(closes(&output), proceed, "{answer:?}");
            let expected = if proceed { vec![Shell::Elevate(root.clone())] } else { vec![] };
            assert_eq!(shell_log(), expected, "{answer:?}");
            let doc = app.doc.as_ref().unwrap();
            assert_eq!(doc.marks, marks);
            assert_eq!(doc.selected, Some(3));
        }
    }

    #[test]
    fn elevated_scan_result_uses_admin_guard() {
        for elevated in [false, true] {
            let mut app = app(fixture(1));
            app.elevated = elevated;
            let scan = install_scan(&mut app);
            let mut tree = Tree::default();
            node(&mut tree, "Z:\\Program Files", NO_NODE, Kind::Dir, 100, 1, 1);
            node(&mut tree, "App", 0, Kind::Dir, 100, 2, 1);
            node(&mut tree, "cache", 1, Kind::File, 100, 0, 0);
            let result = doc(9, tree);
            scan.send(Ok(ScanResult { tree: Arc::try_unwrap(result.tree).ok().unwrap(), stats: result.stats })).unwrap();
            app.poll_scan();
            assert_eq!(app.doc.as_ref().unwrap().guard, if elevated { Guard::Admin } else { Guard::User });
            app.toggle_mark(1);
            let doc = app.doc.as_ref().unwrap();
            if elevated {
                assert!(doc.marks.is_empty());
                let note = app.toast.as_ref().map(|t| t.msg.as_str());
                assert_eq!(note, Some("보호된 항목: 설치된 프로그램 폴더 (관리자 권한)"));
                // 더 깊은 하위는 관리자 권한에서도 표시할 수 있다
                app.toggle_mark(2);
                assert_eq!(app.doc.as_ref().unwrap().marks.iter().copied().collect::<Vec<_>>(), [2]);
            } else {
                assert_eq!(doc.marks.iter().copied().collect::<Vec<_>>(), [1]);
            }
        }
    }

    // ---- 스캔 중지·다시 스캔 ----

    const RESTORED: &str = "스캔을 중지했습니다 · 이전 결과를 그대로 보여 줍니다";

    /// 스캔 스레드가 보낼 결과
    fn scan_result(doc: Doc) -> io::Result<ScanResult> {
        Ok(ScanResult { tree: Arc::try_unwrap(doc.tree).ok().unwrap(), stats: doc.stats })
    }

    /// 중지한 일반 순회의 일부 결과: folder(1)를 다 읽지 못했다
    fn partial_result() -> Doc {
        let mut partial = fixture(9);
        let tree = Arc::get_mut(&mut partial.tree).unwrap();
        for id in [0, 1] {
            tree.nodes[id].flags |= flags::INCOMPLETE;
        }
        partial.stats.cancelled = true;
        partial
    }

    /// 있을 리 없는 경로. 스캔은 열지 못하고 곧바로 끝나며, 테스트는 그 결과를 보지 않는다.
    fn missing_root() -> String {
        std::env::temp_dir().join(format!("darak-test-missing-{}", std::process::id())).display().to_string()
    }

    /// 툴바만 한 프레임 그린다
    fn toolbar_frame(ctx: &egui::Context, app: &mut App, time: f64) -> egui::FullOutput {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1280.0, 820.0))),
            time: Some(time),
            ..Default::default()
        };
        let mut output = ctx.run_ui(input, |ui| {
            egui::Panel::top("toolbar-test").show(ui, |ui| app.toolbar(ui));
        });
        output.textures_delta.clear();
        output
    }

    /// 스캔 화면만 한 프레임 그린다
    fn scan_view_frame(ctx: &egui::Context, app: &mut App, time: f64) -> egui::FullOutput {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1280.0, 820.0))),
            time: Some(time),
            ..Default::default()
        };
        let mut output = ctx.run_ui(input, |ui| {
            egui::CentralPanel::default().show(ui, |ui| app.scan_view(ui));
        });
        output.textures_delta.clear();
        output
    }

    #[test]
    fn stop_with_previous_document_restores_it_immediately() {
        let mut app = app(fixture(1));
        let doc = app.doc.as_mut().unwrap();
        doc.toggle_mark(3).unwrap();
        doc.navigate(1);
        let (tx, rx) = mpsc::channel();
        app.begin_scan("D:\\Other".into(), Arc::default(), rx, None);
        assert_eq!(app.root_input, "D:\\Other");
        // 스캔 스레드가 정리를 마칠 때까지 기다리지 않는다
        app.cancel_scan();
        app.poll_scan();
        assert!(app.scanning.is_none());
        let doc = app.doc.as_ref().unwrap();
        assert_eq!((doc.id, doc.focus), (1, 1));
        assert_eq!(doc.marks.iter().copied().collect::<Vec<_>>(), [3]);
        assert_eq!(app.root_input, "C:\\Data");
        assert!(app.error.is_none());
        assert_eq!(app.toast.as_ref().map(|t| t.msg.as_str()), Some(RESTORED));
        // 받는 쪽이 없으므로 부분 결과는 나중에도 들어올 수 없다
        assert!(tx.send(scan_result(partial_result())).is_err());
        assert_eq!(app.doc.as_ref().unwrap().id, 1);
    }

    #[test]
    fn search_results_refill_after_restore() {
        let (mut app, runtime, wake) = running_app(|_| {});
        app.doc.as_mut().unwrap().search.query = "sibling".into();
        pump_until(&mut app, &wake, |app| app.doc.as_ref().unwrap().search.result.is_some());
        let (_tx, rx) = mpsc::channel();
        app.begin_scan("D:\\Other".into(), Arc::default(), rx, None);
        assert!(app.doc.as_ref().unwrap().search.result.is_none());
        app.cancel_scan();
        app.poll_scan();
        assert!(app.scanning.is_none());
        // 다른 경로와 같은 재개 처리를 거쳐 같은 검색어로 다시 찾는다
        pump_until(&mut app, &wake, |app| app.doc.as_ref().unwrap().search.result.is_some());
        let doc = app.doc.as_ref().unwrap();
        assert_eq!(doc.search.query, "sibling");
        assert_eq!(doc.search.result.as_ref().unwrap().hits, [2]);
        shutdown(runtime);
    }

    #[test]
    fn stop_without_document_waits_then_shows_partial() {
        let ctx = egui::Context::default();
        let mut app = app(fixture(1));
        app.doc = None;
        let tx = preview_scan(&mut app);
        app.cancel_scan();
        app.poll_scan();
        // 이전 문서가 없으면 스캔 스레드가 읽은 데까지 정리하기를 기다린다
        assert!(app.scanning.is_some());
        removal_status_frame(&ctx, &mut app, 0.0);
        let output = removal_status_frame(&ctx, &mut app, 0.1);
        assert!(has_removal_text(&output, "중지하는 중…"));
        assert!(!has_removal_text(&output, "스캔 중…"));
        assert!(!has_removal_text(&output, "중지하면"));
        scan_view_frame(&ctx, &mut app, 0.2);
        let output = scan_view_frame(&ctx, &mut app, 0.3);
        assert!(has_removal_text(&output, "중지하는 중…"));
        assert!(has_removal_text(&output, "읽은 데까지 정리하는 중입니다"));

        tx.send(scan_result(partial_result())).unwrap();
        app.poll_scan();
        assert!(app.scanning.is_none());
        assert!(app.doc.as_ref().unwrap().stats.cancelled);
        // 표시도 검토도 막는다
        app.toggle_mark(3);
        assert!(app.doc.as_ref().unwrap().marks.is_empty());
        assert!(app.toast.as_ref().is_some_and(|t| t.msg.contains("삭제 표시를 할 수 없습니다")));
        app.doc.as_mut().unwrap().marks.insert(3);
        app.open_delete_review();
        assert!(app.delete_dialog.is_none());
        app.doc.as_mut().unwrap().marks.clear();
        // 우클릭 메뉴의 표시 항목도 꺼져 있다
        assert!(!press_menu(app.doc.as_mut().unwrap(), 3, false, "🗑 삭제 표시"));
        assert!(press_menu(&mut fixture(1), 3, false, "🗑 삭제 표시  (Space)"));
        // 배너가 까닭과 다시 스캔을 알린다
        full_frame(&ctx, &mut app, vec![], 1.0);
        let output = full_frame(&ctx, &mut app, vec![], 1.1);
        assert!(has_removal_text(&output, "스캔을 중지한 일부 결과입니다"));
        assert!(has_removal_text(&output, "↻ 다시 스캔"));
        assert!(has_removal_text(&output, "· 중지됨(일부 결과)"));
    }

    #[test]
    fn stopped_scan_with_nothing_read_returns_to_welcome() {
        for interrupted in [false, true] {
            let mut app = app(fixture(1));
            app.doc = None;
            let tx = preview_scan(&mut app);
            app.cancel_scan();
            if interrupted {
                // MFT는 중지하면 부분 결과 없이 Interrupted를 돌려준다
                tx.send(Err(io::Error::new(io::ErrorKind::Interrupted, "스캔을 중지했습니다"))).unwrap();
            } else {
                let mut tree = Tree::default();
                node(&mut tree, "C:\\Data", NO_NODE, Kind::Dir, 0, 0, 0);
                let mut empty = doc(9, tree);
                empty.stats.cancelled = true;
                tx.send(scan_result(empty)).unwrap();
            }
            app.poll_scan();
            assert!(app.scanning.is_none(), "{interrupted}");
            assert!(app.doc.is_none() && app.error.is_none(), "{interrupted}");
            assert_eq!(app.toast.as_ref().map(|t| t.msg.as_str()), Some("스캔을 중지했습니다"));
        }
    }

    #[test]
    fn scan_with_marks_waits_for_confirmation() {
        let ctx = egui::Context::default();
        let mut app = app(fixture(1));
        app.doc.as_mut().unwrap().toggle_mark(3).unwrap();
        let marks = app.doc.as_ref().unwrap().marks.clone();
        let missing = missing_root();
        let mut time = 0.0;
        // 다른 루트는 '새로', 같은 루트는 '다시'. 취소와 Esc는 아무것도 바꾸지 않는다
        for (other, title, answer) in [(true, "새로 스캔할까요?", Some("취소")), (false, "다시 스캔할까요?", None)] {
            if other { app.request_scan(missing.clone()) } else { app.rescan() }
            let root = if other { normalize_root(&missing) } else { "C:\\Data".into() };
            assert_eq!(app.confirm, Some(Confirm::Scan(root.clone())));
            assert!(app.scanning.is_none());
            let mut output = full_frame(&ctx, &mut app, vec![], time);
            for _ in 0..3 {
                time += 0.1;
                output = full_frame(&ctx, &mut app, vec![], time);
            }
            assert!(has_removal_text(&output, title), "{title}");
            assert!(has_removal_text(&output, "삭제 표시 1개(60 B)는 스캔이 끝나면 사라집니다."));
            assert!(has_removal_text(&output, &format!("스캔할 위치: {root}")));
            assert!(has_removal_text(&output, "스캔을 중지하면 지금 결과와 표시가 그대로 남습니다."));
            match answer {
                Some(text) => {
                    press_text(&ctx, &mut app, text, &mut time);
                }
                None => {
                    time += 0.1;
                    full_frame(&ctx, &mut app, vec![key(Key::Escape)], time);
                }
            }
            assert!(app.confirm.is_none(), "{title}");
            assert!(app.scanning.is_none(), "{title}");
            assert_eq!(app.doc.as_ref().unwrap().marks, marks);
        }
        // 진행하면 스캔을 시작한다. 표시는 스캔이 끝날 때까지 그대로다 (중지하면 남는다)
        app.request_scan(missing.clone());
        press_text(&ctx, &mut app, "스캔", &mut time);
        assert!(app.confirm.is_none());
        assert_eq!(app.scanning.as_ref().map(|s| s.root.clone()), Some(normalize_root(&missing)));
        assert_eq!(app.doc.as_ref().unwrap().marks, marks);
        // 스캔 중에는 새로 요청해도 묻지도 시작하지도 않는다
        let scan_id = app.scanning.as_ref().unwrap().scan_id;
        app.request_scan("D:\\Other".into());
        assert!(app.confirm.is_none());
        assert_eq!(app.scanning.as_ref().unwrap().scan_id, scan_id);
        // 표시가 없으면 묻지 않고 바로 시작한다
        app.cancel_scan();
        app.scanning = None;
        app.doc.as_mut().unwrap().clear_marks();
        app.request_scan(missing.clone());
        assert!(app.confirm.is_none());
        assert!(app.scanning.is_some());
    }

    #[test]
    fn f5_and_rescan_button_ask_when_marked() {
        let ctx = egui::Context::default();
        let mut app = app(fixture(1));
        app.doc.as_mut().unwrap().toggle_mark(3).unwrap();
        frame(&ctx, &mut app, vec![key(Key::F5)], 0.0);
        assert_eq!(app.confirm, Some(Confirm::Scan("C:\\Data".into())));
        assert!(app.scanning.is_none());
        app.confirm = None;
        let mut time = 1.0;
        press_text(&ctx, &mut app, "↻", &mut time);
        assert_eq!(app.confirm, Some(Confirm::Scan("C:\\Data".into())));
        assert!(app.scanning.is_none());
        // 표시가 없으면 문서 루트를 바로 다시 스캔한다
        let missing = missing_root();
        let ctx = egui::Context::default();
        let mut plain = self::app(rooted(&missing));
        frame(&ctx, &mut plain, vec![key(Key::F5)], 0.0);
        assert!(plain.confirm.is_none());
        assert_eq!(plain.scanning.as_ref().map(|s| s.root.clone()), Some(normalize_root(&missing)));
    }

    #[test]
    fn rescan_same_root_keeps_focus_and_query() {
        let mut app = app(fixture(1));
        let doc = app.doc.as_mut().unwrap();
        doc.navigate(1);
        doc.search.query = "child".into();
        doc.search.in_focus_only = true;
        let tx = preview_scan(&mut app);
        tx.send(scan_result(fixture(9))).unwrap();
        app.poll_scan();
        let doc = app.doc.as_ref().unwrap();
        assert_eq!(doc.id, 2);
        assert_eq!(doc.tree.name(doc.focus), "folder");
        assert_eq!(doc.search.query, "child");
        assert!(doc.search.in_focus_only);
        assert!(doc.back.is_empty());
        // 미리 보기는 루트 기준이라 하위 폴더로 돌아가면 모양을 보간하지 않는다
        assert!(app.morph_until.is_none());
        // 루트가 다르면 처음부터 본다
        let tx = preview_scan(&mut app);
        tx.send(scan_result(rooted("D:\\Data"))).unwrap();
        app.poll_scan();
        let doc = app.doc.as_ref().unwrap();
        assert_eq!((doc.id, doc.focus), (3, Tree::ROOT));
        assert!(doc.search.query.is_empty() && !doc.search.in_focus_only);
        assert!(app.morph_until.is_some());
    }

    #[test]
    fn scanning_hides_previous_document_controls() {
        let ctx = egui::Context::default();
        let mut app = app(fixture(1));
        let draw = |app: &mut App, time: f64| {
            toolbar_frame(&ctx, app, time);
            toolbar_frame(&ctx, app, time + 0.05)
        };
        let shown = ["⬅", "↻", "C:\\Data", "🔎 검색"];
        let output = draw(&mut app, 0.0);
        for text in shown {
            assert!(has_removal_text(&output, text), "{text}");
        }
        let (_tx, rx) = mpsc::channel();
        app.begin_scan("D:\\Other".into(), Arc::default(), rx, None);
        assert!(!app.can_start_scan());
        let output = draw(&mut app, 1.0);
        for text in shown.into_iter().chain(["⏹ 중지"]) {
            assert!(!has_removal_text(&output, text), "{text}");
        }
        // 스캔 시작 컨트롤은 꺼진 채 남는다 (중지는 스캔 화면에만 있다)
        assert!(has_removal_text(&output, "🔍 스캔"));
        // 상태 바는 이전 결과를 들고 있다고 알린다
        removal_status_frame(&ctx, &mut app, 1.2);
        let output = removal_status_frame(&ctx, &mut app, 1.3);
        assert!(has_removal_text(&output, "D:\\Other 스캔 중…"));
        assert!(has_removal_text(&output, "· 중지하면 C:\\Data의 이전 결과로 돌아갑니다"));
        app.cancel_scan();
        app.poll_scan();
        assert!(app.can_start_scan());
        let output = draw(&mut app, 2.0);
        for text in shown {
            assert!(has_removal_text(&output, text), "{text}");
        }
        // 상태 바에 스캔 시각
        removal_status_frame(&ctx, &mut app, 2.2);
        let output = removal_status_frame(&ctx, &mut app, 2.3);
        let at = scanned_label(app.doc.as_ref().unwrap().scanned_at);
        assert!(has_removal_text(&output, &format!("· {at} 스캔")), "{at}");
    }

    #[test]
    fn breadcrumb_marks_incomplete_ancestors() {
        let ctx = egui::Context::default();
        let mut d = fixture(1);
        Arc::get_mut(&mut d.tree).unwrap().nodes[0].flags |= flags::INCOMPLETE;
        d.navigate(1);
        let mut app = app(d);
        toolbar_frame(&ctx, &mut app, 0.0);
        let output = toolbar_frame(&ctx, &mut app, 0.1);
        // 루트(조상)는 다 세지 못했고, 현재 위치 folder는 다 셌다
        assert!(has_removal_text(&output, "C:\\Data ⚠"));
        assert!(!has_removal_text(&output, "folder ⚠"));
        assert!(!has_removal_text(&output, " ⚠  ·"));
    }

    #[test]
    fn explains_unread_folders() {
        let mut tree = Tree::default();
        node(&mut tree, "C:\\Data", NO_NODE, Kind::Dir, 100, 1, 5);
        for name in ["a", "b", "c", "d", "e"] {
            node(&mut tree, name, 0, Kind::Dir, 20, 0, 0);
        }
        let mut d = doc(1, tree);
        assert_eq!(incomplete_note(&d, 0), None);
        assert_eq!(unread_children(&d.tree, 0), None);
        let tree = Arc::get_mut(&mut d.tree).unwrap();
        tree.nodes[0].flags |= flags::INCOMPLETE;
        tree.nodes[1].flags |= flags::DENIED;
        for id in 2..5 {
            tree.nodes[id].flags |= flags::INCOMPLETE;
        }
        assert_eq!(incomplete_note(&d, 0), Some("읽지 못한 하위 폴더가 있어 실제 크기는 더 클 수 있습니다"));
        assert_eq!(unread_children(&d.tree, 0).as_deref(), Some("이어지는 하위 폴더: a, b, c 외 1개"));
        assert_eq!(incomplete_note(&d, 1), Some("접근이 거부되어 일부 크기를 모릅니다"));
        assert_eq!(incomplete_note(&d, 5), None);
        // 하위 폴더 없이 INCOMPLETE면 중지한 스캔에서만 '중지해 읽지 못한 폴더'다
        assert_eq!(incomplete_note(&d, 2), Some("읽지 못한 하위 폴더가 있어 실제 크기는 더 클 수 있습니다"));
        d.stats.cancelled = true;
        assert!(incomplete_note(&d, 2).unwrap().starts_with("스캔을 중지해"));
        assert!(incomplete_note(&d, 0).unwrap().starts_with("읽지 못한 하위 폴더가"));
        // 지운 폴더는 이어지는 곳이 아니다
        d.apply_removed(&[4]);
        assert_eq!(unread_children(&d.tree, 0).as_deref(), Some("이어지는 하위 폴더: a, b, c"));
    }

    #[test]
    fn scan_time_is_short_for_today() {
        let today = scanned_label(now_secs2000());
        assert!(today.len() == 5 && today.as_bytes()[2] == b':', "{today}");
        // 2000-01-31 UTC (어느 시간대에서도 2000년 1월)
        let old = scanned_label(86_400 * 30);
        assert!(old.starts_with("2000-01-") && old.len() == 16, "{old}");
        assert_eq!(scanned_label(0), "-");
    }

    #[test]
    fn status_bar_shows_admin_badge_only_when_elevated() {
        for elevated in [false, true] {
            for (error, has_doc) in [(None, true), (Some("스캔 오류"), true), (None, false)] {
                let ctx = egui::Context::default();
                let mut app = app(fixture(1));
                app.elevated = elevated;
                app.error = error.map(Into::into);
                if !has_doc {
                    app.doc = None;
                }
                removal_status_frame(&ctx, &mut app, 0.0);
                let output = removal_status_frame(&ctx, &mut app, 0.1);
                assert_eq!(has_removal_text(&output, "관리자 권한"), elevated, "{error:?} {has_doc}");
                if let Some(error) = error {
                    assert!(has_removal_text(&output, error));
                }
            }
        }
    }

    const GB: u64 = 1 << 30;

    /// 가짜 용량 조회의 답: (경로, (전체, 여유), 파일 시스템)
    type Answer = (&'static str, Option<(u64, u64)>, Option<&'static str>);

    /// 경로마다 정해 둔 답을 돌려주는 가짜 용량 조회 (장치에 묻지 않는다)
    fn answering(answers: &'static [Answer]) -> Volumes {
        Volumes::with_probe(None, Arc::new(|path: &str| {
            let (_, space, fs) = answers.iter().find(|a| a.0 == path).copied().unwrap_or(("", None, None));
            Probe { space, fs: fs.map(Into::into) }
        }))
    }

    #[test]
    fn status_bar_shows_fallback_reason_without_hover() {
        let not_elevated = "일반 스캔 · 관리자 권한 없음";
        for (method, reason, badge) in [
            (ScanMethod::Walk, Some(Fallback::NotElevated), not_elevated),
            (ScanMethod::Walk, Some(Fallback::Network), "일반 스캔 · 네트워크 드라이브"),
            (ScanMethod::Walk, Some(Fallback::NotNtfs("exFAT".into())), "일반 스캔 · exFAT 볼륨"),
            (ScanMethod::Walk, None, "일반 스캔"),
            (ScanMethod::Mft, None, "MFT"),
        ] {
            let ctx = egui::Context::default();
            let mut app = app(fixture(1));
            let st = &mut app.doc.as_mut().unwrap().stats;
            (st.method, st.fallback_reason) = (method, reason.clone());
            removal_status_frame(&ctx, &mut app, 0.0);
            let output = removal_status_frame(&ctx, &mut app, 0.1);
            assert!(has_removal_text(&output, badge), "{reason:?}");
            assert_eq!(has_removal_text(&output, "일반 스캔"), method == ScanMethod::Walk, "{reason:?}");
            assert_eq!(has_removal_text(&output, not_elevated), reason == Some(Fallback::NotElevated), "{reason:?}");
        }
        // 툴팁은 전체 문장이고, 권한 때문이면 관리자로 실행하라고 권한다
        let mut st = fixture(1).stats;
        st.fallback_reason = Some(Fallback::NotElevated);
        let (_, hint) = method_badge(&st);
        assert!(hint.starts_with("관리자 권한이 없어 일반 스캔을 사용") && hint.contains("관리자로 실행하면"), "{hint}");
        st.fallback_reason = Some(Fallback::Network);
        assert!(!method_badge(&st).1.contains("관리자로 실행"));
    }

    #[test]
    fn status_bar_never_waits_for_disk_space() {
        let ctx = egui::Context::default();
        // 끊긴 네트워크 드라이브처럼 문이 열릴 때까지 답하지 않는다
        let (open, gate) = mpsc::channel::<()>();
        let gate = std::sync::Mutex::new(gate);
        let mut blocked = app(fixture(1));
        blocked.volumes = Volumes::with_probe(None, Arc::new(move |_: &str| {
            let _ = gate.lock().unwrap().recv_timeout(Duration::from_secs(10));
            Probe { space: Some((476 * GB, 112 * GB)), fs: None }
        }));
        let started = Instant::now();
        for frame in 0..5 {
            let output = removal_status_frame(&ctx, &mut blocked, frame as f64 * 0.1);
            assert!(!has_removal_text(&output, "여유 공간"));
        }
        assert!(started.elapsed() < Duration::from_secs(5), "the UI thread waited for the probe");
        open.send(()).unwrap();
        blocked.volumes.wait_reply();
        let output = removal_status_frame(&ctx, &mut blocked, 0.6);
        assert!(has_removal_text(&output, &format!("여유 공간 {} / {}", human_size(112 * GB), human_size(476 * GB))));

        // 믿을 수 없는 값(2^63 B)은 숫자 없이 알린다
        let mut sentinel = app(fixture(1));
        sentinel.volumes = answering(&[("C:\\Data", Some((1 << 63, 1 << 63)), None)]);
        removal_status_frame(&ctx, &mut sentinel, 1.0);
        sentinel.volumes.wait_reply();
        let output = removal_status_frame(&ctx, &mut sentinel, 1.1);
        assert!(has_removal_text(&output, "여유 공간 정보 없음"));
        assert!(!has_removal_text(&output, "PB"));
    }

    #[test]
    fn welcome_cards_read_cached_capacity() {
        static ANSWERS: &[Answer] = &[
            ("C:\\", Some((476 * GB, 112 * GB)), Some("NTFS")),
            ("D:\\", Some((100 * GB, 5 * GB)), Some("NTFS")),
            ("E:\\", Some((64 * GB, 32 * GB)), Some("exFAT")),
            // 매체 없는 카드 리더
            ("R:\\", None, None),
            // 클라우드·가상 네트워크 드라이브의 표지값 (8192 PB)
            ("Y:\\", Some((1 << 63, 1 << 63)), None),
        ];
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let mut app = app(fixture(1));
        app.doc = None;
        app.volumes = answering(ANSWERS);
        app.drives = ANSWERS
            .iter()
            .map(|&(root, ..)| Drive {
                root: root.into(),
                kind: match root {
                    "E:\\" | "R:\\" => DRIVE_REMOVABLE,
                    "Y:\\" => DRIVE_REMOTE,
                    _ => DRIVE_FIXED,
                },
            })
            .collect();
        // 느린 기기에서 3초가 지나도 welcome이 이 기기의 실제 목록으로 바꾸지 않게 한다
        app.drives_at = Instant::now() + Duration::from_secs(3600);
        // 답이 오기 전: 기다리지 않고 '확인 중'으로 그린다. 이동식은 답이 올 때까지 숨긴다
        let output = full_frame(&ctx, &mut app, vec![], 0.0);
        assert!(has_removal_text(&output, "용량 확인 중…"));
        assert!(!has_removal_text(&output, "E:\\") && !has_removal_text(&output, "R:\\"));
        for _ in ANSWERS {
            app.volumes.wait_reply();
        }
        full_frame(&ctx, &mut app, vec![], 0.1);
        let output = full_frame(&ctx, &mut app, vec![], 0.2);
        let free = |total: u64, free: u64| format!("여유 공간 {} / {}", human_size(free * GB), human_size(total * GB));
        for text in ["로컬 디스크 · NTFS", "이동식 · exFAT", &free(476, 112), &free(100, 5), "· 거의 가득 참", "네트워크 · 일반 스캔이라 느릴 수 있음", "용량 정보 없음"] {
            assert!(has_removal_text(&output, text), "{text}");
        }
        assert!(!has_removal_text(&output, "PB"), "표지값을 용량처럼 보여 주지 않는다");
        assert!(!has_removal_text(&output, "R:\\"), "매체 없는 카드 리더는 숨긴다");
        assert!(!has_removal_text(&output, "용량 확인 중…"));
        // 카드는 화면 낭독기에 이름 있는 버튼이다
        let update = output.platform_output.accesskit_update.expect("accesskit tree");
        let names: Vec<_> = update.nodes.iter().filter(|(_, n)| n.role() == egui::accesskit::Role::Button).filter_map(|(_, n)| n.label()).collect();
        for name in [format!("C:\\ 로컬 디스크 · NTFS · {}", free(476, 112)), format!("D:\\ 로컬 디스크 · NTFS · {} · 거의 가득 참", free(100, 5)),
            "Y:\\ 네트워크 · 일반 스캔이라 느릴 수 있음 · 용량 정보 없음".into()] {
            assert!(names.contains(&name.as_str()), "{name}: {names:?}");
        }
    }

    #[test]
    fn drive_lists_hide_removables_until_media_answers() {
        for (kind, space, shown) in [
            (DRIVE_REMOVABLE, Space::Checking, false),
            (DRIVE_REMOVABLE, Space::Unavailable, false),
            (DRIVE_REMOVABLE, Space::Unknown, true),
            (DRIVE_REMOVABLE, Space::Known(64 * GB, GB), true),
            (DRIVE_FIXED, Space::Checking, true),
            (DRIVE_REMOTE, Space::Unavailable, true),
        ] {
            assert_eq!(shows_drive(kind, space), shown, "{kind} {space:?}");
        }
        assert_eq!(space_note(Space::Unavailable), "용량을 읽을 수 없습니다");
        let net = Drive { root: "Z:\\".into(), kind: DRIVE_REMOTE };
        assert_eq!(drive_subtitle(&net, Some("NTFS")), "네트워크 · 일반 스캔이라 느릴 수 있음");
        assert_eq!(drive_subtitle(&Drive { root: "C:\\".into(), kind: DRIVE_FIXED }, None), "로컬 디스크");
    }

    #[test]
    fn drive_scan_progress_basis_arrives_from_the_cache() {
        let mut app = app(fixture(1));
        app.volumes = answering(&[("D:\\", Some((100 * GB, 40 * GB)), Some("NTFS"))]);
        let _tx = install_scan(&mut app);
        app.fill_progress_basis();
        assert_eq!(app.scanning.as_ref().unwrap().used, None, "폴더는 볼륨 사용량을 기준으로 쓰지 않는다");
        app.scanning.as_mut().unwrap().root = "D:\\".into();
        app.fill_progress_basis();
        assert_eq!(app.scanning.as_ref().unwrap().used, None, "답이 오기 전에는 기다리지 않는다");
        app.volumes.wait_reply();
        app.fill_progress_basis();
        assert_eq!(app.scanning.as_ref().unwrap().used, Some(60 * GB));
    }

    #[test]
    fn rescanning_the_same_walk_uses_its_size_as_progress_basis() {
        let mut d = fixture(1);
        let alloc = d.tree.node(Tree::ROOT).alloc;
        assert_eq!(previous_walk_size(Some(&d), "C:\\Data"), Some(alloc));
        assert_eq!(previous_walk_size(Some(&d), "c:\\data"), Some(alloc));
        assert_eq!(previous_walk_size(Some(&d), "C:\\Other"), None);
        assert_eq!(previous_walk_size(None, "C:\\Data"), None);
        d.stats.cancelled = true;
        assert_eq!(previous_walk_size(Some(&d), "C:\\Data"), None, "중지한 일부 결과");
        d.stats.cancelled = false;
        d.stats.method = ScanMethod::Mft;
        assert_eq!(previous_walk_size(Some(&d), "C:\\Data"), None, "MFT는 볼륨 전체를 센다");
    }

    #[test]
    fn finished_scans_and_applied_removals_ask_disk_space_again() {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = Arc::clone(&calls);
        let probe: crate::drives::ProbeFn = Arc::new(move |_: &str| {
            counted.fetch_add(1, Relaxed);
            Probe { space: Some((100 * GB, 40 * GB)), fs: None }
        });
        let ask = |app: &mut App, expected: usize| {
            assert_eq!(app.volumes.space("C:\\Data") == Space::Checking, expected == 1, "the old value stays while asking again");
            app.volumes.wait_reply();
            assert_eq!(app.volumes.space("C:\\Data"), Space::Known(100 * GB, 40 * GB));
            assert_eq!(calls.load(Relaxed), expected);
        };
        // 삭제 결과를 반영하면 여유 공간을 다시 묻는다
        let mut removing = app(fixture(1));
        removing.volumes = Volumes::with_probe(None, Arc::clone(&probe));
        ask(&mut removing, 1);
        removing.begin_removal(1, vec![1]);
        let fence = removing.pending_removal.as_ref().unwrap().fence;
        removing.search_control(SearchControlEvent::Released(fence));
        removing.finish_removal();
        assert_eq!(removing.doc.as_ref().unwrap().tree.version, 1);
        ask(&mut removing, 2);

        calls.store(0, Relaxed);
        let mut app = app(fixture(1));
        app.volumes = Volumes::with_probe(None, probe);
        ask(&mut app, 1);
        // 사용자가 중지하지 않은 스캔이 끝났다 (성공·실패 모두)
        let tx = install_scan(&mut app);
        assert!(!app.poll_scan());
        tx.send(scan_result(fixture(2))).unwrap();
        assert!(app.poll_scan());
        ask(&mut app, 2);
        install_scan(&mut app).send(Err(io::Error::other("injected scan failure"))).unwrap();
        assert!(app.poll_scan());
        drop(install_scan(&mut app));
        assert!(app.poll_scan(), "the scan thread ended without an answer");
        // 중지한 스캔은 끝났다고 알리지 않는다 (이전 문서로 돌아가거나 첫 스캔의 일부 결과)
        let _tx = install_scan(&mut app);
        app.cancel_scan();
        assert!(!app.poll_scan());
        assert!(app.scanning.is_none());
        app.doc = None;
        let tx = install_scan(&mut app);
        app.cancel_scan();
        tx.send(scan_result(partial_result())).unwrap();
        assert!(!app.poll_scan());
        assert!(app.doc.as_ref().is_some_and(|d| d.stats.cancelled));
    }
}
