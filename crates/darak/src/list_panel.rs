//! 트리 목록 패널 (ViewList 보드 안 A · 오른쪽 패널, 가상 스크롤 표)
//!
//! 현재 폴더를 펼칠 수 있는 크기순 목록으로 보여 준다. 선택 · 호버 · 삭제 표시 · 이동은 트리맵과 같은 Doc 상태를 쓴다.
//! 행 목록과 키보드 계산은 tree_list.rs에 있다.

use std::sync::Arc;

use darak_core::category::categorize;
use darak_core::i18n::{plural, tr};
use darak_core::tr;
use darak_core::{Kind, NodeId, SizeMode, flags, format_time, human_size, thousands};
use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontId, Rect, Sense, Stroke, StrokeKind, pos2, vec2,
    text::{LayoutJob, TextFormat, TextWrapping},
};

use crate::doc::Doc;
use crate::theme;
use crate::tree_list::{Scroll, View};
use crate::treemap::{self, Style, lerp};

/// 행 높이 24 (H_SMALL, 검색 결과 패널과 같음)
const ROW_H: f32 = theme::H_SMALL;
/// 행 바탕의 안쪽 여백 8, 열 사이 6. 바깥 여백은 사이드 패널의 기본 안쪽 여백(좌우 8)이 맡는다
const ROW_PAD: f32 = 8.0;
const GAP: f32 = 6.0;
/// 들여쓰기 (깊이마다), 펼침 칸, 칩
const INDENT: f32 = 14.0;
const DISC_W: f32 = 12.0;
const CHIP: f32 = 10.0;
/// 열 폭: 크기 60 · 비율 84 (막대 40 + 6 + 퍼센트) · 항목 수 56 · 수정일 74
const SIZE_W: f32 = 60.0;
const RATIO_W: f32 = 84.0;
/// 기본 사이드 패널의 좌우 안쪽 여백 합 (보드의 열 숨김 기준은 패널 바깥 폭)
const PANEL_MARGIN: f32 = 16.0;
/// 막대를 숨긴 비율 열 (퍼센트 글자만)
const PCT_W: f32 = 44.0;
const BAR_W: f32 = 40.0;
const COUNT_W: f32 = 56.0;
const DATE_W: f32 = 74.0;

/// 이번 프레임에 목록에서 일어난 일 (App이 처리한다)
#[derive(Default)]
pub struct Outcome {
    /// 가리킨 행 (트리맵 강조)
    pub hovered: Option<NodeId>,
    /// 더블클릭한 파일이나 링크 (폴더는 여기서 바로 이동한다)
    pub open: Option<NodeId>,
    /// 메뉴에서 고른 삭제 표시 켜기·끄기
    pub toggle: Option<NodeId>,
}

/// 열 자리 (행 안쪽 x). 패널이 좁으면 수정일(420 미만) → 항목 수(360 미만) → 비율 막대(300 미만, 퍼센트는 남김) 순서로 숨긴다.
struct Cols {
    /// 이름 칸 오른쪽 끝
    name_r: f32,
    size_r: f32,
    /// 비율 열 (왼쪽, 오른쪽)
    ratio: (f32, f32),
    bar: bool,
    count_r: Option<f32>,
    date_r: Option<f32>,
}

fn cols(right: f32, panel_w: f32) -> Cols {
    let mut x = right;
    let mut take = |w: f32| {
        let r = x;
        x -= w + GAP;
        r
    };
    let date_r = if panel_w >= 420.0 { Some(take(DATE_W)) } else { None };
    let count_r = if panel_w >= 360.0 { Some(take(COUNT_W)) } else { None };
    let bar = panel_w >= 300.0;
    let ratio_w = if bar { RATIO_W } else { PCT_W };
    let ratio_r = take(ratio_w);
    let size_r = take(SIZE_W);
    Cols { name_r: x, size_r, ratio: (ratio_r - ratio_w, ratio_r), bar, count_r, date_r }
}

/// 탭 줄 오른쪽 요약: `me · 412 GB · 항목 7`
pub fn summary(doc: &Doc, mode: SizeMode) -> String {
    let (tree, focus) = (&doc.tree, doc.focus);
    let name = tree.name(focus);
    let size = human_size(tree.node(focus).size(mode));
    let count = tree.live_children(focus).count() as u64;
    let (n, items) = (thousands(count), plural(count, "item", "items"));
    tr!("{name} · {size} · 항목 {n}", "{name} · {size} · {n} {items}")
}

/// 한 줄로 자르고 넘치면 `…`. 왼쪽 가운데를 `at`에 맞춰 그리고 그린 폭을 돌려준다
fn one_line(painter: &egui::Painter, at: egui::Pos2, text: String, font: FontId, color: Color32, max_w: f32) -> f32 {
    let mut job = LayoutJob::single_section(text, TextFormat::simple(font, color));
    job.wrap = TextWrapping { max_width: max_w.max(1.0), max_rows: 1, break_anywhere: true, overflow_character: Some('…') };
    let galley = painter.layout_job(job);
    let w = galley.size().x;
    let rect = Align2::LEFT_CENTER.anchor_size(at, galley.size());
    painter.galley(rect.min, galley, color);
    w
}

/// 열 머리글 (높이 24, 아래 1px line). 정렬 기준인 크기만 text.primary + ▼, 나머지는 text.tertiary
fn header(ui: &mut egui::Ui, panel_w: f32) {
    let t = theme::of(ui);
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(vec2(w, ROW_H), Sense::hover());
    let p = ui.painter_at(rect);
    p.hline(rect.x_range(), rect.bottom() - 0.5, Stroke::new(1.0, t.line));
    let c = cols(rect.right() - ROW_PAD, panel_w);
    let font = FontId::new(theme::CAPTION, crate::fonts::semibold_family(ui.ctx()));
    let y = rect.center().y;
    let left = rect.left() + ROW_PAD + 16.0;
    p.text(pos2(left, y), Align2::LEFT_CENTER, tr("이름", "Name"), font.clone(), t.text_tertiary);
    p.text(pos2(c.size_r, y), Align2::RIGHT_CENTER, tr("크기 ▼", "Size ▼"), font.clone(), t.text_primary);
    p.text(pos2(c.ratio.1, y), Align2::RIGHT_CENTER, tr("비율", "Share"), font.clone(), t.text_tertiary);
    if let Some(x) = c.count_r {
        p.text(pos2(x, y), Align2::RIGHT_CENTER, tr("항목 수", "Items"), font.clone(), t.text_tertiary);
    }
    if let Some(x) = c.date_r {
        p.text(pos2(x, y), Align2::RIGHT_CENTER, tr("수정일", "Modified"), font, t.text_tertiary);
    }
}

/// 목록 패널 본문: 일부 결과 안내 · 열 머리글 · 행. 탭 줄은 App이 그린다.
pub fn show(ui: &mut egui::Ui, doc: &mut Doc, view: &mut View, mode: SizeMode, elevated: bool) -> Outcome {
    let mut out = Outcome::default();
    let t = theme::of(ui);
    let style = Style { dark: ui.visuals().dark_mode };
    let panel_w = ui.available_width() + PANEL_MARGIN;
    view.sync(doc);
    let scroll = view.take_scroll();

    // 중지한 스캔: 덜 읽은 폴더마다 ⚠, 삭제 표시는 할 수 없다 (tint.warn 띠, 12px warn.text)
    if doc.stats.cancelled {
        egui::Frame::new().fill(t.tint_warn).inner_margin(egui::Margin::symmetric(12, 6)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(theme::caption(tr(
                "⚠ 중지한 스캔의 일부 결과 · 삭제 표시를 할 수 없습니다",
                "⚠ Partial results from a stopped scan · Can't mark for deletion",
            )).color(t.warn_text));
        });
    }
    header(ui, panel_w);

    // 메뉴가 열린 동안 행이 밀려도 처음 누른 항목을 가리키게 한다 (메뉴 id는 행 자리에 묶인다)
    let menu = view.menu;
    let map_hover = view.map_hover;
    let mut menu_set = None;
    let rows = view.rows(doc, mode);
    if rows.is_empty() {
        // 머리글은 남기고 남은 자리 가운데에 caption text.tertiary 한 줄
        let (rect, _) = ui.allocate_exact_size(ui.available_size(), Sense::hover());
        let g = ui.painter().layout_no_wrap(tr("이 폴더는 비어 있습니다", "This folder is empty").into(), FontId::proportional(theme::CAPTION), t.text_tertiary);
        ui.painter().galley(rect.center() - g.size() / 2.0, g, t.text_tertiary);
        return out;
    }

    // 행 사이 간격 없이 24씩 (show_rows가 행 높이에 item_spacing.y를 더한다)
    ui.spacing_mut().item_spacing.y = 0.0;
    // 메뉴가 경로 복사 · 이동을 하는 동안에도 그리던 트리를 계속 읽는다
    let tree = Arc::clone(&doc.tree);
    let search_dim = doc.search_active();
    let (mut select, mut expand, mut activate) = (None, None, None);
    egui::ScrollArea::vertical().id_salt("tree-list").auto_shrink([false, false]).show_rows(ui, ROW_H, rows.len(), |ui, range| {
        let w = ui.available_width();
        // 고른 행으로 한 번 스크롤한다. 보이지 않는 행도 자리를 계산할 수 있다 (행 높이가 모두 같다)
        let target = match scroll {
            Some(Scroll::Top) => Some(0),
            Some(Scroll::To(id)) => rows.iter().position(|r| r.id == id),
            None => None,
        };
        if let Some(i) = target {
            let top = ui.max_rect().top() + (i as f32 - range.start as f32) * ROW_H;
            ui.scroll_to_rect(Rect::from_min_size(pos2(ui.max_rect().left(), top), vec2(w, ROW_H)), None);
        }
        for row in &rows[range] {
            let id = row.id;
            let n = tree.node(id);
            let (rect, resp) = ui.allocate_exact_size(vec2(w, ROW_H), Sense::click());
            let p = ui.painter_at(rect);
            let bg = rect;
            let state = doc.mark_state(id);
            let selected = doc.selected == Some(id);
            let hovered = resp.hovered() || map_hover == Some(id);
            // 검색 '트리맵에 강조'가 켜져 있으면 일치하지 않는 행은 바탕까지 모두 50%
            let dim = search_dim && !doc.search_lit(id);
            let fa = |c: Color32| if dim { c.gamma_multiply(0.5) } else { c };

            // 바탕: 선택 accent 8% · 삭제 표시 mark 12% · 호버 surface.hover, 모서리 3
            let radius = CornerRadius::same(theme::R_CHIP);
            if selected {
                p.rect_filled(bg, radius, fa(lerp(t.surface_panel, t.accent, 0.08)));
            } else if state == 1 {
                p.rect_filled(bg, radius, fa(lerp(t.surface_panel, t.mark, 0.12)));
            } else if hovered {
                p.rect_filled(bg, radius, fa(t.surface_hover));
            }
            if state == 1 {
                p.rect_filled(Rect::from_min_size(bg.min, vec2(2.0, bg.height())), CornerRadius::ZERO, fa(t.mark));
            }
            // 선택 테두리는 삭제 표시 위에
            if selected {
                p.rect_stroke(bg, radius, Stroke::new(1.0, fa(t.accent)), StrokeKind::Inside);
            }

            let c = cols(bg.right() - ROW_PAD, panel_w);
            let y = rect.center().y;
            let blank = n.has(flags::DENIED) || n.kind == Kind::Link;
            let hard = n.has(flags::HARDLINK_DUP);
            let struck = state != 0;

            // 이름 칸: 들여쓰기 → 펼침 ▸ ▾ → 칩 → 이름 → ⚠ (사이 4. 깊이 0의 빈 들여쓰기 뒤에도 4)
            let disc_l = bg.left() + ROW_PAD + row.depth as f32 * INDENT + 4.0;
            if row.expandable {
                let glyph = if row.open { "▾" } else { "▸" };
                let g = p.layout_no_wrap(glyph.into(), FontId::proportional(11.0), fa(t.text_tertiary));
                p.galley(pos2(disc_l + DISC_W / 2.0, y) - g.size() / 2.0, g, fa(t.text_tertiary));
            }
            let chip_l = disc_l + DISC_W + 4.0;
            let chip_fill = match n.kind {
                Kind::Dir => style.closed_dir(0),
                Kind::Link => style.link(),
                Kind::File => style.category(categorize(tree.name(id))),
            };
            let chip = Rect::from_min_size(pos2(chip_l, y - CHIP / 2.0), vec2(CHIP, CHIP));
            treemap::chip(&p, chip, chip_fill, struck, t.mark, if dim { 0.5 } else { 1.0 });
            let name_l = chip_l + CHIP + 2.0 + 4.0;
            let warn = n.incomplete().then(|| p.layout_no_wrap("⚠".into(), FontId::proportional(theme::BODY), fa(t.warn_text)));
            let warn_w = warn.as_ref().map_or(0.0, |g| g.size().x + 4.0);
            let name_c = if state == 2 || hard {
                t.text_tertiary
            } else if state == 1 {
                t.text_secondary
            } else {
                t.text_primary
            };
            let name_max = c.name_r - name_l - warn_w;
            if name_max > 8.0 {
                let gw = one_line(&p, pos2(name_l, y), tree.name(id).to_owned(), FontId::proportional(theme::BODY), fa(name_c), name_max);
                // 취소선은 mark 색 (글자 위에 빗금을 깔면 읽기 어렵다)
                if struck {
                    p.hline(name_l..=name_l + gw, y, Stroke::new(1.0, fa(t.mark)));
                }
                if let Some(g) = warn {
                    let at = Align2::LEFT_CENTER.anchor_size(pos2(name_l + gw + 4.0, y), g.size());
                    p.galley(at.min, g, fa(t.warn_text));
                }
            }

            // 크기: mono 12 오른쪽. 접근 거부 —, 하드링크 0 B, 링크 '링크'
            let size = if n.has(flags::DENIED) {
                "—".to_owned()
            } else if n.kind == Kind::Link {
                tr("링크", "Link").to_owned()
            } else {
                human_size(n.size(mode))
            };
            let size_c = if blank || hard { t.text_tertiary } else { t.text_primary };
            p.text(pos2(c.size_r, y), Align2::RIGHT_CENTER, size, FontId::monospace(theme::MONO), fa(size_c));

            // 비율: 부모 폴더 대비. 막대 40×6 (트랙 surface.inset, 라이트는 line.row · 채움 line.hover, 삭제 표시면 mark)
            let parent = tree.parent(id).map_or(0, |pa| tree.node(pa).size(mode));
            let frac = if blank || parent == 0 { 0.0 } else { n.size(mode) as f64 / parent as f64 };
            if c.bar && !blank {
                let track = Rect::from_min_size(pos2(c.ratio.0, y - 3.0), vec2(BAR_W, 6.0));
                let r2 = CornerRadius::same(2);
                p.rect_filled(track, r2, fa(if style.dark { t.surface_inset } else { t.line_row }));
                let fill = Rect::from_min_size(track.min, vec2(BAR_W * frac.clamp(0.0, 1.0) as f32, 6.0));
                p.rect_filled(fill, r2, fa(if struck { t.mark } else { t.line_hover }));
            }
            let pct = if blank { "—".to_owned() } else { format!("{:.1}%", frac * 100.0) };
            let cap = FontId::proportional(theme::CAPTION);
            p.text(pos2(c.ratio.1, y), Align2::RIGHT_CENTER, pct, cap.clone(), fa(t.text_secondary));
            // 항목 수: 하위 파일 수 (Node.files), 파일 행은 —
            if let Some(x) = c.count_r {
                let count = if n.kind == Kind::File || blank { "—".to_owned() } else { thousands(n.files as u64) };
                p.text(pos2(x, y), Align2::RIGHT_CENTER, count, cap.clone(), fa(t.text_secondary));
            }
            // 수정일: YYYY-MM-DD (시각은 툴팁)
            if let Some(x) = c.date_r {
                let date = if n.has(flags::DENIED) || n.mtime == 0 {
                    "—".to_owned()
                } else {
                    format_time(n.mtime).chars().take(10).collect()
                };
                p.text(pos2(x, y), Align2::RIGHT_CENTER, date, cap, fa(t.text_secondary));
            }

            // 클릭 = 선택, ▸ 자리 클릭 = 펼치기/접기만, 더블클릭 = 폴더 이동 · 파일 열기, 오른쪽 클릭 = 선택 + 메뉴
            let on_disc = row.expandable && resp.hover_pos().is_some_and(|q| (disc_l - 4.0..disc_l + DISC_W + 4.0).contains(&q.x));
            if resp.clicked() {
                if on_disc {
                    expand = Some(id);
                } else {
                    select = Some(id);
                }
            }
            if resp.double_clicked() && !on_disc {
                activate = Some(id);
            }
            if resp.secondary_clicked() {
                select = Some(id);
                menu_set = Some(id);
            }
            if resp.hovered() {
                out.hovered = Some(id);
            }
            resp.context_menu(|ui| {
                let target = if resp.secondary_clicked() { Some(id) } else { menu };
                let Some(n) = target else {
                    ui.close();
                    return;
                };
                if crate::app::context_menu(ui, doc, n, elevated) {
                    out.toggle = Some(n);
                }
            });
            resp.on_hover_ui(|ui| crate::app::node_tooltip(ui, doc, id, mode));
        }
    });

    if let Some(m) = menu_set {
        view.menu = Some(m);
    }
    if let Some(id) = expand {
        doc.expanded.toggle(id);
        // 고른 항목을 접어 숨겼으면 접은 폴더를 고른다 (↑ ↓가 그 자리에서 이어진다)
        if !doc.expanded.contains(id) && doc.selected.is_some_and(|s| s != id && tree.is_ancestor(id, s)) {
            doc.selected = Some(id);
        }
    }
    if let Some(id) = select {
        doc.selected = Some(id);
    }
    if let Some(id) = activate {
        if tree.node(id).kind == Kind::Dir {
            doc.navigate(id);
        } else {
            out.open = Some(id);
        }
    }
    out
}
