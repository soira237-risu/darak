//! 디자인 시스템 토큰 (캔버스 「색」 보드). 다크가 기본이고 라이트는 같은 이름에 값만 다르다.
//! egui 위젯 모양은 「컴포넌트」 보드를 따른다.

use eframe::egui::{self, Color32, CornerRadius, Stroke, Theme};

pub struct Tokens {
    /// 창 배경, 트리맵 뒤
    pub bg_app: Color32,
    /// 툴바 · 사이드 패널 · 상태 바
    pub surface_panel: Color32,
    /// 호버 행 · 팝업 · 툴팁
    pub surface_raised: Color32,
    /// 입력 필드 안쪽
    pub surface_inset: Color32,
    /// 구분선 · 카드 테두리
    pub line: Color32,
    /// 입력 · 버튼 테두리
    pub line_strong: Color32,
    /// 호버한 버튼 · 카드 테두리
    pub line_hover: Color32,
    pub text_primary: Color32,
    pub text_secondary: Color32,
    /// 캡션 (아직 쓰는 곳 없음, 보드와 맞춰 둔다)
    #[allow(dead_code)]
    pub text_tertiary: Color32,
    /// 선택 테두리 · 현재 위치 · 포커스
    pub accent: Color32,
    pub accent_hover: Color32,
    pub danger_fill: Color32,
    pub danger_text: Color32,
    /// 삭제 표시 (빗금 + 테두리)
    pub mark: Color32,
    pub warn: Color32,
    pub ok: Color32,
    /// 버튼 호버 · 눌림 바탕
    surface_hover: Color32,
    surface_pressed: Color32,
}

const fn hex(v: u32) -> Color32 {
    Color32::from_rgb((v >> 16) as u8, (v >> 8) as u8, v as u8)
}

pub const DARK: Tokens = Tokens {
    bg_app: hex(0x16181C),
    surface_panel: hex(0x1E2126),
    surface_raised: hex(0x272B31),
    surface_inset: hex(0x121418),
    line: hex(0x33383F),
    line_strong: hex(0x464C55),
    line_hover: hex(0x5A616B),
    text_primary: hex(0xE8EAED),
    text_secondary: hex(0xA4ABB5),
    text_tertiary: hex(0x858D98),
    accent: hex(0xFFB81C),
    accent_hover: hex(0xFFC440),
    danger_fill: hex(0xAA322D),
    danger_text: hex(0xF28B82),
    mark: hex(0xDC3C3C),
    warn: hex(0xE6A03C),
    ok: hex(0x5CC08A),
    surface_hover: hex(0x30353C),
    surface_pressed: hex(0x1A1D21),
};

pub const LIGHT: Tokens = Tokens {
    bg_app: hex(0xF3F4F6),
    surface_panel: hex(0xFFFFFF),
    surface_raised: hex(0xEBEDF0),
    surface_inset: hex(0xFFFFFF),
    line: hex(0xD5D9DF),
    line_strong: hex(0xB7BDC6),
    line_hover: hex(0x9AA1AB),
    text_primary: hex(0x17191D),
    text_secondary: hex(0x4E5560),
    text_tertiary: hex(0x6A717C),
    accent: hex(0xC27C00),
    accent_hover: hex(0xFFB81C),
    danger_fill: hex(0xB3362F),
    danger_text: hex(0xB3261E),
    mark: hex(0xDC3C3C),
    warn: hex(0x9A5B00),
    ok: hex(0x1E7D4F),
    surface_hover: hex(0xE2E5E9),
    surface_pressed: hex(0xD5D9DF),
};

pub fn tokens(dark: bool) -> &'static Tokens {
    if dark { &DARK } else { &LIGHT }
}

/// 현재 테마의 토큰
pub fn of(ui: &egui::Ui) -> &'static Tokens {
    tokens(ui.visuals().dark_mode)
}

pub fn install(ctx: &egui::Context) {
    ctx.set_visuals_of(Theme::Dark, visuals(egui::Visuals::dark(), &DARK));
    ctx.set_visuals_of(Theme::Light, visuals(egui::Visuals::light(), &LIGHT));
}

/// 포커스를 가진 입력 필드에 앰버 테두리
pub fn focus_ring(ui: &egui::Ui, resp: &egui::Response) {
    if resp.has_focus() {
        ui.painter().rect_stroke(resp.rect, CornerRadius::same(4), Stroke::new(1.0, of(ui).accent), egui::StrokeKind::Inside);
    }
}

/// 차분한 회색 배지 (컴포넌트 보드 「배지」: 높이 20 · 모서리 3). 응답은 hover를 감지하므로 툴팁을 붙일 수 있다.
pub fn badge(ui: &mut egui::Ui, text: impl Into<String>) -> egui::Response {
    let t = of(ui);
    egui::Frame::new()
        .fill(t.surface_raised)
        .corner_radius(3)
        .inner_margin(egui::Margin::symmetric(7, 2))
        .show(ui, |ui| ui.label(egui::RichText::new(text).size(12.0).color(t.text_secondary)))
        .response
}

fn visuals(mut v: egui::Visuals, t: &Tokens) -> egui::Visuals {
    let control = CornerRadius::same(4);
    v.panel_fill = t.surface_panel;
    v.window_fill = t.surface_panel;
    v.window_stroke = Stroke::new(1.0, t.line);
    v.window_corner_radius = CornerRadius::same(8);
    v.menu_corner_radius = CornerRadius::same(6);
    v.extreme_bg_color = t.surface_inset;
    v.text_edit_bg_color = Some(t.surface_inset);
    v.faint_bg_color = t.surface_raised;
    v.code_bg_color = t.surface_raised;
    v.hyperlink_color = t.text_secondary;
    v.weak_text_color = Some(t.text_secondary);
    v.warn_fg_color = t.warn;
    v.error_fg_color = t.danger_text;
    // 세그먼트·텍스트 선택은 차분한 회색 면에 기본 글자. 앰버는 `focus_ring`이 따로 그린다.
    v.selection.bg_fill = if v.dark_mode { t.line_strong } else { t.line };
    v.selection.stroke = Stroke::new(1.0, t.text_primary);
    v.text_cursor.stroke = Stroke::new(2.0, t.text_primary);

    let w = &mut v.widgets;
    w.noninteractive.bg_fill = t.surface_panel;
    w.noninteractive.weak_bg_fill = t.surface_panel;
    w.noninteractive.bg_stroke = Stroke::new(1.0, t.line);
    w.noninteractive.fg_stroke = Stroke::new(1.0, t.text_primary);
    w.noninteractive.corner_radius = control;

    w.inactive.bg_fill = t.surface_raised;
    w.inactive.weak_bg_fill = t.surface_raised;
    w.inactive.bg_stroke = Stroke::new(1.0, t.line_strong);
    w.inactive.fg_stroke = Stroke::new(1.0, t.text_primary);
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
