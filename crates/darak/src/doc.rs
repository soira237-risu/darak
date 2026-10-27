//! 스캔 결과 하나와 그 위의 탐색·선택·표시 상태

use std::collections::BTreeSet;
use std::sync::Arc;

use crate::search_worker::{SearchCompletion, SearchKey};

use darak_core::remove::{Guard, protected_reason_as};
use darak_core::search::SearchResult;
use darak_core::{Kind, NodeId, ScanStats, SizeMode, Tree};

pub struct Doc {
    pub id: u64,
    pub tree: Arc<Tree>,
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
    /// 삭제 보호 수준 (관리자 권한으로 스캔한 문서는 Admin)
    pub guard: Guard,
    /// 지우다 실패해 일부만 지워졌을 수 있는 폴더 (크기를 믿기 어렵다, 다시 스캔해야 맞아진다)
    pub stale: BTreeSet<NodeId>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum RemovalApply { Applied, Rejected, Busy }

#[derive(Default)]
pub struct SearchState {
    pub query: String,
    pub applied: Option<SearchKey>,
    pub edited_at: Option<std::time::Instant>,
    /// Input-handler timestamp awaiting key observation.
    pub(crate) pending_edit_at: Option<std::time::Instant>,
    pub result: Option<SearchResult>,
    pub error: Option<String>,
    pub elapsed_ms: f64,
    pub in_focus_only: bool,
    pub highlight: bool,
    /// 노드별: 1 = 일치, 2 = 일치 항목을 포함, 4 = 일치 항목 안에 있음
    pub bits: Vec<u8>,
}

impl SearchState {
    pub fn note_edit(&mut self, at: std::time::Instant) {
        self.pending_edit_at = Some(at);
    }
}

impl Doc {
    pub fn new(id: u64, tree: Tree, stats: ScanStats) -> Self {
        let n = tree.len();
        Self {
            id,
            tree: Arc::new(tree),
            stats,
            focus: Tree::ROOT,
            selected: None,
            hovered: None,
            back: Vec::new(),
            forward: Vec::new(),
            marks: BTreeSet::new(),
            mark_bits: vec![0; n],
            search: SearchState { highlight: true, ..Default::default() },
            guard: Guard::default(),
            stale: BTreeSet::new(),
        }
    }

    pub fn navigate(&mut self, to: NodeId) {
        if to != self.focus && self.tree.node(to).kind == Kind::Dir {
            if self.search.in_focus_only {
                self.search.note_edit(std::time::Instant::now());
            }
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
            if b != self.focus && self.search.in_focus_only {
                self.search.note_edit(std::time::Instant::now());
            }
            self.forward.push(self.focus);
            self.focus = b;
        }
    }

    pub fn go_forward(&mut self) {
        if let Some(f) = self.forward.pop() {
            if f != self.focus && self.search.in_focus_only {
                self.search.note_edit(std::time::Instant::now());
            }
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

    /// 이 문서의 보호 수준에서 이 항목을 지우면 안 되는 이유. 지워도 되면 None.
    /// 표시·검토·실행 직전 검사가 모두 이 기준을 쓴다.
    pub fn protected(&self, id: NodeId) -> Option<&'static str> {
        protected_reason_as(&self.tree, id, self.guard)
    }

    /// 표시를 켜거나 끈다. 표시하면 이 항목 안에서 흡수한 기존 표시 수를, 표시할 수 없으면 이유를 돌려준다.
    pub fn toggle_mark(&mut self, id: NodeId) -> Result<usize, String> {
        if self.marks.remove(&id) {
            self.rebuild_mark_bits();
            return Ok(0);
        }
        if self.mark_state(id) == 2 {
            return Err("상위 폴더가 이미 삭제 표시되어 있습니다".into());
        }
        if let Some(r) = self.protected(id) {
            return Err(format!("보호된 항목: {r}"));
        }
        // 이 항목 아래의 기존 표시는 흡수
        let (tree, before) = (&self.tree, self.marks.len());
        self.marks.retain(|&m| !tree.is_ancestor(id, m));
        let absorbed = before - self.marks.len();
        self.marks.insert(id);
        self.rebuild_mark_bits();
        Ok(absorbed)
    }

    pub fn clear_marks(&mut self) {
        self.marks.clear();
        self.rebuild_mark_bits();
    }

    /// 지운 표시를 되돌린다. 같은 문서·같은 트리 버전이고 그 사이 새 표시가 없는지는 부르는 쪽이 확인한다.
    pub fn restore_marks(&mut self, marks: BTreeSet<NodeId>) {
        self.marks = marks;
        self.rebuild_mark_bits();
    }

    /// 일부만 지워졌을 수 있는 폴더로 기록한다 (범위 밖이거나 이미 지운 노드는 무시)
    pub fn mark_stale(&mut self, ids: &[NodeId]) {
        let tree = &self.tree;
        self.stale.extend(ids.iter().copied().filter(|&id| {
            tree.nodes.get(id as usize).is_some_and(|n| !n.has(darak_core::flags::REMOVED))
        }));
    }

    /// 이 항목이 일부만 지워진 폴더이거나 그 안에 있어 크기를 믿기 어려운지
    pub fn stale_within(&self, id: NodeId) -> bool {
        !self.stale.is_empty() && std::iter::successors(Some(id), |&n| self.tree.parent(n)).any(|n| self.stale.contains(&n))
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

    /// Reject identity and bounds before accessing any returned node.
    pub fn apply_removed_for(&mut self, doc_id: u64, removed: &[NodeId]) -> RemovalApply {
        if self.id != doc_id || removed.iter().any(|&id| id as usize >= self.tree.len()) {
            return RemovalApply::Rejected;
        }
        self.apply_removed(removed)
    }

    /// 다른 소유자가 트리를 계속 붙잡고 있을 때만 쓴다. 트리를 복제해 이 문서만 소유한다.
    pub fn detach_tree(&mut self) {
        Arc::make_mut(&mut self.tree);
    }

    pub fn apply_removed(&mut self, removed: &[NodeId]) -> RemovalApply {
        if removed.iter().any(|&id| id as usize >= self.tree.len()) {
            return RemovalApply::Rejected;
        }
        let Some(tree) = Arc::get_mut(&mut self.tree) else { return RemovalApply::Busy };
        let version = tree.version;
        let (mut files, mut dirs) = (0, 0);
        for &id in removed {
            let (f, d) = tree.remove(id);
            files += f;
            dirs += d;
        }
        if version == tree.version { return RemovalApply::Applied; }
        // 상태 바의 파일·폴더 수도 지운 만큼 줄인다
        self.stats.files = self.stats.files.saturating_sub(files);
        self.stats.dirs = self.stats.dirs.saturating_sub(dirs);
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
        self.stale.retain(|&id| live(id));
        self.rebuild_mark_bits();
        self.clear_search();
        RemovalApply::Applied
    }

    pub fn clear_search(&mut self) {
        self.search.applied = None;
        self.search.result = None;
        self.search.bits.clear();
        self.search.error = None;
        self.search.elapsed_ms = 0.0;
    }

    pub fn search_key(&self, mode: SizeMode) -> SearchKey {
        SearchKey {
            doc_id: self.id, tree_version: self.tree.version,
            query: self.search.query.clone(),
            scope: if self.search.in_focus_only { self.focus } else { Tree::ROOT }, mode,
        }
    }

    pub fn accept_search(&mut self, completion: SearchCompletion, expected: &SearchKey, generation: u64) -> bool {
        // Never inspect result IDs until identity, version, input and generation agree.
        if completion.generation != generation || &completion.key != expected
            || expected != &self.search_key(expected.mode) {
            return false;
        }
        self.search.edited_at = Some(completion.edited_at);
        self.search.elapsed_ms = completion.timings.search.as_secs_f64() * 1000.0;
        self.search.applied = Some(completion.key);
        self.search.result = Some(completion.result);
        self.search.bits = completion.bits;
        self.search.error = None;
        true
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
        doc.search.applied = Some(doc.search_key(SizeMode::Allocated));
        doc.search.result = Some(darak_core::search::search(
            &doc.tree,
            &darak_core::search::Query::parse("file:", 0).unwrap(),
            SizeMode::Allocated,
            Tree::ROOT,
            100,
        ));
        doc.search.bits = vec![2, 2, 1, 1];
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
        assert_eq!(other.apply_removed_for(1, &[1]), RemovalApply::Rejected);
        unchanged(&other);
    }

    #[test]
    fn rejects_foreign_out_of_range_ids() {
        let mut other = fixture(2);
        other.selected = Some(3);
        other.toggle_mark(3).unwrap();
        assert_eq!(other.apply_removed_for(1, &[u32::MAX]), RemovalApply::Rejected);
        unchanged(&other);
        assert_eq!(other.apply_removed_for(2, &[1, u32::MAX]), RemovalApply::Rejected);
        unchanged(&other);
    }

    #[test]
    fn removal_updates_stats_and_stale_folders() {
        let mut doc = fixture(1);
        doc.mark_stale(&[1, 2, u32::MAX]);
        assert_eq!(doc.stale.iter().copied().collect::<Vec<_>>(), [1, 2]);
        assert!(doc.stale_within(1) && doc.stale_within(3) && doc.stale_within(2));
        assert!(!doc.stale_within(Tree::ROOT));
        assert_eq!(doc.apply_removed(&[1]), RemovalApply::Applied);
        // 지운 폴더 하나와 그 안의 파일 하나만큼 줄어든다
        assert_eq!((doc.stats.files, doc.stats.dirs), (1, 1));
        assert_eq!(doc.stale.iter().copied().collect::<Vec<_>>(), [2]);
        // 이미 지운 노드는 기록하지 않는다
        doc.stale.clear();
        doc.mark_stale(&[1, 3]);
        assert!(doc.stale.is_empty());
        assert!(!doc.stale_within(2));
        // 같은 노드를 다시 지워도 두 번 빼지 않는다
        doc.apply_removed(&[1, 2]);
        assert_eq!((doc.stats.files, doc.stats.dirs), (0, 1));
    }

    #[test]
    fn toggle_mark_counts_absorbed_marks_and_restores() {
        let mut doc = fixture(1);
        assert_eq!(doc.toggle_mark(3), Ok(0));
        assert_eq!(doc.toggle_mark(2), Ok(0));
        assert_eq!(doc.toggle_mark(1), Ok(1));
        assert_eq!(doc.marks, BTreeSet::from([1, 2]));
        // 상위 표시를 풀어도 흡수한 표시는 돌아오지 않는다
        assert_eq!(doc.toggle_mark(1), Ok(0));
        assert_eq!(doc.marks, BTreeSet::from([2]));
        doc.clear_marks();
        doc.restore_marks(BTreeSet::from([2, 3]));
        assert_eq!((doc.mark_state(3), doc.mark_state(2), doc.mark_state(1)), (1, 1, 0));
        assert_eq!(doc.marked_size(SizeMode::Allocated), 100);
    }

    #[test]
    fn applies_matching_document_results() {
        let mut doc = fixture(1);
        assert_eq!(doc.apply_removed_for(1, &[1]), RemovalApply::Applied);
        assert_eq!(doc.tree.node(0).alloc, 40);
        assert!(doc.tree.node(1).has(flags::REMOVED));
    }
    #[test]
    fn rejects_foreign_completion_before_indexing() {
        let old = fixture(1);
        let mut current = fixture(2);
        let expected = current.search_key(SizeMode::Allocated);
        let completion = SearchCompletion {
            key: old.search_key(SizeMode::Allocated), generation: 7,
            result: SearchResult { hits: vec![u32::MAX], total: 1, total_size: 60 },
            bits: vec![], timings: Default::default(), edited_at: std::time::Instant::now(),
        };
        assert_eq!(old.tree.version, current.tree.version);
        assert!(!current.accept_search(completion, &expected, 7));
        assert!(current.search.result.is_none());
    }

    #[test]
    fn busy_removal_preserves_all_state() {
        let mut doc = fixture(1);
        doc.selected = Some(3);
        doc.hovered = Some(3);
        doc.focus = 1;
        doc.back = vec![0];
        doc.forward = vec![1];
        doc.toggle_mark(3).unwrap();
        doc.search.query = "file:".into();
        doc.search.applied = Some(doc.search_key(SizeMode::Allocated));
        doc.search.bits = vec![2, 2, 1, 1];
        let lease = Arc::clone(&doc.tree);
        assert_eq!(doc.apply_removed_for(1, &[1]), RemovalApply::Busy);
        unchanged(&doc);
        assert_eq!(doc.focus, 1);
        assert_eq!(doc.hovered, Some(3));
        assert_eq!(doc.back, [0]);
        assert_eq!(doc.forward, [1]);
        assert!(doc.search.applied.is_some());
        assert_eq!(doc.search.bits, [2, 2, 1, 1]);
        drop(lease);
        assert_eq!(doc.apply_removed_for(1, &[1, 3, 1]), RemovalApply::Applied);
        assert_eq!(doc.tree.node(0).alloc, 40);
        assert_eq!(doc.tree.version, 1);
        assert!(doc.search.bits.is_empty());
    }

    #[test]
    fn accepts_complete_vectors_only_for_current_generation_and_key() {
        let mut doc = fixture(1);
        doc.search.query = "file:".into();
        let key = doc.search_key(SizeMode::Allocated);
        let make = |generation, key| SearchCompletion {
            generation, key, result: SearchResult { hits: vec![3, 2], total: 2, total_size: 100 },
            bits: vec![2, 2, 1, 1], timings: Default::default(), edited_at: std::time::Instant::now(),
        };
        assert!(!doc.accept_search(make(4, key.clone()), &key, 5));
        let mut stale = key.clone(); stale.tree_version += 1;
        assert!(!doc.accept_search(make(5, stale), &key, 5));
        let completion = make(5, key.clone());
        let hits = completion.result.hits.as_ptr();
        let bits = completion.bits.as_ptr();
        assert!(doc.accept_search(completion, &key, 5));
        assert_eq!(doc.search.result.as_ref().unwrap().hits.as_ptr(), hits);
        assert_eq!(doc.search.bits.as_ptr(), bits);
        assert_eq!(doc.search.result.as_ref().unwrap().total_size, 100);
    }

}
