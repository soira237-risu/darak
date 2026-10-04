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
            if self.mark_bits[i] == 0 && self.mark_bits[p] != 0 {
                self.mark_bits[i] = 2;
            }
        }
    }

    /// 삭제가 끝난 노드를 트리에서 빼고 표시를 정리한다
    pub fn apply_removed(&mut self, removed: &[NodeId]) {
        for &id in removed {
            self.tree.remove(id);
            self.marks.remove(&id);
            if self.selected.is_some_and(|s| self.tree.is_ancestor(id, s)) {
                self.selected = None;
            }
            if self.tree.is_ancestor(id, self.focus) {
                self.focus = self.tree.parent(id).unwrap_or(Tree::ROOT);
            }
        }
        let tree = &self.tree;
        self.back.retain(|&b| !tree.node(b).has(darak_core::flags::REMOVED));
        self.forward.retain(|&f| !tree.node(f).has(darak_core::flags::REMOVED));
        self.rebuild_mark_bits();
        self.search.applied = None; // 결과 다시 계산
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
