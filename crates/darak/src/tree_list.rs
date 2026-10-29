//! 트리 목록의 상태와 계산 (그리기는 list_panel.rs).
//!
//! - 펼친 폴더 집합은 문서(`Doc::expanded`)에 있다. 이동해도 남고, 지운 폴더는 `Doc::apply_removed`가 뺀다.
//! - 보이는 행은 focus의 자식부터 펼친 폴더를 따라 깊이 우선으로 편 목록이다. 형제는 크기 내림차순.
//! - 행 목록은 (문서, 트리 버전, focus, 펼침 세대, 크기 기준)이 같으면 다시 만들지 않는다.

use std::collections::BTreeSet;

use darak_core::{Kind, NodeId, SizeMode, Tree, flags};

use crate::doc::Doc;

/// 펼친 폴더. 바뀔 때마다 세대가 올라 행 캐시를 무효로 만든다.
#[derive(Default)]
pub struct Expanded {
    set: BTreeSet<NodeId>,
    generation: u64,
}

impl Expanded {
    pub fn contains(&self, id: NodeId) -> bool {
        self.set.contains(&id)
    }

    pub fn insert(&mut self, id: NodeId) {
        if self.set.insert(id) {
            self.generation += 1;
        }
    }

    pub fn remove(&mut self, id: NodeId) {
        if self.set.remove(&id) {
            self.generation += 1;
        }
    }

    pub fn toggle(&mut self, id: NodeId) {
        if self.contains(id) {
            self.remove(id);
        } else {
            self.insert(id);
        }
    }

    /// 조건에 맞는 폴더만 남긴다 (지운 폴더 정리)
    pub fn retain(&mut self, keep: impl Fn(NodeId) -> bool) {
        let before = self.set.len();
        self.set.retain(|&id| keep(id));
        if self.set.len() != before {
            self.generation += 1;
        }
    }

    fn generation(&self) -> u64 {
        self.generation
    }
}

/// 보이는 행 하나
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Row {
    pub id: NodeId,
    /// focus의 자식이 0
    pub depth: u32,
    /// 펼칠 자식이 있는 폴더 (▸ ▾)
    pub expandable: bool,
    pub open: bool,
}

/// 펼칠 수 있는 폴더: 접근이 거부되지 않았고 지우지 않은 자식이 있다. 링크는 따라가지 않는다.
fn expandable(tree: &Tree, id: NodeId) -> bool {
    let n = tree.node(id);
    n.kind == Kind::Dir && !n.has(flags::DENIED) && tree.live_children(id).next().is_some()
}

/// 지우지 않은 자식, 크기 내림차순 (같으면 트리 순서. 안정 정렬)
fn sorted_children(tree: &Tree, id: NodeId, mode: SizeMode) -> Vec<NodeId> {
    let mut kids: Vec<NodeId> = tree.live_children(id).collect();
    kids.sort_by_key(|&c| std::cmp::Reverse(tree.node(c).size(mode)));
    kids
}

/// focus 아래의 보이는 행. 재귀하지 않는다 (아주 깊은 폴더도 스택이 넘치지 않게)
pub fn flatten(tree: &Tree, focus: NodeId, expanded: &Expanded, mode: SizeMode) -> Vec<Row> {
    let mut rows = Vec::new();
    let mut stack = vec![sorted_children(tree, focus, mode).into_iter()];
    while let Some(top) = stack.last_mut() {
        let Some(id) = top.next() else {
            stack.pop();
            continue;
        };
        let depth = (stack.len() - 1) as u32;
        let expandable = expandable(tree, id);
        let open = expandable && expanded.contains(id);
        rows.push(Row { id, depth, expandable, open });
        if open {
            stack.push(sorted_children(tree, id, mode).into_iter());
        }
    }
    rows
}

/// focus와 `id` 사이의 조상 폴더를 모두 펼친다 (트리맵에서 고른 항목이 목록에 보이게)
fn expand_to(tree: &Tree, focus: NodeId, expanded: &mut Expanded, id: NodeId) {
    let chain = tree.ancestry(id);
    let Some(pos) = chain.iter().position(|&n| n == focus) else { return };
    for &a in &chain[pos + 1..chain.len() - 1] {
        expanded.insert(a);
    }
}

/// 목록 키보드
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Up,
    Down,
    Home,
    End,
    /// 펼쳐져 있으면 접고, 아니면 부모 행으로
    Left,
    /// 닫힌 폴더면 펼치고, 펼쳐져 있으면 첫 자식으로
    Right,
}

/// 키 하나를 처리하고 새 선택을 돌려준다 (펼치기·접기만 했으면 그대로). 선택이 목록 밖이면 ↓·Home은 첫 행, ↑·End는 끝 행.
pub fn step(rows: &[Row], expanded: &mut Expanded, selected: Option<NodeId>, step: Step) -> Option<NodeId> {
    let (Some(first), Some(last)) = (rows.first(), rows.last()) else { return selected };
    let at = selected.and_then(|s| rows.iter().position(|r| r.id == s));
    match (step, at) {
        (Step::Home, _) | (Step::Down, None) => Some(first.id),
        (Step::End, _) | (Step::Up, None) => Some(last.id),
        (Step::Up, Some(i)) => Some(rows[i.saturating_sub(1)].id),
        (Step::Down, Some(i)) => Some(rows[(i + 1).min(rows.len() - 1)].id),
        (Step::Right, Some(i)) => {
            let r = rows[i];
            if r.expandable && !r.open {
                expanded.insert(r.id);
                selected
            } else {
                rows.get(i + 1).filter(|c| r.open && c.depth == r.depth + 1).map(|c| c.id).or(selected)
            }
        }
        (Step::Left, Some(i)) => {
            let r = rows[i];
            if r.open {
                expanded.remove(r.id);
                selected
            } else {
                rows[..i].iter().rev().find(|p| p.depth + 1 == r.depth).map(|p| p.id).or(selected)
            }
        }
        (Step::Left | Step::Right, None) => selected,
    }
}

/// 한 번 스크롤할 곳
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scroll {
    /// 다른 폴더로 옮겼다
    Top,
    /// 선택한 행이 보이게
    To(NodeId),
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Key {
    doc: u64,
    version: u64,
    focus: NodeId,
    expanded: u64,
    mode: SizeMode,
}

/// 목록 패널이 프레임 사이에 들고 있는 것: 보이는 행 캐시와 선택을 따라가는 스크롤 요청
#[derive(Default)]
pub struct View {
    key: Option<Key>,
    rows: Vec<Row>,
    /// 마지막으로 본 (문서, focus, 선택)
    seen: Option<(u64, NodeId, Option<NodeId>)>,
    scroll: Option<Scroll>,
    /// 오른쪽 클릭 메뉴를 연 항목 (행이 밀려도 메뉴는 이 항목을 다룬다)
    pub menu: Option<NodeId>,
    /// 트리맵에서 가리킨 항목 (작은 항목 묶음은 빼서 그 폴더 행이 밝아지지 않게)
    pub map_hover: Option<NodeId>,
}

impl View {
    /// 보이는 행. 키가 같으면 지난번 목록을 그대로 쓴다 (프레임마다 정렬하지 않는다)
    pub fn rows(&mut self, doc: &Doc, mode: SizeMode) -> &[Row] {
        let key = Key { doc: doc.id, version: doc.tree.version, focus: doc.focus, expanded: doc.expanded.generation(), mode };
        if self.key != Some(key) {
            self.rows = flatten(&doc.tree, doc.focus, &doc.expanded, mode);
            self.key = Some(key);
        }
        &self.rows
    }

    /// 선택이 바뀌었으면 (트리맵 클릭 · 키보드 · go_up) 조상을 펼치고 그 행으로 한 번 스크롤한다.
    /// 선택 없이 다른 폴더로 옮겼으면 맨 위로.
    pub fn sync(&mut self, doc: &mut Doc) {
        let now = (doc.id, doc.focus, doc.selected);
        if self.seen == Some(now) {
            return;
        }
        let moved = self.seen.is_none_or(|(id, focus, _)| id != doc.id || focus != doc.focus);
        self.seen = Some(now);
        match doc.selected {
            Some(s) if s != doc.focus && doc.tree.is_ancestor(doc.focus, s) => {
                expand_to(&doc.tree, doc.focus, &mut doc.expanded, s);
                self.scroll = Some(Scroll::To(s));
            }
            _ if moved => self.scroll = Some(Scroll::Top),
            _ => {}
        }
    }

    pub fn take_scroll(&mut self) -> Option<Scroll> {
        self.scroll.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::tests::{doc, fixture, node};
    use darak_core::NO_NODE;

    fn ids(rows: &[Row]) -> Vec<(NodeId, u32)> {
        rows.iter().map(|r| (r.id, r.depth)).collect()
    }

    // 할당 크기와 논리 크기 순서가 다른 폴더
    //   0 root ─ 1 a (dir) ─ 4 a1, 5 a2
    //          ├ 2 b (file)
    //          └ 3 c (dir, 빈 폴더)
    fn mixed() -> Doc {
        let mut tree = Tree::default();
        node(&mut tree, "C:\\Data", NO_NODE, Kind::Dir, 100, 1, 3);
        node(&mut tree, "a", 0, Kind::Dir, 60, 4, 2);
        node(&mut tree, "b", 0, Kind::File, 40, 0, 0);
        node(&mut tree, "c", 0, Kind::Dir, 0, 0, 0);
        node(&mut tree, "a1", 1, Kind::File, 20, 0, 0);
        node(&mut tree, "a2", 1, Kind::File, 40, 0, 0);
        // 논리 크기로는 b가 a보다 크다
        tree.nodes[1].logical = 30;
        tree.nodes[4].logical = 25;
        tree.nodes[5].logical = 5;
        doc(1, tree)
    }

    #[test]
    fn flattens_by_size_and_follows_expanded_folders() {
        let mut d = mixed();
        let rows = flatten(&d.tree, d.focus, &d.expanded, SizeMode::Allocated);
        assert_eq!(ids(&rows), [(1, 0), (2, 0), (3, 0)]);
        // 빈 폴더와 파일은 펼칠 수 없다
        assert_eq!(rows.iter().map(|r| r.expandable).collect::<Vec<_>>(), [true, false, false]);
        d.expanded.insert(1);
        let rows = flatten(&d.tree, d.focus, &d.expanded, SizeMode::Allocated);
        assert_eq!(ids(&rows), [(1, 0), (5, 1), (4, 1), (2, 0), (3, 0)]);
        assert!(rows[0].open);
        // 논리 크기 순서
        let rows = flatten(&d.tree, d.focus, &d.expanded, SizeMode::Logical);
        assert_eq!(ids(&rows), [(2, 0), (1, 0), (4, 1), (5, 1), (3, 0)]);
        // 접근이 거부된 폴더는 펼치지 않는다
        tree_mut(&mut d).nodes[1].flags |= flags::DENIED;
        let rows = flatten(&d.tree, d.focus, &d.expanded, SizeMode::Allocated);
        assert_eq!(ids(&rows), [(1, 0), (2, 0), (3, 0)]);
        assert!(!rows[0].expandable && !rows[0].open);
    }

    fn tree_mut(d: &mut Doc) -> &mut Tree {
        std::sync::Arc::get_mut(&mut d.tree).unwrap()
    }

    #[test]
    fn cache_rebuilds_on_expand_and_focus() {
        let mut d = fixture(1);
        let mut view = View::default();
        assert_eq!(ids(view.rows(&d, SizeMode::Allocated)), [(1, 0), (2, 0)]);
        let generation = d.expanded.generation();
        d.expanded.toggle(1);
        assert_ne!(d.expanded.generation(), generation);
        assert_eq!(ids(view.rows(&d, SizeMode::Allocated)), [(1, 0), (3, 1), (2, 0)]);
        // 이미 펼친 폴더를 다시 넣어도 세대는 그대로
        let generation = d.expanded.generation();
        d.expanded.insert(1);
        assert_eq!(d.expanded.generation(), generation);
        d.expanded.toggle(1);
        assert_eq!(ids(view.rows(&d, SizeMode::Allocated)), [(1, 0), (2, 0)]);
        // 폴더로 들어가면 그 폴더의 자식이 뿌리. 나와도 펼침은 남는다
        d.expanded.insert(1);
        d.navigate(1);
        assert_eq!(ids(view.rows(&d, SizeMode::Allocated)), [(3, 0)]);
        d.go_back();
        assert_eq!(ids(view.rows(&d, SizeMode::Allocated)), [(1, 0), (3, 1), (2, 0)]);
    }

    #[test]
    fn removal_prunes_expanded_folders_and_rows() {
        let mut d = mixed();
        let mut view = View::default();
        d.expanded.insert(1);
        assert_eq!(view.rows(&d, SizeMode::Allocated).len(), 5);
        d.apply_removed(&[5]);
        // 남은 자식은 그대로 펼쳐 보이고, 작아진 a는 b 뒤로 간다
        assert!(d.expanded.contains(1));
        assert_eq!(ids(view.rows(&d, SizeMode::Allocated)), [(2, 0), (1, 0), (4, 1), (3, 0)]);
        d.apply_removed(&[1]);
        assert!(!d.expanded.contains(1));
        assert_eq!(ids(view.rows(&d, SizeMode::Allocated)), [(2, 0), (3, 0)]);
    }

    #[test]
    fn keyboard_moves_expands_and_collapses() {
        let mut d = fixture(1);
        let mut view = View::default();
        let mut press = |d: &mut Doc, s: Step| {
            let rows = view.rows(d, SizeMode::Allocated).to_vec();
            d.selected = step(&rows, &mut d.expanded, d.selected, s);
        };
        // 선택이 없으면 ↓는 첫 행
        press(&mut d, Step::Down);
        assert_eq!(d.selected, Some(1));
        press(&mut d, Step::Up);
        assert_eq!(d.selected, Some(1));
        // → 펼치기, 한 번 더 → 첫 자식
        press(&mut d, Step::Right);
        assert!(d.expanded.contains(1));
        assert_eq!(d.selected, Some(1));
        press(&mut d, Step::Right);
        assert_eq!(d.selected, Some(3));
        // 파일에서 →는 그대로, ←는 부모 행
        press(&mut d, Step::Right);
        assert_eq!(d.selected, Some(3));
        press(&mut d, Step::Left);
        assert_eq!(d.selected, Some(1));
        // ← 펼친 폴더는 접는다
        press(&mut d, Step::Left);
        assert!(!d.expanded.contains(1));
        press(&mut d, Step::End);
        assert_eq!(d.selected, Some(2));
        press(&mut d, Step::Down);
        assert_eq!(d.selected, Some(2));
        // 맨 위 행에서 ←는 그대로
        press(&mut d, Step::Home);
        press(&mut d, Step::Left);
        assert_eq!(d.selected, Some(1));
        // 목록 밖의 선택(지금 폴더 자신)에서 ↑는 끝 행
        d.selected = Some(0);
        press(&mut d, Step::Up);
        assert_eq!(d.selected, Some(2));
    }

    #[test]
    fn selection_from_treemap_expands_ancestors_once() {
        let mut d = mixed();
        let mut view = View::default();
        // 처음 보면 맨 위로
        view.sync(&mut d);
        assert_eq!(view.take_scroll(), Some(Scroll::Top));
        d.selected = Some(4);
        view.sync(&mut d);
        assert!(d.expanded.contains(1));
        assert_eq!(view.take_scroll(), Some(Scroll::To(4)));
        // 같은 선택이면 다시 펼치지 않는다 (사용자가 접은 폴더는 그대로)
        d.expanded.remove(1);
        view.sync(&mut d);
        assert!(!d.expanded.contains(1));
        assert_eq!(view.take_scroll(), None);
        // 다른 폴더로 옮기면 맨 위로, 선택이 그 안이면 그 행으로
        d.navigate(1);
        view.sync(&mut d);
        assert_eq!(view.take_scroll(), Some(Scroll::To(4)));
        d.selected = None;
        d.go_back();
        view.sync(&mut d);
        assert_eq!(view.take_scroll(), Some(Scroll::Top));
    }
}
