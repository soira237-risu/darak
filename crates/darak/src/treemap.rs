//! SpaceSniffer식 중첩 treemap: 레이아웃 계산(캐시)과 그리기, 히트 테스트.

use darak_core::category::{Category, categorize};
use darak_core::layout::{self, squarify};
use darak_core::{Kind, NodeId, SizeMode, Tree, flags, human_size};
use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontId, Painter, Pos2, Rect, Stroke, StrokeKind, Vec2,
    text::{LayoutJob, TextFormat, TextWrapping},
};

/// 디렉터리 제목 바 높이
pub const HEADER_H: f32 = 16.0;
/// 디렉터리 안쪽 여백
const PAD: f32 = 2.0;
/// 한 디렉터리에서 그릴 최대 자식 수
const MAX_CHILDREN: usize = 3000;
/// 전체 항목 수 상한 (안전장치)
const MAX_ITEMS: usize = 60_000;

/// 얼마나 잘게 그릴지: 1(SpaceSniffer처럼 큰 박스 몇 단계) ~ 5(아주 잘게)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Detail(u8);

impl Default for Detail {
    fn default() -> Self {
        Detail(3)
    }
}

impl Detail {
    pub const MIN: u8 = 1;
    pub const MAX: u8 = 5;

    pub fn level(self) -> u8 {
        self.0
    }
    pub fn more(self) -> Self {
        Detail((self.0 + 1).min(Self::MAX))
    }
    pub fn less(self) -> Self {
        Detail((self.0 - 1).max(Self::MIN))
    }

    /// (펼칠 최소 폭, 펼칠 최소 높이, 묶음 기준 면적 px², 최대 깊이)
    fn params(self) -> (f32, f32, f32, u16) {
        match self.0 {
            1 => (160.0, 110.0, 8000.0, 3),
            2 => (110.0, 76.0, 3000.0, 4),
            3 => (56.0, HEADER_H + 30.0, 400.0, 8),
            4 => (40.0, HEADER_H + 18.0, 120.0, 12),
            _ => (28.0, HEADER_H + 10.0, 30.0, u16::MAX),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ItemKind {
    /// 자식을 펼친 디렉터리 (제목 바 + 안쪽에 자식)
    OpenDir,
    /// 너무 작아서 펼치지 않은 디렉터리
    ClosedDir,
    File,
    /// 따라가지 않은 junction/symlink
    Link,
    /// 작은 항목 묶음 (node는 소유 디렉터리)
    Group { count: u32, size: u64 },
}

#[derive(Clone, Copy, Debug)]
pub struct Item {
    pub node: NodeId,
    pub rect: Rect,
    pub depth: u16,
    pub kind: ItemKind,
    /// 경로로 만든 키. 스냅샷마다 노드 번호가 바뀌어도 같은 폴더면 같은 키다.
    pub key: u64,
    /// 0..1 (나타나는 중이면 1보다 작다)
    pub fade: f32,
}

#[derive(PartialEq, Clone, Copy)]
struct Key {
    version: u64,
    focus: NodeId,
    size: Vec2,
    origin: Pos2,
    mode: SizeMode,
    tree_id: u64,
    detail: Detail,
}

/// 한 폴더의 줄 구성: 항목 키 순서와 줄마다 개수
struct Plan {
    order: Vec<u64>,
    rows: Vec<usize>,
}

#[derive(Default)]
pub struct Layout {
    key: Option<Key>,
    pub items: Vec<Item>,
    /// true면 폴더마다 줄 구성을 기억해 두고, 크기가 바뀌어도 자리를 유지한다 (스캔 중 미리 보기)
    pub stable: bool,
    plans: std::collections::HashMap<u64, Plan>,
}

impl Layout {
    /// 크기가 바뀌어도 자리를 유지하는 레이아웃 (스캔 중 미리 보기용)
    pub fn stable() -> Self {
        Self { stable: true, ..Default::default() }
    }

    /// 필요할 때만 다시 계산한다.
    pub fn update(&mut self, tree: &Tree, tree_id: u64, focus: NodeId, area: Rect, mode: SizeMode, detail: Detail) {
        let key = Key { version: tree.version, focus, size: area.size(), origin: area.min, mode, tree_id, detail };
        if self.key == Some(key) {
            return;
        }
        self.key = Some(key);
        self.items.clear();
        let (min_w, min_h, min_area, max_depth) = detail.params();
        let mut b = Builder {
            tree,
            mode,
            out: &mut self.items,
            min_w,
            min_h,
            min_area,
            max_depth,
            plans: self.stable.then_some(&mut self.plans),
        };
        b.place(focus, area, 0, path_key(tree, focus));
    }

    pub fn invalidate(&mut self) {
        self.key = None;
    }

    /// 가장 깊은 항목
    pub fn hit(&self, pos: Pos2) -> Option<&Item> {
        self.items.iter().rev().find(|it| it.rect.contains(pos))
    }

    pub fn rect_of(&self, node: NodeId) -> Option<Rect> {
        self.items.iter().find(|it| it.node == node && !matches!(it.kind, ItemKind::Group { .. })).map(|it| it.rect)
    }

    /// `chain`(루트→목표 경로) 중 화면에 있는 가장 깊은 항목의 사각형
    pub fn deepest_rect(&self, chain: &[NodeId]) -> Option<Rect> {
        chain.iter().rev().find_map(|&n| self.rect_of(n))
    }
}

fn hash_name(parent: u64, name: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    parent.hash(&mut h);
    name.hash(&mut h);
    h.finish()
}

/// 루트부터 `id`까지 이름으로 만든 키
pub fn path_key(tree: &Tree, id: NodeId) -> u64 {
    tree.ancestry(id)[1..].iter().fold(0, |k, &n| hash_name(k, tree.name(n)))
}

fn child_key(tree: &Tree, parent_key: u64, id: NodeId) -> u64 {
    if tree.node(id).has(flags::SYNTHETIC) { hash_name(parent_key, "\0files") } else { hash_name(parent_key, tree.name(id)) }
}

struct Builder<'a> {
    tree: &'a Tree,
    mode: SizeMode,
    out: &'a mut Vec<Item>,
    min_w: f32,
    min_h: f32,
    min_area: f32,
    max_depth: u16,
    plans: Option<&'a mut std::collections::HashMap<u64, Plan>>,
}

/// 한 폴더 안에 배치할 항목 (node가 None이면 작은 항목 묶음)
struct Entry {
    key: u64,
    value: f64,
    node: Option<NodeId>,
}

/// 줄 구성을 바꾸지 않고 버틸 최대 가로세로 비율
const MAX_STABLE_ASPECT: f32 = 8.0;

impl Builder<'_> {
    fn place(&mut self, id: NodeId, rect: Rect, depth: u16, key: u64) {
        let tree = self.tree;
        let n = tree.node(id);
        let kind = match n.kind {
            Kind::File => ItemKind::File,
            Kind::Link => ItemKind::Link,
            Kind::Dir => {
                let can_open = (depth == 0
                    || rect.width() >= self.min_w && rect.height() >= self.min_h && depth < self.max_depth)
                    && rect.width() >= 8.0
                    && rect.height() >= HEADER_H + 4.0
                    && n.child_count > 0
                    && self.out.len() < MAX_ITEMS;
                if can_open { ItemKind::OpenDir } else { ItemKind::ClosedDir }
            }
        };
        self.out.push(Item { node: id, rect, depth, kind, key, fade: 1.0 });
        if kind != ItemKind::OpenDir {
            return;
        }

        let inner = Rect::from_min_max(
            Pos2::new(rect.min.x + PAD, rect.min.y + HEADER_H),
            Pos2::new(rect.max.x - PAD, rect.max.y - PAD),
        );
        if inner.width() <= 1.0 || inner.height() <= 1.0 {
            return;
        }
        let total = n.size(self.mode);
        if total == 0 {
            return;
        }

        let mode = self.mode;
        let mut kids: Vec<NodeId> = tree.live_children(id).filter(|&c| tree.node(c).size(mode) > 0).collect();
        if !kids.is_sorted_by(|&a, &b| tree.node(a).size(mode) >= tree.node(b).size(mode)) {
            kids.sort_unstable_by_key(|&c| std::cmp::Reverse(tree.node(c).size(mode)));
        }

        // 화면에서 너무 작은 항목은 하나로 묶는다
        let scale = (inner.width() * inner.height()) as f64 / total as f64;
        let mut shown = 0;
        while shown < kids.len()
            && shown < MAX_CHILDREN
            && (tree.node(kids[shown]).size(mode) as f64 * scale) as f32 >= self.min_area
        {
            shown += 1;
        }
        // 묶음 하나만 남는 경우라면 차라리 그 항목을 그대로 보여 준다
        if shown + 1 == kids.len() {
            shown += 1;
        }
        let rest: u64 = kids[shown..].iter().map(|&c| tree.node(c).size(mode)).sum();
        let mut entries: Vec<Entry> = kids[..shown]
            .iter()
            .map(|&c| Entry { key: child_key(tree, key, c), value: tree.node(c).size(mode) as f64, node: Some(c) })
            .collect();
        if rest > 0 {
            // 묶음은 크기와 상관없이 맨 뒤(오른쪽 아래)에 들어간다
            entries.push(Entry { key: hash_name(key, "\0group"), value: rest as f64, node: None });
        }
        if entries.is_empty() {
            return;
        }

        let area = layout::Rect::new(inner.min.x, inner.min.y, inner.width(), inner.height());
        let rects = self.arrange(key, &mut entries, area);

        for (e, r) in entries.iter().zip(rects) {
            let r = to_egui(r);
            match e.node {
                Some(c) => self.place(c, r, depth + 1, e.key),
                None => self.out.push(Item {
                    node: id,
                    rect: r,
                    depth: depth + 1,
                    kind: ItemKind::Group { count: (kids.len() - shown) as u32, size: rest },
                    key: e.key,
                    fade: 1.0,
                }),
            }
        }
    }

    /// 항목 순서를 정하고 사각형을 배치한다. 안정 모드면 이전 줄 구성을 최대한 유지한다.
    fn arrange(&mut self, dir_key: u64, entries: &mut [Entry], area: layout::Rect) -> Vec<layout::Rect> {
        let mut rects = Vec::with_capacity(entries.len());
        let Some(plans) = self.plans.as_deref_mut() else {
            let values: Vec<f64> = entries.iter().map(|e| e.value).collect();
            squarify(&values, area, &mut rects);
            return rects;
        };

        if let Some(plan) = plans.get(&dir_key) {
            // 기억해 둔 순서대로 놓고, 처음 보는 항목은 마지막 줄로
            let pos = |k: u64| plan.order.iter().position(|&o| o == k).unwrap_or(usize::MAX);
            entries.sort_by_key(|e| pos(e.key));
            let mut rows = Vec::with_capacity(plan.rows.len() + 1);
            let mut start = 0;
            for &len in &plan.rows {
                let row_keys = &plan.order[start.min(plan.order.len())..(start + len).min(plan.order.len())];
                let present = entries.iter().filter(|e| row_keys.contains(&e.key)).count();
                if present > 0 {
                    rows.push(present);
                }
                start += len;
            }
            let placed: usize = rows.iter().sum();
            if placed < entries.len() {
                rows.push(entries.len() - placed);
            }
            let values: Vec<f64> = entries.iter().map(|e| e.value).collect();
            layout::layout_rows(&values, &rows, area, &mut rects);
            if layout::worst_aspect(&rects) <= MAX_STABLE_ASPECT {
                plans.insert(dir_key, Plan { order: entries.iter().map(|e| e.key).collect(), rows });
                return rects;
            }
            rects.clear();
        }

        // 처음이거나 너무 일그러졌으면 크기순으로 새로 배치하고 줄 구성을 기억한다
        entries.sort_by(|a, b| b.value.total_cmp(&a.value));
        let values: Vec<f64> = entries.iter().map(|e| e.value).collect();
        let mut rows = Vec::new();
        layout::squarify_with_rows(&values, area, &mut rects, &mut rows);
        plans.insert(dir_key, Plan { order: entries.iter().map(|e| e.key).collect(), rows });
        rects
    }
}

fn to_egui(r: layout::Rect) -> Rect {
    Rect::from_min_size(Pos2::new(r.x, r.y), Vec2::new(r.w, r.h))
}

// ---------------------------------------------------------------------------
// 줌 애니메이션

/// 화면에 보이는 영역(레이아웃 좌표)을 `area`로 늘려 그리는 카메라
#[derive(Clone, Copy)]
pub struct Camera {
    pub view: Rect,
    pub area: Rect,
}

impl Camera {
    pub fn map(&self, r: Rect) -> Rect {
        let sx = self.area.width() / self.view.width().max(1e-3);
        let sy = self.area.height() / self.view.height().max(1e-3);
        let f = |p: Pos2| Pos2::new(self.area.min.x + (p.x - self.view.min.x) * sx, self.area.min.y + (p.y - self.view.min.y) * sy);
        Rect::from_min_max(f(r.min), f(r.max))
    }
}

pub struct ZoomAnim {
    start: std::time::Instant,
    /// 줌 인: 새 레이아웃이 이 사각형(이전 화면 좌표)에서 화면 전체로 커진다
    /// 줌 아웃: 새 레이아웃에서 이전 focus 자리(이 사각형)가 화면 전체에서 원래 크기로 줄어든다
    from: Rect,
    zoom_in: bool,
    /// 줌 인 동안 밑에 깔아 둘 이전 화면
    pub old_items: Vec<Item>,
}

pub const ANIM_SECS: f32 = 0.22;

impl ZoomAnim {
    pub fn zoom_in(from: Rect, old_items: Vec<Item>) -> Self {
        Self { start: std::time::Instant::now(), from, zoom_in: true, old_items }
    }
    pub fn zoom_out(from: Rect) -> Self {
        Self { start: std::time::Instant::now(), from, zoom_in: false, old_items: Vec::new() }
    }

    /// 진행률 0..1 (ease-out). 끝났으면 None
    fn t(&self) -> Option<f32> {
        let x = self.start.elapsed().as_secs_f32() / ANIM_SECS;
        (x < 1.0).then(|| 1.0 - (1.0 - x).powi(3))
    }

    pub fn is_zoom_in(&self) -> bool {
        self.zoom_in
    }

    /// 지금 프레임의 카메라
    pub fn camera(&self, area: Rect) -> Option<Camera> {
        let t = self.t()?;
        let lerp = |a: Rect, b: Rect| Rect::from_min_max(a.min.lerp(b.min, t), a.max.lerp(b.max, t));
        let view = if self.zoom_in {
            // 새 레이아웃 전체가 from 안에 들어가 보이게 하는 view에서 시작
            let r = self.from;
            let sx = area.width() / r.width().max(1.0);
            let sy = area.height() / r.height().max(1.0);
            let v0 = Rect::from_min_max(
                Pos2::new(area.min.x - (r.min.x - area.min.x) * sx, area.min.y - (r.min.y - area.min.y) * sy),
                Pos2::new(area.max.x + (area.max.x - r.max.x) * sx, area.max.y + (area.max.y - r.max.y) * sy),
            );
            lerp(v0, area)
        } else {
            lerp(self.from, area)
        };
        Some(Camera { view, area })
    }
}

// ---------------------------------------------------------------------------
// 모양 변화 보간 (스캔 중 미리 보기, 스캔 완료 직후)

/// 작은 변화가 미끄러지는 시간, 새 박스가 나타나는 시간
const MOVE_SECS: f32 = 0.3;
const FADE_SECS: f32 = 0.4;
/// 모서리가 이보다 많이 움직이면(px) 미끄러지지 않고 바로 바꾼다 (큰 재배치)
const MAX_SLIDE_PX: f32 = 40.0;

fn ease_in_out(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    if t < 0.5 { 4.0 * t * t * t } else { 1.0 - (-2.0 * t + 2.0).powi(3) / 2.0 }
}

struct Tween {
    from: Rect,
    to: Rect,
    start: std::time::Instant,
    born: std::time::Instant,
}

impl Tween {
    fn rect(&self, now: std::time::Instant) -> (Rect, f32) {
        let t = ease_in_out((now - self.start).as_secs_f32() / MOVE_SECS);
        (Rect::from_min_max(self.from.min.lerp(self.to.min, t), self.from.max.lerp(self.to.max, t)), t)
    }
}

/// 레이아웃이 바뀔 때: 작은 변화는 짧게 미끄러지고, 새 박스는 서서히 나타나고, 큰 재배치는 바로 바꾼다.
/// 화면 전체를 겹치지 않으므로 깜빡이지 않는다.
#[derive(Default)]
pub struct Morph {
    tweens: std::collections::HashMap<u64, Tween>,
}

impl Morph {
    pub fn reset(&mut self) {
        self.tweens.clear();
    }

    /// 이번 프레임에 그릴 항목들. 아직 바뀌는 중이면 true도 함께 돌려준다.
    pub fn step(&mut self, items: &[Item]) -> (Vec<Item>, bool) {
        let now = std::time::Instant::now();
        let first = self.tweens.is_empty();
        let long_ago = now - std::time::Duration::from_secs(5);
        let mut moving = false;
        let mut next = std::collections::HashMap::with_capacity(items.len());
        let out = items
            .iter()
            .map(|it| {
                let target = it.rect;
                let tw = match self.tweens.remove(&it.key) {
                    Some(t) if t.to == target => t,
                    Some(t) => {
                        let (cur, _) = t.rect(now);
                        let dist = (cur.min - target.min).length().max((cur.max - target.max).length());
                        if dist <= MAX_SLIDE_PX {
                            Tween { from: cur, to: target, start: now, born: t.born }
                        } else {
                            Tween { from: target, to: target, start: long_ago, born: t.born }
                        }
                    }
                    // 첫 화면은 바로, 그 뒤 새로 생긴 박스는 제자리에서 서서히
                    None => Tween { from: target, to: target, start: long_ago, born: if first { long_ago } else { now } },
                };
                let (rect, t) = tw.rect(now);
                let fade = ease_in_out((now - tw.born).as_secs_f32() / FADE_SECS);
                moving |= t < 1.0 || fade < 1.0;
                next.insert(it.key, tw);
                Item { rect, fade, ..*it }
            })
            .collect();
        self.tweens = next;
        (out, moving)
    }
}

// ---------------------------------------------------------------------------
// 그리기

pub struct Style {
    pub dark: bool,
}

impl Style {
    fn dir_header(&self, depth: u16) -> Color32 {
        let t = (depth.min(10) as f32) / 10.0;
        if self.dark {
            lerp(Color32::from_rgb(58, 68, 82), Color32::from_rgb(96, 106, 120), t)
        } else {
            lerp(Color32::from_rgb(150, 160, 175), Color32::from_rgb(196, 202, 212), t)
        }
    }

    fn dir_body(&self, depth: u16) -> Color32 {
        let t = (depth.min(10) as f32) / 10.0;
        if self.dark {
            lerp(Color32::from_rgb(32, 35, 40), Color32::from_rgb(50, 54, 60), t)
        } else {
            lerp(Color32::from_rgb(222, 225, 230), Color32::from_rgb(240, 242, 245), t)
        }
    }

    fn closed_dir(&self, depth: u16) -> Color32 {
        let t = (depth.min(10) as f32) / 10.0;
        if self.dark {
            lerp(Color32::from_rgb(84, 94, 108), Color32::from_rgb(118, 126, 138), t)
        } else {
            lerp(Color32::from_rgb(170, 178, 190), Color32::from_rgb(204, 208, 216), t)
        }
    }

    /// 범주 10개를 채도 낮은 5색 + 회색으로 묶는다 (디자인 시스템 `cat.*`).
    /// 밝기를 고르게 맞춰서 타일 글자는 모두 어두운 색이 된다.
    pub fn category(&self, c: Category) -> Color32 {
        let (dark, light) = match c {
            // 미디어 (주황)
            Category::Video | Category::Audio | Category::Image => ((181, 125, 92), (197, 154, 128)),
            // 문서 (노랑)
            Category::Document => ((179, 160, 102), (196, 181, 136)),
            // 개발 (초록)
            Category::Code => ((126, 154, 114), (154, 176, 145)),
            // 프로그램 (파랑)
            Category::Executable | Category::System => ((110, 138, 171), (142, 164, 189)),
            // 덩어리 파일 (보라)
            Category::Archive | Category::Data => ((143, 127, 170), (168, 155, 189)),
            // 기타 (따뜻한 회색)
            Category::Other => ((135, 128, 121), (161, 156, 150)),
        };
        let (r, g, b) = if self.dark { dark } else { light };
        Color32::from_rgb(r, g, b)
    }

    fn border(&self) -> Color32 {
        if self.dark { Color32::from_black_alpha(160) } else { Color32::from_black_alpha(90) }
    }
}

fn lerp(a: Color32, b: Color32, t: f32) -> Color32 {
    let f = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgb(f(a.r(), b.r()), f(a.g(), b.g()), f(a.b(), b.b()))
}

/// 어두운 글자와 밝은 글자 중 바탕과 대비(WCAG)가 더 높은 쪽
fn text_on(bg: Color32) -> Color32 {
    const DARK: Color32 = Color32::from_rgb(20, 20, 24);
    const LIGHT: Color32 = Color32::from_rgb(240, 240, 244);
    let contrast = |a: f32, b: f32| (a.max(b) + 0.05) / (a.min(b) + 0.05);
    let l = luminance(bg);
    if contrast(l, luminance(DARK)) >= contrast(l, luminance(LIGHT)) { DARK } else { LIGHT }
}

fn luminance(c: Color32) -> f32 {
    let ch = |v: u8| {
        let v = v as f32 / 255.0;
        if v <= 0.039_28 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
    };
    0.2126 * ch(c.r()) + 0.7152 * ch(c.g()) + 0.0722 * ch(c.b())
}

/// 한 줄로 자르고 넘치면 `…`. `center`면 x가 가운데 기준
fn label(painter: &Painter, pos: Pos2, text: String, size: f32, color: Color32, max_w: f32, center: bool) {
    let mut job = LayoutJob::single_section(text, TextFormat::simple(FontId::proportional(size), color));
    job.wrap = TextWrapping { max_width: max_w.max(1.0), max_rows: 1, break_anywhere: true, overflow_character: Some('…') };
    let galley = painter.layout_job(job);
    let pos = if center { Pos2::new(pos.x - galley.size().x / 2.0, pos.y) } else { pos };
    painter.galley(pos, galley, color);
}

pub struct Highlights<'a> {
    pub hovered: Option<NodeId>,
    /// 가리킨 항목의 조상들 (제목 바를 밝게)
    pub hover_chain: &'a [NodeId],
    pub selected: Option<NodeId>,
    pub marked: &'a dyn Fn(NodeId) -> bool,
    pub filter: Option<&'a dyn Fn(NodeId) -> bool>,
}

pub fn paint(
    painter: &Painter,
    tree: &Tree,
    items: &[Item],
    camera: Option<Camera>,
    style: &Style,
    mode: SizeMode,
    hl: &Highlights,
) {
    let border = style.border();
    let mark_color = Color32::from_rgb(220, 60, 60);
    let hover_glow = Color32::WHITE;
    let clip = painter.clip_rect();
    for it in items {
        let r = match camera {
            Some(c) => c.map(it.rect),
            None => it.rect,
        };
        if r.width() < 0.5 || r.height() < 0.5 || !r.intersects(clip) {
            continue;
        }
        let is_group = matches!(it.kind, ItemKind::Group { .. });
        let marked = (hl.marked)(it.node) && !is_group;
        let dim = hl.filter.is_some_and(|f| !f(it.node)) && it.kind != ItemKind::OpenDir;
        let hovered = hl.hovered == Some(it.node) && !is_group;
        let in_chain = !is_group && hl.hover_chain.contains(&it.node);
        let n = tree.node(it.node);
        let name = tree.name(it.node);
        let a = it.fade;
        let fa = |c: Color32| if a < 1.0 { c.gamma_multiply(a) } else { c };
        let border = fa(border);
        match it.kind {
            ItemKind::OpenDir => {
                let mut head_bg = style.dir_header(it.depth);
                let mut body_bg = style.dir_body(it.depth);
                if marked {
                    head_bg = lerp(head_bg, mark_color, 0.7);
                    body_bg = lerp(body_bg, mark_color, 0.35);
                }
                if hovered {
                    head_bg = lerp(head_bg, Color32::from_rgb(255, 196, 64), 0.55);
                    body_bg = lerp(body_bg, hover_glow, 0.25);
                } else if in_chain {
                    head_bg = lerp(head_bg, Color32::from_rgb(255, 196, 64), 0.28);
                }
                painter.rect_filled(r, CornerRadius::ZERO, fa(body_bg));
                let head = Rect::from_min_max(r.min, Pos2::new(r.max.x, (r.min.y + HEADER_H).min(r.max.y)));
                painter.rect_filled(head, CornerRadius::ZERO, fa(head_bg));
                if r.width() > 24.0 {
                    let tc = fa(text_on(head_bg));
                    let size_txt = human_size(n.size(mode));
                    let clip = painter.with_clip_rect(head.shrink2(Vec2::new(3.0, 0.0)).intersect(clip));
                    let wide = head.width() > 120.0;
                    let size_w = (size_txt.chars().count() as f32) * 6.6 + 6.0;
                    let name_max = (head.width() - 8.0 - if wide { size_w } else { 0.0 }).max(10.0);
                    let warn = if n.has(flags::DENIED) { " ⚠" } else { "" };
                    label(&clip, Pos2::new(head.min.x + 4.0, head.min.y + 1.5), format!("{name}{warn}"), 11.5, tc, name_max, false);
                    if wide {
                        clip.text(
                            Pos2::new(head.max.x - 4.0, head.center().y),
                            Align2::RIGHT_CENTER,
                            size_txt,
                            FontId::proportional(11.0),
                            tc.gamma_multiply(0.85),
                        );
                    }
                }
                painter.rect_stroke(r, CornerRadius::ZERO, Stroke::new(1.0, border), StrokeKind::Inside);
            }
            ItemKind::ClosedDir | ItemKind::File | ItemKind::Link | ItemKind::Group { .. } => {
                let mut bg = match it.kind {
                    ItemKind::ClosedDir => style.closed_dir(it.depth),
                    ItemKind::File => style.category(categorize(name)),
                    ItemKind::Link => Color32::from_rgb(110, 110, 120),
                    _ => {
                        if style.dark {
                            Color32::from_rgb(70, 70, 76)
                        } else {
                            Color32::from_rgb(190, 190, 196)
                        }
                    }
                };
                if n.has(flags::HARDLINK_DUP) {
                    bg = bg.gamma_multiply(0.6);
                }
                if dim {
                    bg = lerp(bg, if style.dark { Color32::from_gray(25) } else { Color32::from_gray(235) }, 0.75);
                }
                if hovered {
                    bg = lerp(bg, hover_glow, 0.35);
                }
                painter.rect_filled(r, CornerRadius::ZERO, fa(bg));
                // 위쪽이 밝은 약한 입체감
                if r.width() > 6.0 && r.height() > 6.0 {
                    let hi = Rect::from_min_size(r.min, Vec2::new(r.width(), (r.height() * 0.45).min(14.0)));
                    painter.rect_filled(hi, CornerRadius::ZERO, fa(Color32::from_white_alpha(14)));
                }
                painter.rect_stroke(r, CornerRadius::ZERO, Stroke::new(1.0, border), StrokeKind::Inside);
                if is_group {
                    hatch(painter, r, Color32::from_black_alpha(40), 7.0, 1.0);
                }
                // 삭제 표시는 색을 섞지 않고 빨간 빗금과 테두리를 겹친다 (어떤 범주색 위에서도 같게 보이도록)
                if marked {
                    let m = Color32::from_rgba_unmultiplied(mark_color.r(), mark_color.g(), mark_color.b(), 140);
                    hatch(painter, r, fa(m), 10.0, 2.0);
                    painter.rect_stroke(r, CornerRadius::ZERO, Stroke::new(2.0, fa(mark_color)), StrokeKind::Inside);
                }

                if r.width() >= 34.0 && r.height() >= 14.0 {
                    let tc = fa(if dim { text_on(bg).gamma_multiply(0.5) } else { text_on(bg) });
                    let clip = painter.with_clip_rect(r.shrink(2.0).intersect(clip));
                    let (title, sub) = match it.kind {
                        ItemKind::Group { count, size } => (format!("작은 항목 {count}개"), human_size(size)),
                        ItemKind::Link => (format!("{name} →"), "링크".to_string()),
                        _ => (name.to_string(), human_size(n.size(mode))),
                    };
                    // SpaceSniffer처럼 이름과 크기를 가운데에
                    let two_lines = r.height() >= 32.0;
                    let y = if two_lines { r.center().y - 14.0 } else { r.center().y - 8.0 };
                    let max_w = r.width() - 6.0;
                    label(&clip, Pos2::new(r.center().x, y), title, 12.0, tc, max_w, true);
                    if two_lines {
                        label(&clip, Pos2::new(r.center().x, y + 15.0), sub, 11.0, tc.gamma_multiply(0.8), max_w, true);
                    }
                }
            }
        }
    }

    // 선택 테두리는 맨 위에
    if let Some(s) = hl.selected.and_then(|s| items.iter().find(|it| it.node == s && !matches!(it.kind, ItemKind::Group { .. }))) {
        let r = camera.map_or(s.rect, |c| c.map(s.rect));
        painter.rect_stroke(r, CornerRadius::ZERO, Stroke::new(2.5, Color32::from_rgb(255, 184, 28)), StrokeKind::Inside);
        // 안쪽의 어두운 선: 노란·주황 타일 위에서도 앰버 테두리가 보이게
        if r.width() > 10.0 && r.height() > 10.0 {
            painter.rect_stroke(r.shrink(2.5), CornerRadius::ZERO, Stroke::new(1.5, Color32::from_rgb(20, 20, 24)), StrokeKind::Inside);
        }
    }
}

/// 45° 빗금. `step`은 선 사이 가로 간격
fn hatch(painter: &Painter, r: Rect, color: Color32, step: f32, width: f32) {
    if r.width() < 6.0 || r.height() < 6.0 {
        return;
    }
    let p = painter.with_clip_rect(r.intersect(painter.clip_rect()));
    let mut x = r.min.x - r.height();
    while x < r.max.x {
        p.line_segment([Pos2::new(x, r.max.y), Pos2::new(x + r.height(), r.min.y)], Stroke::new(width, color));
        x += step;
    }
}

/// 그리기 영역에서 쓸 반응 영역
pub fn canvas(ui: &mut egui::Ui) -> (egui::Response, Painter) {
    let size = ui.available_size();
    ui.allocate_painter(size, egui::Sense::click())
}
