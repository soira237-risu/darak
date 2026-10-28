//! 디자인 시스템 토큰과 위젯 모양 (캔버스 「색」·「글자」·「컴포넌트」 보드).
//! 다크가 기본이고 라이트는 같은 이름에 값만 다르다. 화면 코드는 여기 도우미로 보드의 모양을 그린다.

use eframe::egui::{
    self, Align, Color32, CornerRadius, FontFamily, FontId, Frame, InnerResponse, Layout, Margin, Rect, Response,
    RichText, Sense, Stroke, StrokeKind, Theme, Ui, epaint::Shadow, pos2, vec2,
};

pub struct Tokens {
    // ── 크롬 ──
    /// 창 배경, 트리맵 뒤
    pub bg_app: Color32,
    /// 툴바 · 사이드 패널 · 상태 바 · 대화상자
    pub surface_panel: Color32,
    /// 호버 행 · 팝업 · 툴팁 · 보조 버튼
    pub surface_raised: Color32,
    /// 입력 필드 안쪽 · 진행 막대 트랙 · 세그먼트 트랙
    pub surface_inset: Color32,
    /// 버튼 호버
    pub surface_hover: Color32,
    /// 버튼 눌림
    pub surface_pressed: Color32,
    /// 비활성 보조 버튼 바탕
    pub surface_disabled: Color32,
    /// 구분선 · 카드 테두리 · 사용량 막대 트랙 · 켜진 세그먼트
    pub line: Color32,
    /// 입력 · 버튼 · 대화상자 테두리
    pub line_strong: Color32,
    /// 호버한 버튼 · 카드 테두리
    pub line_hover: Color32,
    /// 팝업(툴팁 · 메뉴 · 콤보) 테두리와 메뉴 구분선
    pub line_popup: Color32,
    /// 목록 행 사이 구분선
    pub line_row: Color32,
    pub text_primary: Color32,
    /// 보조 설명 · 경로 · 이동 경로 링크
    pub text_secondary: Color32,
    /// 캡션 · 입력 자리 표시 글자 (4.5:1 유지)
    pub text_tertiary: Color32,
    /// 비활성 버튼 글자
    pub text_disabled: Color32,
    /// 이동 경로 구분자 `›` · 비활성 아이콘 (장식, 대비 기준 밖)
    pub text_faint: Color32,
    /// 중립 배지 글자
    pub text_badge: Color32,
    /// 토스트 바탕 (반전)
    pub inverse_surface: Color32,
    /// 토스트 글자 (반전)
    pub inverse_text: Color32,
    // ── 의미 색 ──
    /// 트리맵 선택 테두리 · 현재 위치 밑줄 · 포커스
    pub accent: Color32,
    /// 호버한 폴더 제목 바에 섞는 색
    pub accent_hover: Color32,
    /// 앰버 면 위의 글자 (보드 토큰, 앰버 면을 쓰는 화면이 아직 없다)
    #[allow(dead_code)]
    pub on_accent: Color32,
    /// 강조 버튼 (화면당 하나)
    pub primary_fill: Color32,
    pub primary_hover: Color32,
    pub primary_pressed: Color32,
    pub primary_disabled_fill: Color32,
    pub primary_disabled_text: Color32,
    /// 강조 버튼 글자
    pub on_primary: Color32,
    /// 위험 버튼 (삭제 흐름에만)
    pub danger_fill: Color32,
    pub danger_hover: Color32,
    pub danger_pressed: Color32,
    pub danger_disabled_fill: Color32,
    pub danger_disabled_text: Color32,
    /// 위험 버튼 글자
    pub on_danger: Color32,
    /// 오류 · 되돌릴 수 없음 경고 글자
    pub danger_text: Color32,
    /// 삭제 표시 (빗금 + 테두리)
    pub mark: Color32,
    /// 경고 면 · 막대 (가득 찬 드라이브)
    pub warn: Color32,
    /// 경고 글자
    pub warn_text: Color32,
    pub ok: Color32,
    // ── 상태 배지 ──
    pub tint_danger: Color32,
    pub tint_danger_text: Color32,
    /// 경고 배지 바탕 (글자는 warn_text)
    pub tint_warn: Color32,
    pub tint_ok: Color32,
    pub tint_ok_text: Color32,
    // ── 그림자 ──
    /// 팝업 · 토스트: 0 8 24
    pub shadow_popup: Color32,
    /// 대화상자: 0 16 48
    pub shadow_dialog: Color32,
}

const fn hex(v: u32) -> Color32 {
    Color32::from_rgb((v >> 16) as u8, (v >> 8) as u8, v as u8)
}

const fn black(a: u8) -> Color32 {
    Color32::from_rgba_premultiplied(0, 0, 0, a)
}

pub const DARK: Tokens = Tokens {
    bg_app: hex(0x16181C),
    surface_panel: hex(0x1E2126),
    surface_raised: hex(0x272B31),
    surface_inset: hex(0x121418),
    surface_hover: hex(0x30353C),
    surface_pressed: hex(0x1A1D21),
    surface_disabled: hex(0x22252A),
    line: hex(0x33383F),
    line_strong: hex(0x464C55),
    line_hover: hex(0x5A616B),
    line_popup: hex(0x3A3F46),
    line_row: hex(0x2A2E34),
    text_primary: hex(0xE8EAED),
    text_secondary: hex(0xA4ABB5),
    text_tertiary: hex(0x858D98),
    text_disabled: hex(0x6A717C),
    text_faint: hex(0x50565F),
    text_badge: hex(0xC4C9D0),
    inverse_surface: hex(0xE8EAED),
    inverse_text: hex(0x16181C),
    accent: hex(0xFFB81C),
    accent_hover: hex(0xFFC440),
    on_accent: hex(0x16181C),
    primary_fill: hex(0xE8EAED),
    primary_hover: hex(0xFFFFFF),
    primary_pressed: hex(0xC9CDD3),
    primary_disabled_fill: hex(0x3A3F46),
    primary_disabled_text: hex(0x858D98),
    on_primary: hex(0x16181C),
    danger_fill: hex(0xAA322D),
    danger_hover: hex(0xBE3B35),
    danger_pressed: hex(0x8E2A26),
    danger_disabled_fill: hex(0x4A2B2A),
    danger_disabled_text: hex(0xB08A87),
    on_danger: hex(0xFFFFFF),
    danger_text: hex(0xF28B82),
    mark: hex(0xDC3C3C),
    warn: hex(0xE6A03C),
    warn_text: hex(0xF0C27E),
    ok: hex(0x5CC08A),
    tint_danger: hex(0x4A2422),
    tint_danger_text: hex(0xF6B4AE),
    tint_warn: hex(0x3F3020),
    tint_ok: hex(0x1F3A2C),
    tint_ok_text: hex(0x8EDDB1),
    shadow_popup: black(102),
    shadow_dialog: black(128),
};

pub const LIGHT: Tokens = Tokens {
    bg_app: hex(0xF3F4F6),
    surface_panel: hex(0xFFFFFF),
    surface_raised: hex(0xEBEDF0),
    surface_inset: hex(0xFFFFFF),
    surface_hover: hex(0xE2E5E9),
    surface_pressed: hex(0xD5D9DF),
    surface_disabled: hex(0xF3F4F6),
    line: hex(0xD5D9DF),
    line_strong: hex(0xB7BDC6),
    line_hover: hex(0x9AA1AB),
    line_popup: hex(0xC4C9D0),
    line_row: hex(0xE6E8EC),
    text_primary: hex(0x17191D),
    text_secondary: hex(0x4E5560),
    text_tertiary: hex(0x6A717C),
    text_disabled: hex(0x9AA1AB),
    text_faint: hex(0xB7BDC6),
    text_badge: hex(0x3E444D),
    inverse_surface: hex(0x17191D),
    inverse_text: hex(0xFFFFFF),
    accent: hex(0xC27C00),
    accent_hover: hex(0xFFB81C),
    on_accent: hex(0x16181C),
    primary_fill: hex(0x17191D),
    primary_hover: hex(0x2E333A),
    primary_pressed: hex(0x454B54),
    primary_disabled_fill: hex(0xD5D9DF),
    primary_disabled_text: hex(0x6A717C),
    on_primary: hex(0xFFFFFF),
    danger_fill: hex(0xB3362F),
    danger_hover: hex(0x9E2F29),
    danger_pressed: hex(0x872722),
    danger_disabled_fill: hex(0xF2D6D4),
    danger_disabled_text: hex(0x9E6B67),
    on_danger: hex(0xFFFFFF),
    danger_text: hex(0xB3261E),
    mark: hex(0xDC3C3C),
    warn: hex(0x9A5B00),
    warn_text: hex(0x8A5100),
    ok: hex(0x1E7D4F),
    tint_danger: hex(0xFBE3E1),
    tint_danger_text: hex(0xA1231C),
    tint_warn: hex(0xFBEBD3),
    tint_ok: hex(0xDDF1E5),
    tint_ok_text: hex(0x17663F),
    shadow_popup: black(36),
    shadow_dialog: black(48),
};

// ── 글자 크기 (「글자」 보드 크기 단계). 행간은 egui에 따로 두지 않는다. ──
/// body 13/20 · 버튼
pub const BODY: f32 = 13.0;
/// caption 12/16 · 상태 바 · 보조 설명
pub const CAPTION: f32 = 12.0;
/// heading 16/22/600 · 패널 제목 (보드의 사이드 패널은 아직 계획)
#[allow(dead_code)]
pub const HEADING: f32 = 16.0;
/// title 20/28/600 · 환영 화면 · 대화상자 제목
pub const TITLE: f32 = 20.0;
/// display 28/34/600 (드라이브 용량 카드, 아직 쓰는 화면 없음)
#[allow(dead_code)]
pub const DISPLAY: f32 = 28.0;
/// mono 12/16 · 경로 · 바이트 수 · 검색 문법
pub const MONO: f32 = 12.0;

// ── 컨트롤 높이 · 모서리 ──
pub const H_SMALL: f32 = 24.0;
pub const H_TOOLBAR: f32 = 28.0;
pub const H_DEFAULT: f32 = 32.0;
pub const R_CHIP: u8 = 3;
pub const R_CONTROL: u8 = 4;
pub const R_PANEL: u8 = 6;
pub const R_DIALOG: u8 = 8;

pub fn tokens(dark: bool) -> &'static Tokens {
    if dark { &DARK } else { &LIGHT }
}

/// 현재 테마의 토큰
pub fn of(ui: &Ui) -> &'static Tokens {
    tokens(ui.visuals().dark_mode)
}

/// Ui가 없을 때 (Modal 프레임 등)
pub fn of_ctx(ctx: &egui::Context) -> &'static Tokens {
    tokens(ctx.theme() == Theme::Dark)
}

pub fn install(ctx: &egui::Context) {
    ctx.set_visuals_of(Theme::Dark, visuals(egui::Visuals::dark(), &DARK));
    ctx.set_visuals_of(Theme::Light, visuals(egui::Visuals::light(), &LIGHT));
    // all_styles_mut 안에서 ctx를 다시 읽으면 잠금이 겹치므로 미리 구한다.
    let bold = crate::fonts::semibold_family(ctx);
    ctx.all_styles_mut(|s| {
        use egui::TextStyle;
        s.text_styles.insert(TextStyle::Small, FontId::proportional(CAPTION));
        s.text_styles.insert(TextStyle::Body, FontId::proportional(BODY));
        s.text_styles.insert(TextStyle::Button, FontId::proportional(BODY));
        // ui.heading()은 환영 화면 · 대화상자 제목에 쓰므로 title 20/600
        s.text_styles.insert(TextStyle::Heading, FontId::new(TITLE, bold.clone()));
        s.text_styles.insert(TextStyle::Monospace, FontId::monospace(MONO));
        let sp = &mut s.spacing;
        sp.item_spacing = vec2(8.0, 4.0);
        // 기본 24 (작게 · 최소 누름 영역). 툴바는 28, 대화상자는 32로 `control_height`/`dialog_footer`가 올린다.
        sp.interact_size = vec2(40.0, H_SMALL);
        sp.button_padding = vec2(12.0, 2.0);
        sp.icon_width = 16.0;
        sp.icon_width_inner = 8.0;
        sp.icon_spacing = 8.0;
        // egui는 툴팁 · 메뉴 · 콤보 팝업이 여백 하나를 함께 쓴다 (보드: 툴팁 12×14, 메뉴 4)
        sp.menu_margin = Margin::symmetric(10, 8);
        sp.window_margin = Margin::symmetric(24, 20);
    });
}

fn popup_shadow(t: &Tokens) -> Shadow {
    Shadow { offset: [0, 8], blur: 24, spread: 0, color: t.shadow_popup }
}

fn dialog_shadow(t: &Tokens) -> Shadow {
    Shadow { offset: [0, 16], blur: 48, spread: 0, color: t.shadow_dialog }
}

fn visuals(mut v: egui::Visuals, t: &Tokens) -> egui::Visuals {
    let control = CornerRadius::same(R_CONTROL);
    v.panel_fill = t.surface_panel;
    // window_fill은 egui의 팝업(툴팁 · 메뉴 · 콤보 · 기본 Modal)이 쓴다. 대화상자는 `dialog_frame`으로 바꿔 끼운다.
    v.window_fill = t.surface_raised;
    v.window_stroke = Stroke::new(1.0, t.line_popup);
    v.window_corner_radius = CornerRadius::same(R_DIALOG);
    v.menu_corner_radius = CornerRadius::same(R_PANEL);
    v.popup_shadow = popup_shadow(t);
    v.window_shadow = dialog_shadow(t);
    v.extreme_bg_color = t.surface_inset;
    v.text_edit_bg_color = Some(t.surface_inset);
    v.faint_bg_color = t.surface_raised;
    v.code_bg_color = t.surface_raised;
    v.hyperlink_color = t.text_secondary;
    v.weak_text_color = Some(t.text_secondary);
    v.warn_fg_color = t.warn_text;
    v.error_fg_color = t.danger_text;
    // 세그먼트·텍스트 선택은 차분한 회색 면에 기본 글자. 앰버는 `focus_ring`·`keyboard_focus`가 따로 그린다.
    v.selection.bg_fill = if v.dark_mode { t.line_strong } else { t.line };
    v.selection.stroke = Stroke::new(1.0, t.text_primary);
    v.text_cursor.stroke = Stroke::new(1.0, t.text_primary);

    let w = &mut v.widgets;
    w.noninteractive.bg_fill = t.surface_panel;
    w.noninteractive.weak_bg_fill = t.surface_panel;
    w.noninteractive.bg_stroke = Stroke::new(1.0, t.line);
    w.noninteractive.fg_stroke = Stroke::new(1.0, t.text_primary);
    w.noninteractive.corner_radius = control;

    w.inactive.bg_fill = t.surface_raised;
    w.inactive.weak_bg_fill = t.surface_raised;
    w.inactive.bg_stroke = Stroke::new(1.0, t.line_strong);
    w.inactive.fg_stroke = Stroke::new(1.5, t.text_primary);
    w.inactive.corner_radius = control;

    for s in [&mut w.hovered, &mut w.open] {
        s.bg_fill = t.surface_hover;
        s.weak_bg_fill = t.surface_hover;
        s.bg_stroke = Stroke::new(1.0, t.line_hover);
        s.fg_stroke = Stroke::new(1.5, t.text_primary);
        s.corner_radius = control;
        s.expansion = 0.0;
    }

    w.active.bg_fill = t.surface_pressed;
    w.active.weak_bg_fill = t.surface_pressed;
    w.active.bg_stroke = Stroke::new(1.0, t.line_strong);
    w.active.fg_stroke = Stroke::new(1.5, t.text_primary);
    w.active.corner_radius = control;
    w.active.expansion = 0.0;
    v
}

// ───────────── 글자 ─────────────

/// 굵기 600 (body.strong). 폰트를 설치하지 않은 Context에서는 보통 굵기.
pub fn semibold(ui: &Ui, text: impl Into<String>) -> RichText {
    RichText::new(text).family(crate::fonts::semibold_family(ui.ctx()))
}

/// title 20/600: 환영 화면 · 대화상자 제목
pub fn title(ui: &Ui, text: impl Into<String>) -> RichText {
    semibold(ui, text).size(TITLE)
}

/// heading 16/600: 패널 제목
#[allow(dead_code)]
pub fn heading(ui: &Ui, text: impl Into<String>) -> RichText {
    semibold(ui, text).size(HEADING)
}

/// display 28/600
#[allow(dead_code)]
pub fn display(ui: &Ui, text: impl Into<String>) -> RichText {
    semibold(ui, text).size(DISPLAY)
}

/// caption 12. 보조 설명이면 `.weak()`을 붙인다 (text.secondary)
pub fn caption(text: impl Into<String>) -> RichText {
    RichText::new(text).size(CAPTION)
}

/// mono 12: 경로 · 정확한 바이트 · 검색 문법
pub fn mono(text: impl Into<String>) -> RichText {
    RichText::new(text).family(FontFamily::Monospace).size(MONO)
}

/// 입력 칸 자리 표시 글자 (text.tertiary). `TextEdit::hint_text`에 넘긴다.
pub fn hint(ui: &Ui, text: impl Into<String>) -> RichText {
    RichText::new(text).color(of(ui).text_tertiary)
}

/// 긴 경로를 가운데에서 줄인다: `C:\Users\…\Local\Temp`. 앞(드라이브와 첫 폴더)과 끝(이름)을 남긴다.
pub fn middle_ellipsis(ui: &Ui, text: &str, font: &FontId, max_w: f32) -> String {
    let width = |s: &str| ui.painter().layout_no_wrap(s.to_owned(), font.clone(), Color32::PLACEHOLDER).size().x;
    if width(text) <= max_w {
        return text.to_owned();
    }
    let head_len = text.char_indices().filter(|&(_, c)| c == '\\').nth(1).map_or(0, |(i, _)| i + 1);
    let head = if width(&format!("{}…", &text[..head_len])) <= max_w { &text[..head_len] } else { "" };
    let starts: Vec<usize> = text[head.len()..].char_indices().map(|(i, _)| head.len() + i).collect();
    // 꼬리가 짧아질수록 폭이 줄어드니 들어가는 가장 긴 꼬리를 이분 탐색
    let first = starts.partition_point(|&s| width(&format!("{head}…{}", &text[s..])) > max_w);
    let Some(&start) = starts.get(first) else { return format!("{head}…") };
    // 폴더 경계에서 끊는다 (이름만 남으면 이름 앞을 줄인다)
    let start = text[start..].find('\\').map_or(start, |j| start + j);
    format!("{head}…{}", &text[start..])
}

// ───────────── 포커스 ─────────────

/// 포커스를 가진 입력 필드에 앰버 1px 테두리 (검색 · 경로 칸). egui가 그린 테두리 위에 덮는다.
pub fn focus_ring(ui: &Ui, resp: &Response) {
    if resp.has_focus() {
        ui.painter().rect_stroke(resp.rect, CornerRadius::same(R_CONTROL), Stroke::new(1.0, of(ui).accent), StrokeKind::Inside);
    }
}

/// 마지막 조작이 Tab이면 참, 마우스를 누르면 거짓. 키보드 포커스 링은 이때만 그린다.
pub fn keyboard_nav(ctx: &egui::Context) -> bool {
    let id = egui::Id::new("darak.keyboard_nav");
    let (tab, pointer) = ctx.input(|i| (i.key_pressed(egui::Key::Tab), i.pointer.any_pressed()));
    let prev = ctx.data(|d| d.get_temp::<bool>(id)).unwrap_or(false);
    let on = (prev || tab) && !pointer;
    if on != prev {
        ctx.data_mut(|d| d.insert_temp(id, on));
    }
    on
}

/// 2px 앰버 링, 바탕과 2px 간격 (컴포넌트 보드 「키보드 포커스」)
fn ring(painter: &egui::Painter, rect: Rect, accent: Color32) {
    painter.rect_stroke(rect.expand(2.0), CornerRadius::same(R_CONTROL + 2), Stroke::new(2.0, accent), StrokeKind::Outside);
}

/// 위젯 하나에 키보드 포커스 링
pub fn keyboard_focus(ui: &Ui, resp: &Response) {
    if resp.has_focus() && keyboard_nav(ui.ctx()) {
        ring(ui.painter(), resp.rect, of(ui).accent);
    }
}

/// 프레임 끝에 한 번: 키보드 포커스를 가진 egui 위젯(버튼 · 콤보 · 체크 · 라디오)에 링을 그린다.
/// 글 입력 칸은 `focus_ring`이 맡으므로 건너뛴다.
pub fn paint_keyboard_focus(ctx: &egui::Context) {
    if !keyboard_nav(ctx) {
        return;
    }
    let Some(id) = ctx.memory(|m| m.focused()) else { return };
    if egui::TextEdit::load_state(ctx, id).is_some() {
        return;
    }
    let Some(resp) = ctx.read_response(id) else { return };
    ring(&ctx.layer_painter(resp.layer_id), resp.rect, of_ctx(ctx).accent);
}

// ───────────── 높이 · 배치 ─────────────

/// 이 Ui 안의 버튼 · 콤보 · 행 높이 (H_SMALL / H_TOOLBAR / H_DEFAULT)
pub fn control_height(ui: &mut Ui, h: f32) {
    ui.spacing_mut().interact_size.y = h;
}

/// 대화상자 아래 버튼 줄: 오른쪽 정렬, 간격 8, 높이 32. 먼저 넣은 버튼이 오른쪽 끝에 온다 (실행 → 취소 순으로 넣는다).
pub fn dialog_footer<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    ui.add_space(4.0);
    ui.scope(|ui| {
        control_height(ui, H_DEFAULT);
        ui.spacing_mut().item_spacing.x = 8.0;
        ui.horizontal(|ui| ui.with_layout(Layout::right_to_left(Align::Center), add).inner).inner
    })
    .inner
}

// ───────────── 버튼 ─────────────

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Secondary,
    Primary,
    Danger,
    Ghost,
}

/// (바탕, 테두리, 글자)
fn look(t: &Tokens, kind: Kind, on: bool, hover: bool, down: bool) -> (Color32, Color32, Color32) {
    let none = Color32::TRANSPARENT;
    match kind {
        Kind::Secondary if !on => (t.surface_disabled, t.line, t.text_disabled),
        Kind::Secondary if down => (t.surface_pressed, t.line_strong, t.text_primary),
        Kind::Secondary if hover => (t.surface_hover, t.line_hover, t.text_primary),
        Kind::Secondary => (t.surface_raised, t.line_strong, t.text_primary),
        Kind::Primary if !on => (t.primary_disabled_fill, t.primary_disabled_fill, t.primary_disabled_text),
        Kind::Primary if down => (t.primary_pressed, t.primary_pressed, t.on_primary),
        Kind::Primary if hover => (t.primary_hover, t.primary_hover, t.on_primary),
        Kind::Primary => (t.primary_fill, t.primary_fill, t.on_primary),
        Kind::Danger if !on => (t.danger_disabled_fill, t.danger_disabled_fill, t.danger_disabled_text),
        Kind::Danger if down => (t.danger_pressed, t.danger_pressed, t.on_danger),
        Kind::Danger if hover => (t.danger_hover, t.danger_hover, t.on_danger),
        Kind::Danger => (t.danger_fill, t.danger_fill, t.on_danger),
        Kind::Ghost if !on => (none, none, t.text_faint),
        Kind::Ghost if down => (t.surface_pressed, none, t.text_primary),
        Kind::Ghost if hover => (t.surface_raised, none, t.text_primary),
        Kind::Ghost => (none, none, t.text_secondary),
    }
}

/// 상태마다 토큰 색으로 직접 그리는 버튼. 비활성은 egui의 반투명 대신 비활성 토큰으로 그리고 클릭을 받지 않는다.
fn paint_button(ui: &mut Ui, kind: Kind, enabled: bool, text: &str, font: FontId, size: Option<f32>) -> Response {
    let t = of(ui);
    let galley = ui.painter().layout_no_wrap(text.to_owned(), font, Color32::PLACEHOLDER);
    let pad = if matches!(kind, Kind::Primary | Kind::Danger) { 14.0 } else { 12.0 };
    let h = ui.spacing().interact_size.y;
    let desired = match size {
        Some(s) => vec2(s, s),
        None => vec2(galley.size().x + pad * 2.0, h.max(galley.size().y)),
    };
    let (rect, resp) = ui.allocate_exact_size(desired, if enabled { Sense::click() } else { Sense::hover() });
    let on = enabled && ui.is_enabled();
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, on, text));
    if ui.is_rect_visible(rect) {
        let (fill, stroke, fg) = look(t, kind, on, resp.hovered(), resp.is_pointer_button_down_on());
        let p = ui.painter();
        let r = CornerRadius::same(if size.is_some_and(|s| s < H_TOOLBAR) { R_CHIP } else { R_CONTROL });
        p.rect_filled(rect, r, fill);
        if stroke != Color32::TRANSPARENT {
            p.rect_stroke(rect, r, Stroke::new(1.0, stroke), StrokeKind::Inside);
        }
        p.galley(rect.center() - galley.size() / 2.0, galley, fg);
        if resp.has_focus() && keyboard_nav(ui.ctx()) {
            ring(p, rect, t.accent);
        }
    }
    resp
}

/// 보조 버튼 (기본값): raised · line.strong, 좌우 12. 높이는 현재 `interact_size.y`.
pub fn button(ui: &mut Ui, enabled: bool, text: impl AsRef<str>) -> Response {
    paint_button(ui, Kind::Secondary, enabled, text.as_ref(), FontId::proportional(BODY), None)
}

/// 강조 버튼 (화면당 하나): text.primary 면에 반전 글자 600, 좌우 14
pub fn primary_button(ui: &mut Ui, enabled: bool, text: impl AsRef<str>) -> Response {
    let font = FontId::new(BODY, crate::fonts::semibold_family(ui.ctx()));
    paint_button(ui, Kind::Primary, enabled, text.as_ref(), font, None)
}

/// 위험 버튼 (삭제 흐름에만): danger.fill 면에 흰 글자 600, 좌우 14
pub fn danger_button(ui: &mut Ui, enabled: bool, text: impl AsRef<str>) -> Response {
    let font = FontId::new(BODY, crate::fonts::semibold_family(ui.ctx()));
    paint_button(ui, Kind::Danger, enabled, text.as_ref(), font, None)
}

/// 고스트 아이콘 버튼: 정사각형 `size` (툴바 28, 행 안 24), 테두리 없음, 글리프는 text.secondary.
/// 뜻은 `on_hover_text`로 붙인다.
pub fn icon_button(ui: &mut Ui, enabled: bool, glyph: &str, size: f32) -> Response {
    paint_button(ui, Kind::Ghost, enabled, glyph, FontId::proportional(15.0), Some(size))
}

// ───────────── 세그먼트 (크기 토글) ─────────────

/// 세그먼트 트랙: inset 바탕, 1px line, 모서리 4, 안쪽 2, 간격 2. 안에서 `segment`를 부른다. 전체 높이 28.
pub fn segments<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> InnerResponse<R> {
    let t = of(ui);
    Frame::new()
        .fill(t.surface_inset)
        .stroke(Stroke::new(1.0, t.line))
        .corner_radius(CornerRadius::same(R_CONTROL))
        .inner_margin(Margin::same(2))
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            control_height(ui, H_TOOLBAR - 6.0);
            ui.horizontal(add).inner
        })
}

/// 세그먼트 하나: 높이 22, 좌우 12, 모서리 3, 12px. 켜짐 line 면 · text.primary 600, 꺼짐 투명 · text.secondary.
pub fn segment(ui: &mut Ui, on: bool, text: &str) -> Response {
    let t = of(ui);
    let family = if on { crate::fonts::semibold_family(ui.ctx()) } else { FontFamily::Proportional };
    let galley = ui.painter().layout_no_wrap(text.to_owned(), FontId::new(CAPTION, family), Color32::PLACEHOLDER);
    let size = vec2(galley.size().x + 24.0, H_TOOLBAR - 6.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, ui.is_enabled(), on, text));
    if ui.is_rect_visible(rect) {
        let p = ui.painter();
        if on {
            p.rect_filled(rect, CornerRadius::same(R_CHIP), t.line);
        }
        let fg = if on || resp.hovered() { t.text_primary } else { t.text_secondary };
        p.galley(rect.center() - galley.size() / 2.0, galley, fg);
        if resp.has_focus() && keyboard_nav(ui.ctx()) {
            ring(p, rect, t.accent);
        }
    }
    resp
}

// ───────────── 이동 경로 ─────────────

/// 조상 경로 링크: text.secondary, 호버 text.primary + 밑줄, 안쪽 4×6
pub fn crumb(ui: &mut Ui, text: &str) -> Response {
    let t = of(ui);
    let galley = ui.painter().layout_no_wrap(text.to_owned(), FontId::proportional(BODY), Color32::PLACEHOLDER);
    let gs = galley.size();
    let pad = vec2(6.0, 4.0);
    let (rect, resp) = ui.allocate_exact_size(gs + pad * 2.0, Sense::click());
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Link, ui.is_enabled(), text));
    let hovered = resp.hovered();
    let fg = if hovered { t.text_primary } else { t.text_secondary };
    let pos = rect.min + pad;
    if hovered {
        ui.painter().hline(pos.x..=pos.x + gs.x, pos.y + gs.y - 1.0, Stroke::new(1.0, fg));
    }
    ui.painter().galley(pos, galley, fg);
    keyboard_focus(ui, &resp);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// 현재 위치: 600 text.primary, 아래 2px 앰버 밑줄. 누를 수 없다.
pub fn crumb_current(ui: &mut Ui, text: &str) -> Response {
    let t = of(ui);
    let font = FontId::new(BODY, crate::fonts::semibold_family(ui.ctx()));
    let galley = ui.painter().layout_no_wrap(text.to_owned(), font, Color32::PLACEHOLDER);
    let pad = vec2(6.0, 4.0);
    let (rect, resp) = ui.allocate_exact_size(galley.size() + pad * 2.0, Sense::hover());
    let p = ui.painter();
    p.galley(rect.min + pad, galley, t.text_primary);
    p.rect_filled(Rect::from_min_max(pos2(rect.left(), rect.bottom() - 2.0), rect.max), CornerRadius::ZERO, t.accent);
    resp
}

/// 경로 구분자 `›` (text.faint)
pub fn crumb_separator(ui: &mut Ui) -> Response {
    let c = of(ui).text_faint;
    ui.label(RichText::new("›").color(c))
}

// ───────────── 배지 ─────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tone {
    Neutral,
    // 보드 「배지」의 색조 배지. 지금 화면은 경고를 글자색으로만 쓴다.
    #[allow(dead_code)]
    Danger,
    #[allow(dead_code)]
    Warn,
    #[allow(dead_code)]
    Ok,
}

/// 배지 (높이 20 · 모서리 3 · 좌우 7 · 12px). 색만으로 뜻을 전하지 않도록 글자 앞에 글리프를 붙여 쓴다.
/// 응답은 hover를 감지하므로 툴팁을 붙일 수 있다.
pub fn badge_tone(ui: &mut Ui, text: impl Into<String>, tone: Tone) -> Response {
    let t = of(ui);
    let (bg, fg) = match tone {
        Tone::Neutral => (t.surface_raised, t.text_badge),
        Tone::Danger => (t.tint_danger, t.tint_danger_text),
        Tone::Warn => (t.tint_warn, t.warn_text),
        Tone::Ok => (t.tint_ok, t.tint_ok_text),
    };
    Frame::new()
        .fill(bg)
        .corner_radius(CornerRadius::same(R_CHIP))
        .inner_margin(Margin::symmetric(7, 2))
        .show(ui, |ui| ui.add(egui::Label::new(caption(text).color(fg)).selectable(false)))
        .response
}

/// 중립 회색 배지
pub fn badge(ui: &mut Ui, text: impl Into<String>) -> Response {
    badge_tone(ui, text, Tone::Neutral)
}

// ───────────── 진행 막대 ─────────────

/// 스캔 · 삭제 진행: 높이 4, 모서리 2, 트랙 surface.inset, 채움 text.primary, 글자 없음.
/// `None`이면 움직이는 막대 (진행률을 모를 때).
pub fn scan_progress(ui: &mut Ui, width: f32, frac: Option<f32>) -> Response {
    let t = of(ui);
    let (rect, resp) = ui.allocate_exact_size(vec2(width, 4.0), Sense::hover());
    let r = CornerRadius::same(2);
    let p = ui.painter().with_clip_rect(rect);
    p.rect_filled(rect, r, t.surface_inset);
    let bar = match frac {
        Some(f) => Rect::from_min_size(rect.min, vec2(rect.width() * f.clamp(0.0, 1.0), rect.height())),
        None => {
            let x = (ui.input(|i| i.time) as f32 * 0.7).fract();
            let w = rect.width() * 0.3;
            ui.ctx().request_repaint();
            Rect::from_min_size(pos2(rect.left() - w + (rect.width() + w) * x, rect.top()), vec2(w, rect.height()))
        }
    };
    p.rect_filled(bar, r, t.text_primary);
    resp
}

/// 드라이브 사용량: 높이 6, 모서리 3, 트랙 line, 채움 text.secondary (90% 이상이면 warn)
pub fn usage_bar(ui: &mut Ui, width: f32, frac: f32, warn: bool) -> Response {
    let t = of(ui);
    let (rect, resp) = ui.allocate_exact_size(vec2(width, 6.0), Sense::hover());
    let r = CornerRadius::same(3);
    let p = ui.painter();
    p.rect_filled(rect, r, t.line);
    let fill = Rect::from_min_size(rect.min, vec2(rect.width() * frac.clamp(0.0, 1.0), rect.height()));
    p.rect_filled(fill, r, if warn { t.warn } else { t.text_secondary });
    resp
}

// ───────────── 프레임 ─────────────

/// 대화상자 (Modal::frame에 넘긴다): surface.panel, 1px line.strong, 모서리 8, 안쪽 24×20, 그림자 0 16 48.
pub fn dialog_frame(ctx: &egui::Context) -> Frame {
    let t = of_ctx(ctx);
    Frame::new()
        .fill(t.surface_panel)
        .stroke(Stroke::new(1.0, t.line_strong))
        .corner_radius(CornerRadius::same(R_DIALOG))
        .inner_margin(Margin::symmetric(24, 20))
        .shadow(dialog_shadow(t))
}


/// 알림 배너 (일부 결과 등): surface.panel, 1px line, 모서리 6, 안쪽 12×8
pub fn banner_frame(ui: &Ui) -> Frame {
    let t = of(ui);
    Frame::new()
        .fill(t.surface_panel)
        .stroke(Stroke::new(1.0, t.line))
        .corner_radius(CornerRadius::same(R_PANEL))
        .inner_margin(Margin::symmetric(12, 8))
}

/// 토스트: 반전 면(inverse.surface) · 반전 글자, 모서리 6, 안쪽 14×8, 그림자 0 8 24.
/// 안의 글자와 egui 버튼(되돌리기)은 반전 색으로 그려진다.
pub fn toast<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> InnerResponse<R> {
    let t = of(ui);
    Frame::new()
        .fill(t.inverse_surface)
        .corner_radius(CornerRadius::same(R_PANEL))
        .inner_margin(Margin::symmetric(14, 8))
        .shadow(popup_shadow(t))
        .show(ui, |ui| {
            let v = ui.visuals_mut();
            v.override_text_color = Some(t.inverse_text);
            let w = &mut v.widgets;
            for (s, fill) in [
                (&mut w.inactive, t.inverse_surface),
                (&mut w.hovered, t.primary_hover),
                (&mut w.open, t.primary_hover),
                (&mut w.active, t.primary_pressed),
            ] {
                s.bg_fill = fill;
                s.weak_bg_fill = fill;
                s.bg_stroke = Stroke::new(1.0, t.primary_pressed);
                s.fg_stroke.color = t.inverse_text;
            }
            add(ui)
        })
}

/// 메뉴 구분선 (line.popup, 위아래 4)
pub fn menu_separator(ui: &mut Ui) {
    let c = of(ui).line_popup;
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 9.0), Sense::hover());
    ui.painter().hline(rect.x_range(), rect.center().y, Stroke::new(1.0, c));
}
