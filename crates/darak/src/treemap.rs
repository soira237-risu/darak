//! SpaceSniffer식 중첩 treemap: 레이아웃 계산(캐시)과 그리기, 히트 테스트.

use darak_core::category::{Category, categorize};
use darak_core::i18n::{plural, tr};
use darak_core::layout::{self, squarify};
use darak_core::{Kind, NodeId, SizeMode, Tree, flags, human_size};
use darak_core::tr;
use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontFamily, FontId, Painter, Pos2, Rect, Stroke, StrokeKind, Vec2,
    text::{LayoutJob, TextFormat, TextWrapping},
};
use std::collections::HashMap;
use std::time::Instant;

use crate::theme;

/// 디렉터리 제목 바 높이
pub const HEADER_H: f32 = 17.0;
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
    ranks: HashMap<u64, usize>,
    row_by_key: HashMap<u64, usize>,
    duplicates: bool,
}

impl Plan {
    fn new(order: Vec<u64>, rows: Vec<usize>) -> Self {
        let mut ranks = HashMap::with_capacity(order.len());
        let mut duplicates = false;
        for (rank, &key) in order.iter().enumerate() {
            match ranks.entry(key) {
                std::collections::hash_map::Entry::Vacant(entry) => { entry.insert(rank); }
                std::collections::hash_map::Entry::Occupied(_) => duplicates = true,
            }
        }
        let mut row_by_key = HashMap::with_capacity(order.len());
        let mut start = 0;
        for (row, &len) in rows.iter().enumerate() {
            for &key in &order[start.min(order.len())..(start + len).min(order.len())] {
                row_by_key.entry(key).or_insert(row);
            }
            start += len;
        }
        Self { order, rows, ranks, row_by_key, duplicates }
    }
}

#[derive(Default)]
pub struct Layout {
    key: Option<Key>,
    generation: u64,
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
        self.generation = self.generation.wrapping_add(1);
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

    /// 실제 레이아웃 재계산마다 바뀌는 번호.
    pub fn generation(&self) -> u64 {
        self.generation
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
                    && rect.height() >= HEADER_H + PAD + 4.0
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
            Pos2::new(rect.min.x + PAD, rect.min.y + HEADER_H + PAD),
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
            entries.sort_by_key(|e| plan.ranks.get(&e.key).copied().unwrap_or(usize::MAX));
            let mut rows = if plan.duplicates {
                // 같은 키가 여러 줄에 있으면 이전 membership 집계를 그대로 쓴다.
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
                rows
            } else {
                let mut rows = vec![0; plan.rows.len()];
                for entry in entries.iter() {
                    if let Some(&row) = plan.row_by_key.get(&entry.key) {
                        rows[row] += 1;
                    }
                }
                rows.retain(|&count| count > 0);
                rows
            };
            let placed: usize = rows.iter().sum();
            if placed < entries.len() {
                rows.push(entries.len() - placed);
            }
            let values: Vec<f64> = entries.iter().map(|e| e.value).collect();
            layout::layout_rows(&values, &rows, area, &mut rects);
            if layout::worst_aspect(&rects) <= MAX_STABLE_ASPECT {
                plans.insert(dir_key, Plan::new(entries.iter().map(|e| e.key).collect(), rows));
                return rects;
            }
            rects.clear();
        }

        // 처음이거나 너무 일그러졌으면 크기순으로 새로 배치하고 줄 구성을 기억한다
        entries.sort_by(|a, b| b.value.total_cmp(&a.value));
        let values: Vec<f64> = entries.iter().map(|e| e.value).collect();
        let mut rows = Vec::new();
        layout::squarify_with_rows(&values, area, &mut rects, &mut rows);
        plans.insert(dir_key, Plan::new(entries.iter().map(|e| e.key).collect(), rows));
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

pub const ANIM_SECS: f32 = 0.15;

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

#[derive(Clone, Copy)]
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

/// 스캔 번호와 문서 번호가 같아도 서로 다른 레이아웃 소스다.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayoutSource {
    Preview(u64),
    Document(u64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MorphKey {
    pub layout_id: LayoutSource,
    pub generation: u64,
}

/// 작은 변화는 짧게 미끄러지고, 새 박스는 서서히 나타나고, 큰 재배치는 바로 바꾼다.
#[derive(Default)]
pub struct Morph {
    tweens: HashMap<u64, Tween>,
    scratch: HashMap<u64, Tween>,
    settled: Option<MorphKey>,
}

impl Morph {
    pub fn reset(&mut self) {
        self.tweens.clear();
        self.scratch.clear();
        self.invalidate();
    }

    /// 문서/미리 보기 전환 시 기존 보간은 유지하고 완료 캐시만 지운다.
    pub fn invalidate(&mut self) {
        self.settled = None;
    }

    pub fn is_settled(&self, key: MorphKey) -> bool {
        self.settled == Some(key)
    }

    /// `out`을 현재 항목/metadata로 채운다. 아직 보간 중이면 true.
    /// 완료된 같은 키는 Layout.items를 직접 사용하면 복사도 생략할 수 있다.
    pub fn step_into(&mut self, items: &[Item], key: MorphKey, out: &mut Vec<Item>) -> bool {
        self.step_into_at(items, key, out, Instant::now())
    }

    fn step_into_at(&mut self, items: &[Item], key: MorphKey, out: &mut Vec<Item>, now: Instant) -> bool {
        out.clear();
        if self.is_settled(key) {
            out.extend_from_slice(items);
            return false;
        }
        self.invalidate();
        let first = self.tweens.is_empty();
        let long_ago = now - std::time::Duration::from_secs(5);
        let mut moving = false;
        let mut duplicates = false;
        self.scratch.clear();
        self.scratch.reserve(items.len());
        out.reserve(items.len());
        for it in items {
            let target = it.rect;
            let next = self.scratch.entry(it.key);
            let previous = match &next {
                std::collections::hash_map::Entry::Vacant(_) => self.tweens.get(&it.key).copied(),
                // The old remove-based loop consumed the previous tween only
                // once per key; preserve that behavior for hash collisions.
                std::collections::hash_map::Entry::Occupied(_) => { duplicates = true; None }
            };
            let tw = match previous {
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
            match next {
                std::collections::hash_map::Entry::Vacant(entry) => { entry.insert(tw); }
                std::collections::hash_map::Entry::Occupied(mut entry) => { entry.insert(tw); }
            }
            out.push(Item { rect, fade, ..*it });
        }
        std::mem::swap(&mut self.tweens, &mut self.scratch);
        self.scratch.clear();
        if !moving && !duplicates {
            self.settled = Some(key);
        }
        moving
    }
}

// ---------------------------------------------------------------------------
// 그리기

pub struct Style {
    pub dark: bool,
}

const fn hex(v: u32) -> Color32 {
    Color32::from_rgb((v >> 16) as u8, (v >> 8) as u8, v as u8)
}

/// 타일 데이터 색 (「색」·「트리맵 타일」 보드의 dir.* · cat.* · tile.*). 크롬 토큰이 아니라서 여기 둔다.
struct Palette {
    /// 깊이 0 → 10 사이를 보간하는 램프 (양 끝)
    dir_header: [Color32; 2],
    dir_body: [Color32; 2],
    dir_closed: [Color32; 2],
    /// cat.media, doc, dev, program, bulk, other
    cat: [Color32; 6],
    /// tile.border: 다크 검정 63%, 라이트 검정 35%
    border: Color32,
    /// tile.link (두 테마 같은 값)
    link: Color32,
    /// tile.group
    group: Color32,
}

const DARK: Palette = Palette {
    dir_header: [hex(0x3A4452), hex(0x606A78)],
    dir_body: [hex(0x202328), hex(0x32363C)],
    dir_closed: [hex(0x545E6C), hex(0x767E8A)],
    cat: [hex(0xB57D5C), hex(0xB3A066), hex(0x7E9A72), hex(0x6E8AAB), hex(0x8F7FAA), hex(0x878079)],
    border: Color32::from_rgba_premultiplied(0, 0, 0, 160),
    link: hex(0x6E6E78),
    group: hex(0x46464C),
};

const LIGHT: Palette = Palette {
    dir_header: [hex(0x96A0AF), hex(0xC4CAD4)],
    dir_body: [hex(0xDEE1E6), hex(0xF0F2F5)],
    dir_closed: [hex(0xAAB2BE), hex(0xCCD0D8)],
    cat: [hex(0xC59A80), hex(0xC4B588), hex(0x9AB091), hex(0x8EA4BD), hex(0xA89BBD), hex(0xA19C96)],
    border: Color32::from_rgba_premultiplied(0, 0, 0, 90),
    link: hex(0x6E6E78),
    group: hex(0xBEBEC4),
};

/// tile.text.dark / tile.text.light: 바탕과 대비가 높은 쪽을 고른다. 선택 테두리 안쪽 선도 dark.
const TILE_INK: Color32 = hex(0x141418);
const TILE_INK_LIGHT: Color32 = hex(0xF0F0F4);
/// tile.highlight: 흰색 5.5% (두 테마)
const TILE_HIGHLIGHT: Color32 = Color32::from_rgba_premultiplied(14, 14, 14, 14);
/// tile.group.hatch: 검정 16% (α40), 1px, 7px 간격
const GROUP_HATCH: Color32 = Color32::from_rgba_premultiplied(0, 0, 0, 40);
/// 호버 때 섞는 흰색 (파일 35%, 폴더 몸통 25%)
const HOVER_GLOW: Color32 = Color32::WHITE;
/// 삭제 빗금: mark 55%, 2px, 8px 간격
const MARK_HATCH_ALPHA: u8 = 140;
/// 목록 칩의 안쪽 테두리: 검정 35% (두 테마)
const CHIP_EDGE: Color32 = Color32::from_rgba_premultiplied(0, 0, 0, 89);

/// 글자 크기: 폴더 제목 11.5/600, 타일 이름 12/600, 크기 11/400
const DIR_TITLE: f32 = 11.5;
const TILE_NAME: f32 = 12.0;
const TILE_SIZE: f32 = 11.0;

impl Style {
    fn palette(&self) -> &'static Palette {
        if self.dark { &DARK } else { &LIGHT }
    }

    fn ramp(ends: [Color32; 2], depth: u16) -> Color32 {
        lerp(ends[0], ends[1], (depth.min(10) as f32) / 10.0)
    }

    fn dir_header(&self, depth: u16) -> Color32 {
        Self::ramp(self.palette().dir_header, depth)
    }

    fn dir_body(&self, depth: u16) -> Color32 {
        Self::ramp(self.palette().dir_body, depth)
    }

    /// 펼치지 않은 폴더 (목록의 폴더 칩은 깊이 0 값)
    pub fn closed_dir(&self, depth: u16) -> Color32 {
        Self::ramp(self.palette().dir_closed, depth)
    }

    /// tile.link (목록의 링크 칩)
    pub fn link(&self) -> Color32 {
        self.palette().link
    }

    /// 범주 10개를 채도 낮은 5색 + 회색으로 묶는다 (디자인 시스템 `cat.*`).
    /// 밝기를 고르게 맞춰서 타일 글자는 모두 어두운 색이 된다.
    pub fn category(&self, c: Category) -> Color32 {
        let i = match c {
            Category::Video | Category::Audio | Category::Image => 0,
            Category::Document => 1,
            Category::Code => 2,
            Category::Executable | Category::System => 3,
            Category::Archive | Category::Data => 4,
            Category::Other => 5,
        };
        self.palette().cat[i]
    }
}

pub(crate) fn lerp(a: Color32, b: Color32, t: f32) -> Color32 {
    let f = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgb(f(a.r(), b.r()), f(a.g(), b.g()), f(a.b(), b.b()))
}

/// 어두운 글자와 밝은 글자 중 바탕과 대비(WCAG)가 더 높은 쪽
fn text_on(bg: Color32) -> Color32 {
    let contrast = |a: f32, b: f32| (a.max(b) + 0.05) / (a.min(b) + 0.05);
    let l = luminance(bg);
    if contrast(l, luminance(TILE_INK)) >= contrast(l, luminance(TILE_INK_LIGHT)) { TILE_INK } else { TILE_INK_LIGHT }
}

fn luminance(c: Color32) -> f32 {
    let ch = |v: u8| {
        let v = v as f32 / 255.0;
        if v <= 0.039_28 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
    };
    0.2126 * ch(c.r()) + 0.7152 * ch(c.g()) + 0.0722 * ch(c.b())
}

/// 한 줄로 자르고 넘치면 `…`. `align`은 `pos`에 대한 글자 상자의 기준점
fn label(painter: &Painter, pos: Pos2, text: String, font: FontId, color: Color32, max_w: f32, align: Align2) {
    let mut job = LayoutJob::single_section(text, TextFormat::simple(font, color));
    job.wrap = TextWrapping { max_width: max_w.max(1.0), max_rows: 1, break_anywhere: true, overflow_character: Some('…') };
    let galley = painter.layout_job(job);
    let rect = align.anchor_size(pos, galley.size());
    painter.galley(rect.min, galley, color);
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
    let pal = style.palette();
    let border = pal.border;
    let t = theme::tokens(style.dark);
    let mark_color = t.mark;
    let bold = crate::fonts::semibold_family(painter.ctx());
    let font = |size: f32, bold_name: bool| FontId::new(size, if bold_name { bold.clone() } else { FontFamily::Proportional });
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
                    head_bg = lerp(head_bg, t.accent_hover, 0.55);
                    body_bg = lerp(body_bg, HOVER_GLOW, 0.25);
                } else if in_chain {
                    head_bg = lerp(head_bg, t.accent_hover, 0.28);
                }
                painter.rect_filled(r, CornerRadius::ZERO, fa(body_bg));
                let head = Rect::from_min_max(r.min, Pos2::new(r.max.x, (r.min.y + HEADER_H).min(r.max.y)));
                painter.rect_filled(head, CornerRadius::ZERO, fa(head_bg));
                if r.width() >= 24.0 {
                    let tc = fa(text_on(head_bg));
                    let clip = painter.with_clip_rect(head.shrink2(Vec2::new(3.0, 0.0)).intersect(clip));
                    // 폭 120 이상이면 오른쪽에 크기 (재어서 이름 자리를 정한다)
                    let size_g = (head.width() >= 120.0)
                        .then(|| painter.layout_no_wrap(human_size(n.size(mode)), font(TILE_SIZE, false), tc.gamma_multiply(0.85)));
                    let size_w = size_g.as_ref().map_or(0.0, |g| g.size().x + 6.0);
                    let name_max = (head.width() - 8.0 - size_w).max(10.0);
                    // 접근 거부·중지로 읽지 못한 폴더가 안에 있다 (따라 내려가면 그 폴더에 닿는다)
                    let warn = if n.incomplete() { " ⚠" } else { "" };
                    let at = Pos2::new(head.min.x + 4.0, head.center().y);
                    label(&clip, at, format!("{name}{warn}"), font(DIR_TITLE, true), tc, name_max, Align2::LEFT_CENTER);
                    if let Some(g) = size_g {
                        let rect = Align2::RIGHT_CENTER.anchor_size(Pos2::new(head.max.x - 4.0, head.center().y), g.size());
                        clip.galley(rect.min, g, tc);
                    }
                }
                painter.rect_stroke(r, CornerRadius::ZERO, Stroke::new(1.0, border), StrokeKind::Inside);
            }
            ItemKind::ClosedDir | ItemKind::File | ItemKind::Link | ItemKind::Group { .. } => {
                let mut bg = match it.kind {
                    ItemKind::ClosedDir => style.closed_dir(it.depth),
                    ItemKind::File => style.category(categorize(name)),
                    ItemKind::Link => pal.link,
                    _ => pal.group,
                };
                // 60% 밝기로 (불투명하게). 지금은 크기 0이라 그려지지 않는다
                if n.has(flags::HARDLINK_DUP) {
                    bg = lerp(bg, Color32::BLACK, 0.4);
                }
                if dim {
                    bg = lerp(bg, t.bg_app, 0.75);
                }
                if hovered {
                    bg = lerp(bg, HOVER_GLOW, 0.35);
                }
                painter.rect_filled(r, CornerRadius::ZERO, fa(bg));
                // 위쪽이 밝은 약한 입체감
                if r.width() > 6.0 && r.height() > 6.0 {
                    let hi = Rect::from_min_size(r.min, Vec2::new(r.width(), (r.height() * 0.45).min(14.0)));
                    painter.rect_filled(hi, CornerRadius::ZERO, fa(TILE_HIGHLIGHT));
                }
                painter.rect_stroke(r, CornerRadius::ZERO, Stroke::new(1.0, border), StrokeKind::Inside);
                if is_group {
                    hatch(painter, r, fa(GROUP_HATCH), 7.0, 1.0);
                }
                // 삭제 표시는 색을 섞지 않고 빨간 빗금과 테두리를 겹친다 (어떤 범주색 위에서도 같게 보이도록)
                if marked {
                    let m = Color32::from_rgba_unmultiplied(mark_color.r(), mark_color.g(), mark_color.b(), MARK_HATCH_ALPHA);
                    hatch(painter, r, fa(m), 8.0, 2.0);
                    painter.rect_stroke(r, CornerRadius::ZERO, Stroke::new(2.0, fa(mark_color)), StrokeKind::Inside);
                }

                if r.width() >= 34.0 && r.height() >= 14.0 {
                    let tc = fa(if dim { text_on(bg).gamma_multiply(0.5) } else { text_on(bg) });
                    let clip = painter.with_clip_rect(r.shrink(2.0).intersect(clip));
                    let (title, sub) = match it.kind {
                        ItemKind::Group { count, size } => {
                            let w = plural(u64::from(count), "small item", "small items");
                            (tr!("작은 항목 {count}개", "{count} {w}"), human_size(size))
                        }
                        ItemKind::Link => (format!("{name} →"), tr("링크", "Link").to_string()),
                        ItemKind::ClosedDir if n.incomplete() => (name.to_string(), format!("{} ⚠", human_size(n.size(mode)))),
                        _ => (name.to_string(), human_size(n.size(mode))),
                    };
                    // SpaceSniffer처럼 이름과 크기를 가운데에
                    let two_lines = r.height() >= 32.0;
                    let y = if two_lines { r.center().y - 14.0 } else { r.center().y - 8.0 };
                    let max_w = r.width() - 6.0;
                    label(&clip, Pos2::new(r.center().x, y), title, font(TILE_NAME, true), tc, max_w, Align2::CENTER_TOP);
                    if two_lines {
                        let sub_c = tc.gamma_multiply(0.8);
                        label(&clip, Pos2::new(r.center().x, y + 15.0), sub, font(TILE_SIZE, false), sub_c, max_w, Align2::CENTER_TOP);
                    }
                }
            }
        }
    }

    // 선택 테두리는 맨 위에
    if let Some(s) = hl.selected.and_then(|s| items.iter().find(|it| it.node == s && !matches!(it.kind, ItemKind::Group { .. }))) {
        let r = camera.map_or(s.rect, |c| c.map(s.rect));
        painter.rect_stroke(r, CornerRadius::ZERO, Stroke::new(2.5, t.accent), StrokeKind::Inside);
        // 안쪽의 어두운 선: 노란·주황 타일 위에서도 앰버 테두리가 보이게
        if r.width() > 10.0 && r.height() > 10.0 {
            painter.rect_stroke(r.shrink(2.5), CornerRadius::ZERO, Stroke::new(1.5, TILE_INK), StrokeKind::Inside);
        }
    }
}

/// 45° 빗금. `period`는 선에 수직으로 잰 간격 (CSS repeating-linear-gradient와 같다)
fn hatch(painter: &Painter, r: Rect, color: Color32, period: f32, width: f32) {
    let step = period * std::f32::consts::SQRT_2;
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

/// 목록 행의 범주 칩 (모서리 2, 안쪽 1px 검정 35%). 삭제 표시면 타일과 같은 빨간 빗금을 겹친다.
/// `fade`가 1보다 작으면 그만큼 흐리게 (검색 필터 밖)
pub fn chip(painter: &Painter, r: Rect, fill: Color32, marked: bool, mark: Color32, fade: f32) {
    let fa = |c: Color32| if fade < 1.0 { c.gamma_multiply(fade) } else { c };
    let radius = CornerRadius::same(2);
    painter.rect_filled(r, radius, fa(fill));
    if marked {
        let m = Color32::from_rgba_unmultiplied(mark.r(), mark.g(), mark.b(), MARK_HATCH_ALPHA);
        hatch(painter, r, fa(m), 8.0, 2.0);
    }
    painter.rect_stroke(r, radius, Stroke::new(1.0, fa(CHIP_EDGE)), StrokeKind::Inside);
}

/// 그리기 영역에서 쓸 반응 영역
pub fn canvas(ui: &mut egui::Ui) -> (egui::Response, Painter) {
    let size = ui.available_size();
    ui.allocate_painter(size, egui::Sense::click())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::time::{Duration, Instant};

    // The original membership-based implementation is deliberately independent
    // of Plan's indices, so it catches order and row-topology regressions.
    fn reference_arrange(plan: &mut Option<(Vec<u64>, Vec<usize>)>, entries: &mut [Entry], area: layout::Rect) -> Vec<layout::Rect> {
        let mut rects = Vec::with_capacity(entries.len());
        if let Some((order, old_rows)) = plan {
            let pos = |k: u64| order.iter().position(|&o| o == k).unwrap_or(usize::MAX);
            entries.sort_by_key(|e| pos(e.key));
            let mut rows = Vec::with_capacity(old_rows.len() + 1);
            let mut start = 0;
            for &len in old_rows.iter() {
                let row_keys = &order[start.min(order.len())..(start + len).min(order.len())];
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
                *plan = Some((entries.iter().map(|e| e.key).collect(), rows));
                return rects;
            }
            rects.clear();
        }
        entries.sort_by(|a, b| b.value.total_cmp(&a.value));
        let values: Vec<f64> = entries.iter().map(|e| e.value).collect();
        let mut rows = Vec::new();
        layout::squarify_with_rows(&values, area, &mut rects, &mut rows);
        *plan = Some((entries.iter().map(|e| e.key).collect(), rows));
        rects
    }

    fn entries(values: &[(u64, f64)]) -> Vec<Entry> {
        values.iter().enumerate().map(|(i, &(key, value))| Entry { key, value, node: Some(i as NodeId) }).collect()
    }

    #[test]
    fn indexed_rows_match_reference_with_ties_and_duplicates() {
        let seeds = [
            (vec![1, 2, 3, 4], vec![2, 2]),
            (vec![1, 2, 3, 4], vec![0, 2, 0, 2, 0]),
            (vec![1, 2, 3, 4], vec![1]),
            (vec![1, 2, 3, 4], vec![0, 0]),
            (vec![1, 2, 1, 4], vec![2, 2]),
            (vec![1, 1, 3, 4], vec![2, 2]),
            (vec![1, 2, 3, 4], vec![2, 8]),
            (vec![], vec![]),
        ];
        let inputs = [
            vec![(4, 10.0), (2, 10.0), (1, 10.0), (3, 10.0)],
            vec![(9, 10.0), (4, 10.0), (8, 10.0), (1, 10.0), (7, 10.0)],
            vec![(9, 1.0), (4, 1.0), (8, 1.0)],
            vec![(2, 10.0), (2, 10.0), (1, 10.0), (1, 10.0)],
            vec![(1, 10000.0), (3, 1.0), (4, 1.0), (2, 1.0)],
            // A size-mode change alters sizes but retains the path keys.
            vec![(4, 300.0), (2, 80.0), (3, 40.0), (1, 20.0)],
            vec![],
        ];
        let tree = Tree::default();
        let mut out = Vec::new();
        for (order, rows) in seeds {
            for area in [layout::Rect::new(5.0, 9.0, 400.0, 300.0), layout::Rect::new(0.0, 0.0, 2000.0, 10.0)] {
                // Each input also starts from the seeded topology; suffixes test
                // rebuilding the indices through successive layout changes.
                for start in 0..inputs.len() {
                    let mut plans = HashMap::from([(42, Plan::new(order.clone(), rows.clone()))]);
                    let mut reference = Some((order.clone(), rows.clone()));
                    let mut builder = Builder { tree: &tree, mode: SizeMode::Allocated, out: &mut out,
                        min_w: 0.0, min_h: 0.0, min_area: 0.0, max_depth: 8, plans: Some(&mut plans) };
                    for input in &inputs[start..] {
                        let mut actual = entries(input);
                        let mut expected = entries(input);
                        let expected_rects = reference_arrange(&mut reference, &mut expected, area);
                        let actual_rects = builder.arrange(42, &mut actual, area);
                        assert_eq!(actual_rects, expected_rects);
                        assert_eq!(actual.iter().map(|e| (e.key, e.node)).collect::<Vec<_>>(),
                            expected.iter().map(|e| (e.key, e.node)).collect::<Vec<_>>());
                        let stored = builder.plans.as_ref().unwrap().get(&42).unwrap();
                        let (expected_order, expected_rows) = reference.as_ref().unwrap();
                        assert_eq!(&stored.order, expected_order);
                        assert_eq!(&stored.rows, expected_rows);
                    }
                }
            }
        }
    }

    #[test]
    fn indexed_wide_rows_match_reference() {
        let tree = Tree::default();
        let mut out = Vec::new();
        let order: Vec<u64> = (0..MAX_CHILDREN as u64).collect();
        let rows = vec![50; MAX_CHILDREN / 50];
        let mut reference = Some((order.clone(), rows.clone()));
        let mut plans = HashMap::from([(42, Plan::new(order, rows))]);
        let mut builder = Builder { tree: &tree, mode: SizeMode::Allocated, out: &mut out,
            min_w: 0.0, min_h: 0.0, min_area: 0.0, max_depth: 8, plans: Some(&mut plans) };
        let area = layout::Rect::new(0.0, 0.0, 1600.0, 1200.0);
        for round in 0..3 {
            let input: Vec<_> = (0..MAX_CHILDREN as u64).rev().filter(|i| (i + round) % 7 != 0)
                .chain(4000..4050).map(|key| (key, (key % 5 + 1) as f64)).collect();
            let mut actual = entries(&input);
            let mut expected = entries(&input);
            assert_eq!(builder.arrange(42, &mut actual, area), reference_arrange(&mut reference, &mut expected, area));
            assert_eq!(actual.iter().map(|e| (e.key, e.node)).collect::<Vec<_>>(),
                expected.iter().map(|e| (e.key, e.node)).collect::<Vec<_>>());
            let stored = builder.plans.as_ref().unwrap().get(&42).unwrap();
            assert_eq!((&stored.order, &stored.rows), {
                let (order, rows) = reference.as_ref().unwrap(); (order, rows)
            });
        }
    }

    fn item(key: u64, x: f32) -> Item {
        Item { node: key as NodeId, rect: Rect::from_min_size(Pos2::new(x, 0.0), Vec2::new(100.0, 100.0)),
            depth: 1, kind: ItemKind::File, key, fade: 1.0 }
    }

    fn morph_key(generation: u64) -> MorphKey {
        MorphKey { layout_id: LayoutSource::Preview(7), generation }
    }

    fn assert_item(actual: &Item, expected: &Item) {
        assert_eq!(actual.node, expected.node);
        assert_eq!(actual.rect, expected.rect);
        assert_eq!(actual.depth, expected.depth);
        assert_eq!(actual.kind, expected.kind);
        assert_eq!(actual.key, expected.key);
        assert_eq!(actual.fade, expected.fade);
    }

    #[test]
    fn settled_morph_refreshes_metadata() {
        let now = Instant::now();
        let mut morph = Morph::default();
        let mut out = Vec::new();
        let original = item(1, 0.0);
        assert!(!morph.step_into_at(&[original], morph_key(1), &mut out, now));
        assert!(morph.is_settled(morph_key(1)));
        for (generation, kind) in [(2, ItemKind::ClosedDir), (3, ItemKind::Group { count: 99, size: 12345 }),
            (4, ItemKind::Group { count: 3, size: 4 })] {
            let fresh = Item { node: 700 + generation as NodeId, depth: 8, kind, ..original };
            assert!(!morph.step_into_at(&[fresh], morph_key(generation), &mut out, now));
            assert_item(&out[0], &fresh);
            assert!(morph.is_settled(morph_key(generation)));
            assert!(!morph.is_settled(morph_key(generation - 1)));
            // Even a repeated caller-owned key must never return old metadata.
            let newer = Item { node: fresh.node + 1, depth: 9, ..fresh };
            assert!(!morph.step_into_at(&[newer], morph_key(generation), &mut out, now));
            assert_item(&out[0], &newer);
        }
        let final_key = MorphKey { layout_id: LayoutSource::Document(7), generation: 4 };
        assert!(!morph.is_settled(final_key));
        morph.invalidate();
        assert!(!morph.is_settled(morph_key(4)));
        assert!(!morph.step_into_at(&[original], final_key, &mut out, now));
        assert!(morph.is_settled(final_key));
    }

    #[test]
    fn morph_preserves_move_fade_and_reset() {
        let now = Instant::now();
        let mut morph = Morph::default();
        let mut out = Vec::new();
        assert!(!morph.step_into_at(&[item(1, 0.0)], morph_key(1), &mut out, now));
        let target = [item(1, 20.0), item(2, 120.0)];
        assert!(morph.step_into_at(&target, morph_key(2), &mut out, now));
        assert_eq!(out[0].rect.min.x, 0.0);
        assert_eq!(out[0].fade, 1.0);
        assert_eq!(out[1].fade, 0.0);
        let half_move = now + Duration::from_millis(150);
        assert!(morph.step_into_at(&target, morph_key(2), &mut out, half_move));
        assert!((out[0].rect.min.x - 10.0).abs() < 1e-4);
        assert!((out[1].fade - ease_in_out(0.375)).abs() < 1e-4);
        let final_key = MorphKey { layout_id: LayoutSource::Document(8), generation: 1 };
        morph.invalidate();
        assert!(morph.step_into_at(&target, final_key, &mut out, half_move));
        assert!((out[0].rect.min.x - 10.0).abs() < 1e-4);
        let done = now + Duration::from_millis(401);
        assert!(!morph.step_into_at(&target, final_key, &mut out, done));
        assert!(morph.is_settled(final_key));
        for (actual, expected) in out.iter().zip(&target) { assert_item(actual, expected); }
        // Reorder and removal follow the new layout; large movement snaps.
        assert!(!morph.step_into_at(&[item(2, 300.0), item(1, 20.0)], morph_key(3), &mut out, done));
        assert_eq!(out[0].key, 2);
        assert_eq!(out[0].rect.min.x, 300.0);
        assert!(!morph.step_into_at(&[item(2, 300.0)], morph_key(4), &mut out, done));
        assert_eq!(out.len(), 1);
        assert!(!morph.tweens.contains_key(&1));
        morph.reset();
        assert!(!morph.is_settled(morph_key(4)));
        assert!(!morph.step_into_at(&[item(99, 0.0)], morph_key(4), &mut out, done));
        assert_eq!(out[0].fade, 1.0);
    }

    #[test]
    fn duplicate_morph_keys_never_take_settled_fast_path() {
        let now = Instant::now();
        let mut morph = Morph::default();
        let mut out = Vec::new();
        let duplicates = [item(1, 0.0), Item { node: 9, ..item(1, 100.0) }];
        assert!(!morph.step_into_at(&duplicates, morph_key(1), &mut out, now));
        assert!(!morph.is_settled(morph_key(1)));
        assert_eq!(out.len(), 2);
        assert_eq!(out[1].node, 9);
        assert!(morph.step_into_at(&duplicates, morph_key(1), &mut out, now + Duration::from_secs(1)));
        assert!(!morph.is_settled(morph_key(1)));
        assert_eq!(out[1].fade, 0.0);
    }

    #[test]
    fn morph_retargets_from_current_rect_and_respects_slide_limit() {
        let now = Instant::now();
        let mut morph = Morph::default();
        let mut out = Vec::new();
        morph.step_into_at(&[item(1, 0.0)], morph_key(1), &mut out, now);
        morph.step_into_at(&[item(1, 20.0)], morph_key(2), &mut out, now);
        let halfway = now + Duration::from_millis(150);
        assert!(morph.step_into_at(&[item(1, 30.0)], morph_key(3), &mut out, halfway));
        assert!((out[0].rect.min.x - 10.0).abs() < 1e-4);
        assert!(morph.step_into_at(&[item(1, 30.0)], morph_key(3), &mut out, now + Duration::from_millis(300)));
        assert!((out[0].rect.min.x - 20.0).abs() < 1e-4);
        let done = now + Duration::from_millis(451);
        assert!(!morph.step_into_at(&[item(1, 30.0)], morph_key(3), &mut out, done));
        assert!(morph.step_into_at(&[item(1, 70.0)], morph_key(4), &mut out, done));
        assert_eq!(out[0].rect.min.x, 30.0);
        let done = done + Duration::from_millis(301);
        assert!(!morph.step_into_at(&[item(1, 70.0)], morph_key(4), &mut out, done));
        assert!(!morph.step_into_at(&[item(1, 110.01)], morph_key(5), &mut out, done));
        assert_eq!(out[0].rect.min.x, 110.01);
        assert_eq!(out[0].fade, 1.0);
    }

    #[test]
    fn morph_reuses_buffers_after_warmup() {
        let now = Instant::now();
        let mut morph = Morph::default();
        let mut out = Vec::new();
        let items: Vec<_> = (0..3000).map(|i| item(i, i as f32)).collect();
        for generation in 0..2 { morph.step_into_at(&items, morph_key(generation), &mut out, now); }
        let capacities = (morph.tweens.capacity(), morph.scratch.capacity(), out.capacity());
        let pointer = out.as_ptr();
        for generation in 2..20 {
            let n = if generation % 2 == 0 { items.len() } else { 1500 };
            morph.step_into_at(&items[..n], morph_key(generation), &mut out, now + Duration::from_secs(generation));
            assert_eq!(out.capacity(), capacities.2);
            assert_eq!(out.as_ptr(), pointer);
            assert_eq!(morph.tweens.capacity(), capacities.0);
            assert_eq!(morph.scratch.capacity(), capacities.1);
        }
        morph.reset();
        assert!(morph.tweens.capacity() >= items.len());
        assert!(morph.scratch.capacity() >= items.len());
        morph.step_into_at(&items, morph_key(22), &mut out, now + Duration::from_secs(30));
        assert_eq!(out.capacity(), capacities.2);
    }

    #[test]
    fn layout_generation_only_advances_on_recompute() {
        let mut tree = Tree { nodes: vec![darak_core::Node { alloc: 1, logical: 1, name_off: 0,
            parent: darak_core::NO_NODE, first_child: 0, child_count: 0, files: 1, mtime: 0,
            name_len: 0, kind: Kind::File, flags: 0 }], ..Tree::default() };
        let mut layout = Layout::default();
        let area = Rect::from_min_size(Pos2::ZERO, Vec2::new(400.0, 300.0));
        assert_eq!(layout.generation(), 0);
        layout.update(&tree, 1, 0, area, SizeMode::Allocated, Detail::default());
        assert_eq!(layout.generation(), 1);
        layout.update(&tree, 1, 0, area, SizeMode::Allocated, Detail::default());
        assert_eq!(layout.generation(), 1);
        tree.version += 1;
        layout.update(&tree, 1, 0, area, SizeMode::Allocated, Detail::default());
        assert_eq!(layout.generation(), 2);
        layout.invalidate();
        assert_eq!(layout.generation(), 2);
        layout.update(&tree, 1, 0, area, SizeMode::Allocated, Detail::default());
        assert_eq!(layout.generation(), 3);
        tree.nodes.push(tree.nodes[0]);
        let shifted = area.translate(Vec2::new(1.0, 0.0));
        let resized = Rect::from_min_size(shifted.min, Vec2::new(401.0, 300.0));
        for (generation, tree_id, focus, area, mode, detail) in [
            (4, 1, 0, shifted, SizeMode::Allocated, Detail::default()),
            (5, 1, 0, resized, SizeMode::Allocated, Detail::default()),
            (6, 2, 0, resized, SizeMode::Allocated, Detail::default()),
            (7, 2, 1, resized, SizeMode::Allocated, Detail::default()),
            (8, 2, 1, resized, SizeMode::Logical, Detail::default()),
            (9, 2, 1, resized, SizeMode::Logical, Detail::default().more()),
        ] {
            layout.update(&tree, tree_id, focus, area, mode, detail);
            assert_eq!(layout.generation(), generation);
            layout.update(&tree, tree_id, focus, area, mode, detail);
            assert_eq!(layout.generation(), generation);
        }
    }
}
