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
use crate::doc::Doc;
use crate::search_panel;
use crate::treemap::{self, Detail, Highlights, ItemKind, Layout, Morph, Style, ZoomAnim};

pub struct App {
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
    morph_until: Option<Instant>,
}

struct Scanning {
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
    pub fn new(cc: &eframe::CreationContext) -> Self {
        crate::fonts::install(&cc.egui_ctx);
        let drives = list_drives();
        let root_input = drives.first().map(|d| d.root.clone()).unwrap_or_else(|| "C:\\".into());
        let mut app = Self {
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
            morph_until: None,
        };
        // `darak.exe D:\` 처럼 경로를 주면 바로 스캔
        if let Some(arg) = std::env::args().nth(1) {
            app.start_scan(arg);
        }
        app
    }

    fn start_scan(&mut self, root: String) {
        if self.delete_dialog.is_some() {
            return;
        }
        if let Some(s) = &self.scanning {
            s.progress.cancel();
        }
        let root = normalize_root(&root);
        self.root_input = root.clone();
        self.morph.reset();
        self.morph_until = None;
        self.error = None;
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
        self.scanning = Some(Scanning {
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

    fn open_delete_review(&mut self) {
        if self.scanning.is_some() || self.delete_dialog.is_some() {
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
        match s.rx.try_recv() {
            Ok(Ok(res)) => {
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
    }

    fn toast(&mut self, msg: impl Into<String>) {
        self.toast = Some((msg.into(), Instant::now()));
    }

    fn toggle_mark(&mut self, id: NodeId) {
        if let Some(doc) = &mut self.doc
            && let Err(e) = doc.toggle_mark(id) {
                self.toast = Some((e, Instant::now()));
            }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll_scan();
        if self.scanning.is_some() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
        if let Some(doc) = &mut self.doc
            && search_panel::update(doc, self.mode) {
                ctx.request_repaint_after(Duration::from_millis(50));
            }

        egui::Panel::top("toolbar").show(ui, |ui| self.toolbar(ui));
        egui::Panel::bottom("status").show(ui, |ui| self.status_bar(ui));

        let show_results = self.scanning.is_none()
            && self.doc.as_ref().is_some_and(|d| !d.search.query.trim().is_empty());
        if show_results {
            let mut action = search_panel::Action::None;
            egui::Panel::right("results").resizable(true).default_size(440.0).min_size(260.0).show(ui, |ui| {
                if let Some(doc) = &mut self.doc {
                    action = search_panel::panel(ui, doc, self.mode);
                }
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

        let bg = ui.style().visuals.extreme_bg_color;
        egui::CentralPanel::default().frame(egui::Frame::NONE.fill(bg)).show(ui, |ui| {
            if self.scanning.is_some() {
                self.scan_view(ui);
            } else if self.doc.is_some() {
                self.treemap_view(ui);
            } else {
                self.welcome(ui);
            }
        });

        self.delete_dialog_ui(&ctx);
        self.toast_ui(&ctx);
        self.keyboard(&ctx);
    }
}

// ---------------------------------------------------------------------------
// 화면 구성

impl App {
    fn toolbar(&mut self, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        let mut review = false;
        ui.horizontal(|ui| {
            ui.add_enabled_ui(self.delete_dialog.is_none(), |ui| egui::ComboBox::from_id_salt("drive").selected_text("드라이브").width(90.0).show_ui(ui, |ui| {
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

            let edit = ui.add_enabled(self.delete_dialog.is_none(), egui::TextEdit::singleline(&mut self.root_input).desired_width(260.0).hint_text("스캔할 경로"));
            let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));

            if ui.add_enabled(self.delete_dialog.is_none(), egui::Button::new("📁 폴더…")).clicked()
                && let Some(dir) = rfd::FileDialog::new().pick_folder() {
                    self.start_scan(dir.display().to_string());
                }
            if let Some(s) = &self.scanning {
                if ui.button("⏹ 중지").clicked() {
                    s.progress.cancel();
                }
            } else if ui.add_enabled(self.delete_dialog.is_none(), egui::Button::new("🔍 스캔")).clicked() || enter {
                self.start_scan(self.root_input.clone());
            }

            if !self.elevated
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
                    doc.search.edited_at = Some(Instant::now());
                }
                if resp.has_focus() && ui.input(|i| i.key_pressed(Key::Escape)) {
                    doc.search.query.clear();
                }
                resp.on_hover_text(
                    "이름 일부, *.확장자, ext:mp4;mkv, size:>1gb, size:<10mb,\ndm:<7d (최근 7일 수정), dm:>1y, path:단어, file:, folder:, !제외어",
                );

                if !doc.marks.is_empty() {
                    ui.separator();
                    let txt = format!("🗑 표시 {}개 · {}  검토…", doc.marks.len(), human_size(doc.marked_size(self.mode)));
                    if ui.add_enabled(self.scanning.is_none() && self.delete_dialog.is_none(), egui::Button::new(RichText::new(txt).color(Color32::WHITE)).fill(Color32::from_rgb(170, 50, 45))).clicked() {
                        review = true;
                    }
                    if ui.small_button("표시 모두 해제").clicked() {
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
        ui.horizontal(|ui| {
            if let Some(e) = &self.error {
                ui.colored_label(Color32::from_rgb(230, 90, 80), format!("⚠ {e}"));
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

        // 일반 순회는 지금까지의 결과를 주기적으로 미리 보여 준다 (MFT는 몇 초면 끝나므로 진행률만)
        if total_hint == 0 && s.last_preview.elapsed() > Duration::from_millis(1200) {
            s.last_preview = Instant::now();
            let lt = p.live.lock().unwrap();
            if lt.version != s.preview_version {
                s.preview_version = lt.version;
                s.preview = lt.snapshot();
            }
        }

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
            }
        });
        let (resp, painter) = treemap::canvas(ui);
        let area = resp.rect.shrink(2.0);
        s.preview_layout.update(tree, 0, Tree::ROOT, area, mode, detail);
        let (shown, moving) = self.morph.step(&s.preview_layout.items);
        if moving {
            ui.ctx().request_repaint();
        }
        let hit = resp.hover_pos().and_then(|pos| shown.iter().rev().find(|it| it.rect.contains(pos))).copied();
        let hovered = hit.map(|it| it.node);
        let chain = hovered.map(|h| tree.ancestry(h)).unwrap_or_default();
        treemap::paint(
            &painter,
            tree,
            &shown,
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
        self.layout.update(&doc.tree, doc.id, doc.focus, area, mode, detail);
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
        let morphed = match self.morph_until {
            Some(until) if camera.is_none() && Instant::now() < until => {
                let (shown, moving) = self.morph.step(&self.layout.items);
                if moving {
                    ui.ctx().request_repaint();
                    Some(shown)
                } else {
                    None
                }
            }
            _ => None,
        };
        if morphed.is_none() && self.morph_until.is_some() {
            self.morph_until = None;
            self.morph.reset();
        }

        let hit = if camera.is_some() { None } else { resp.hover_pos().and_then(|p| self.layout.hit(p)).copied() };
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
                morphed.as_deref().unwrap_or(&self.layout.items),
                camera,
                &style,
                mode,
                &Highlights { hovered: d.hovered, hover_chain: &chain, selected: d.selected, marked: &marked, filter },
            );
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
                ItemKind::Link => reveal_in_explorer(&doc.tree.path(it.node)),
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
        if !removed.is_empty()
            && let Some(doc) = &mut self.doc
            && doc.apply_removed_for(dlg.doc_id, &removed)
        {
            self.layout.invalidate();
            self.anim = None;
            self.morph.reset();
            self.context_node = None;
            self.drives = list_drives();
        }
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
        if ctx.egui_wants_keyboard_input() || self.delete_dialog.is_some() || self.scanning.is_some() {
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
            if ui.button("📄 열기").clicked() {
                open_item(doc, n);
                ui.close();
            }
        }
        Kind::Dir => {
            if ui.button("📂 폴더 열기").clicked() {
                shell_open(&path);
                ui.close();
            }
        }
        Kind::Link => {}
    }
    if ui.button("🔎 탐색기에서 위치 보기").clicked() {
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
    let path = doc.tree.path(id);
    if !shell_open(&path) {
        reveal_in_explorer(&path);
    }
}

fn reveal_in_explorer(path: &str) {
    let _ = std::process::Command::new("explorer.exe").raw_arg(format!("/select,\"{path}\"")).spawn();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::tests::{doc, fixture, node};
    use darak_core::NO_NODE;

    fn app(doc: Doc) -> App {
        App {
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
            morph_until: None,
        }
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
}
