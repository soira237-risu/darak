//! 스캔 결과 하나와 그 위의 탐색·선택·표시 상태

use std::collections::BTreeSet;

use darak_core::remove::protected_reason;
use darak_core::search::SearchResult;
use darak_core::{Kind, NodeId, ScanStats, SizeMode, Tree};

pub struct Doc {
    pub id: u64,
    pub tree: Tree,
    pub stats: ScanStats,
    pub focus: NodeId,
    pub selected: Option<NodeId>,
    pub hovered: Option<NodeId>,
    pub back: Vec<NodeId>,
    pub forward: Vec<NodeId>,
    /// 삭제 표시 (상위가 표시되면 하위는 빠진다)
    pub marks: BTreeSet<NodeId>,
    /// 노드별: 1 = 표시됨, 2 = 표시된 항목 안에 있음
    mark_bits: Vec<u8>,
    pub search: SearchState,
}

#[derive(Default)]
pub struct SearchState {
    pub query: String,
    pub applied: Option<(String, NodeId, SizeMode, u64)>,
    pub edited_at: Option<std::time::Instant>,
    pub result: Option<SearchResult>,
    pub error: Option<String>,
    pub elapsed_ms: f64,
    pub in_focus_only: bool,
    pub highlight: bool,
    /// 노드별: 1 = 일치, 2 = 일치 항목을 포함, 4 = 일치 항목 안에 있음
    pub bits: Vec<u8>,
}

impl Doc {
    pub fn new(id: u64, tree: Tree, stats: ScanStats) -> Self {
        let n = tree.len();
        Self {
            id,
            tree,
            stats,
            focus: Tree::ROOT,
            selected: None,
            hovered: None,
            back: Vec::new(),
            forward: Vec::new(),
            marks: BTreeSet::new(),
            mark_bits: vec![0; n],
            search: SearchState { highlight: true, ..Default::default() },
        }
    }

    pub fn navigate(&mut self, to: NodeId) {
        if to != self.focus && self.tree.node(to).kind == Kind::Dir {
            self.back.push(self.focus);
            self.forward.clear();
            self.focus = to;
        }
    }

    pub fn go_up(&mut self) {
        if let Some(p) = self.tree.parent(self.focus) {
            let from = self.focus;
            self.navigate(p);
            self.selected = Some(from);
        }
    }

    pub fn go_back(&mut self) {
        if let Some(b) = self.back.pop() {
            self.forward.push(self.focus);
            self.focus = b;
        }
    }

    pub fn go_forward(&mut self) {
        if let Some(f) = self.forward.pop() {
            self.back.push(self.focus);
            self.focus = f;
        }
    }

    /// 항목이 보이도록 상위 폴더로 이동하고 선택한다
    pub fn reveal(&mut self, id: NodeId) {
        let target = self.tree.parent(id).unwrap_or(Tree::ROOT);
        self.navigate(target);
        self.selected = Some(id);
    }

    /// `id`를 포함하는 focus의 직계 자식
    pub fn child_of_focus_toward(&self, id: NodeId) -> Option<NodeId> {
        let chain = self.tree.ancestry(id);
        let pos = chain.iter().position(|&n| n == self.focus)?;
        chain.get(pos + 1).copied()
    }

    // ---- 삭제 표시 ----

    pub fn is_marked(&self, id: NodeId) -> bool {
        self.mark_bits.get(id as usize).is_some_and(|&b| b != 0)
    }

    pub fn mark_state(&self, id: NodeId) -> u8 {
        self.mark_bits.get(id as usize).copied().unwrap_or(0)
    }

    /// 표시를 켜거나 끈다. 표시할 수 없으면 이유를 돌려준다.
    pub fn toggle_mark(&mut self, id: NodeId) -> Result<(), String> {
        if self.marks.remove(&id) {
            self.rebuild_mark_bits();
            return Ok(());
        }
        if self.mark_state(id) == 2 {
            return Err("상위 폴더가 이미 삭제 표시되어 있습니다".into());
        }
        if let Some(r) = protected_reason(&self.tree, id) {
            return Err(format!("보호된 항목: {r}"));
        }
        // 이 항목 아래의 기존 표시는 흡수
        let tree = &self.tree;
        self.marks.retain(|&m| !tree.is_ancestor(id, m));
        self.marks.insert(id);
        self.rebuild_mark_bits();
        Ok(())
    }

    pub fn clear_marks(&mut self) {
        self.marks.clear();
        self.rebuild_mark_bits();
    }

    pub fn marked_size(&self, mode: SizeMode) -> u64 {
        self.marks.iter().map(|&m| self.tree.node(m).size(mode)).sum()
    }

    /// 노드는 BFS 순서라서 부모 인덱스가 항상 자식보다 작다 → 한 번의 순방향 패스로 전파
    fn rebuild_mark_bits(&mut self) {
        let n = self.tree.len();
        self.mark_bits.clear();
        self.mark_bits.resize(n, 0);
        if self.marks.is_empty() {
            return;
        }
        for &m in &self.marks {
            self.mark_bits[m as usize] = 1;
        }
        let first = *self.marks.iter().next().unwrap() as usize;
        for i in first.max(1)..n {
            let p = self.tree.nodes[i].parent as usize;
            if !self.tree.nodes[i].has(darak_core::flags::REMOVED) && self.mark_bits[i] == 0 && self.mark_bits[p] != 0 {
                self.mark_bits[i] = 2;
            }
        }
    }

    /// 다른 문서의 결과는 NodeId를 읽기 전에 거부한다.
    pub fn apply_removed_for(&mut self, doc_id: u64, removed: &[NodeId]) -> bool {
        if self.id != doc_id || removed.iter().any(|&id| id as usize >= self.tree.len()) {
            return false;
        }
        self.apply_removed(removed);
        true
    }

    /// 삭제가 끝난 서브트리를 트리와 탐색·검색 상태에서 뺀다.
    pub fn apply_removed(&mut self, removed: &[NodeId]) {
        let version = self.tree.version;
        for &id in removed {
            if (id as usize) < self.tree.len() {
            self.tree.remove(id);
            }
        }
            if version == self.tree.version {
            return;
        }
        let tree = &self.tree;
        let live = |id: NodeId| {
            tree.nodes
                .get(id as usize)
                .is_some_and(|n| !n.has(darak_core::flags::REMOVED))
        };
        while !live(self.focus) {
            self.focus = tree.parent(self.focus).unwrap_or(Tree::ROOT);
        }
        self.selected = self.selected.filter(|&id| live(id));
        self.hovered = self.hovered.filter(|&id| live(id));
        self.back.retain(|&id| live(id));
        self.forward.retain(|&id| live(id));
        self.marks.retain(|&id| live(id));
        self.rebuild_mark_bits();
        self.search.applied = None;
        self.search.result = None;
        self.search.bits.clear();
        self.search.error = None;
        self.search.elapsed_ms = 0.0;
    }

    // ---- 검색 강조 ----

    pub fn set_search_bits(&mut self) {
        let n = self.tree.len();
        let bits = &mut self.search.bits;
        bits.clear();
        let Some(res) = &self.search.result else { return };
        bits.resize(n, 0);
        for &h in &res.hits {
            bits[h as usize] |= 1;
            let mut cur = self.tree.node(h).parent;
            while cur != darak_core::NO_NODE && bits[cur as usize] & 2 == 0 {
                bits[cur as usize] |= 2;
                cur = self.tree.node(cur).parent;
            }
        }
        for i in 1..n {
            let p = self.tree.nodes[i].parent as usize;
            if bits[p] & 5 != 0 {
                bits[i] |= 4;
            }
        }
    }

    /// 검색 강조가 켜져 있을 때, 흐리게 그리지 않을 항목인지
    pub fn search_lit(&self, id: NodeId) -> bool {
        self.search.bits.get(id as usize).is_none_or(|&b| b != 0)
    }

    pub fn search_active(&self) -> bool {
        self.search.highlight && self.search.result.is_some() && !self.search.bits.is_empty()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use darak_core::{NO_NODE, Node, ScanMethod, flags};
    use std::time::Duration;

    pub(crate) fn node(
        tree: &mut Tree,
        name: &str,
        parent: NodeId,
        kind: Kind,
        size: u64,
        first: NodeId,
        count: u32,
    ) {
        let name_off = tree.names.len() as u32;
        tree.names.push_str(name);
        tree.nodes.push(Node {
            alloc: size,
            logical: size,
            name_off,
            parent,
            first_child: first,
            child_count: count,
            files: if kind == Kind::File { 1 } else { count },
            mtime: 0,
            name_len: name.len() as u16,
            kind,
            flags: 0,
        });
    }

    pub(crate) fn doc(id: u64, tree: Tree) -> Doc {
        Doc::new(
            id,
            tree,
            ScanStats {
                elapsed: Duration::ZERO,
                files: 2,
                dirs: 2,
                errors: 0,
                hardlink_dups: 0,
                cancelled: false,
                method: ScanMethod::Walk,
                fallback_reason: None,
            },
        )
    }

    pub(crate) fn fixture(id: u64) -> Doc {
        let mut tree = Tree::default();
        node(&mut tree, "C:\\Data", NO_NODE, Kind::Dir, 100, 1, 2);
        node(&mut tree, "folder", 0, Kind::Dir, 60, 3, 1);
        node(&mut tree, "sibling", 0, Kind::File, 40, 0, 0);
        node(&mut tree, "child", 1, Kind::File, 60, 0, 0);
        doc(id, tree)
    }

    #[test]
    fn clears_removed_navigation_state() {
        let mut doc = fixture(1);
        doc.focus = 1;
        doc.selected = Some(3);
        doc.hovered = Some(3);
        doc.back = vec![0, 1];
        doc.forward = vec![1];
        doc.toggle_mark(3).unwrap();
        doc.search.applied = Some(("file:".into(), 0, SizeMode::Allocated, 0));
        doc.search.result = Some(darak_core::search::search(
            &doc.tree,
            &darak_core::search::Query::parse("file:", 0).unwrap(),
            SizeMode::Allocated,
            Tree::ROOT,
            100,
        ));
        doc.set_search_bits();
        doc.apply_removed(&[1]);
        assert_eq!(doc.focus, Tree::ROOT);
        assert_eq!(doc.selected, None);
        assert_eq!(doc.hovered, None);
        assert_eq!(doc.back, vec![0]);
        assert!(doc.forward.is_empty());
        assert!(doc.marks.is_empty());
        assert_eq!(doc.marked_size(SizeMode::Allocated), 0);
        assert!(doc.search.applied.is_none());
        assert!(doc.search.result.is_none());
        assert!(doc.search.bits.is_empty());
        assert!(doc.tree.node(3).has(flags::REMOVED));
    }

    fn unchanged(doc: &Doc) {
        assert_eq!(doc.tree.node(0).alloc, 100);
        assert_eq!(doc.tree.version, 0);
        assert_eq!(doc.selected, Some(3));
        assert_eq!(doc.marks.iter().copied().collect::<Vec<_>>(), vec![3]);
        assert!(doc.tree.nodes.iter().all(|n| !n.has(flags::REMOVED)));
    }

    #[test]
    fn rejects_foreign_document_results() {
        let mut other = fixture(2);
        other.selected = Some(3);
        other.toggle_mark(3).unwrap();
        assert!(!other.apply_removed_for(1, &[1]));
        unchanged(&other);
    }

    #[test]
    fn rejects_foreign_out_of_range_ids() {
        let mut other = fixture(2);
        other.selected = Some(3);
        other.toggle_mark(3).unwrap();
        assert!(!other.apply_removed_for(1, &[u32::MAX]));
        unchanged(&other);
        assert!(!other.apply_removed_for(2, &[1, u32::MAX]));
        unchanged(&other);
    }

    #[test]
    fn applies_matching_document_results() {
        let mut doc = fixture(1);
        assert!(doc.apply_removed_for(1, &[1]));
        assert_eq!(doc.tree.node(0).alloc, 40);
        assert!(doc.tree.node(1).has(flags::REMOVED));
    }
}
