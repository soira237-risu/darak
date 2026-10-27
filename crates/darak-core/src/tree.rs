//! 스캔 결과를 담는 아레나 트리.
//!
//! - 노드는 `Vec<Node>` 하나에 들어 있고 `u32` 인덱스로 서로 가리킨다.
//! - 한 디렉터리의 자식은 연속된 구간 `first_child .. first_child + child_count`에 있고,
//!   스캔 직후에는 할당 크기 내림차순으로 정렬돼 있다.
//! - 이름은 하나의 문자열 풀에 노드 순서대로 이어 붙인다. (나중에 검색과 직렬화에 유리)

use std::ops::Range;

pub type NodeId = u32;
pub const NO_NODE: NodeId = u32::MAX;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    File = 0,
    Dir = 1,
    /// 따라가지 않은 reparse point 디렉터리 (junction, 디렉터리 symlink, 마운트 지점)
    Link = 2,
}

pub mod flags {
    /// 열 수 없었던 디렉터리 (크기를 모름)
    pub const DENIED: u8 = 1 << 0;
    /// 이미 다른 경로에서 센 하드링크 (크기 0으로 취급)
    pub const HARDLINK_DUP: u8 = 1 << 1;
    /// reparse point 속성이 있는 파일
    pub const REPARSE: u8 = 1 << 2;
    /// UTF-16 → UTF-8 변환에서 깨진 이름 (경로 재구성이 정확하지 않음)
    pub const LOSSY_NAME: u8 = 1 << 3;
    /// 삭제되어 트리에서 빠진 노드
    pub const REMOVED: u8 = 1 << 4;
    /// 실제 파일이 아닌 표시용 노드 (미리 보기의 "파일 n개" 묶음)
    pub const SYNTHETIC: u8 = 1 << 5;
}

#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct Node {
    /// 할당 크기 (디렉터리면 하위 합계)
    pub alloc: u64,
    /// 논리 크기 (디렉터리면 하위 합계)
    pub logical: u64,
    pub name_off: u32,
    pub parent: NodeId,
    pub first_child: NodeId,
    pub child_count: u32,
    /// 하위 파일 수 (파일이면 1)
    pub files: u32,
    /// 수정 시각, 2000-01-01 UTC 기준 초
    pub mtime: u32,
    pub name_len: u16,
    pub kind: Kind,
    pub flags: u8,
}

const _: () = assert!(std::mem::size_of::<Node>() == 48);

impl Node {
    pub fn is_dir(&self) -> bool {
        self.kind == Kind::Dir
    }
    pub fn has(&self, flag: u8) -> bool {
        self.flags & flag != 0
    }
    pub fn size(&self, mode: SizeMode) -> u64 {
        match mode {
            SizeMode::Allocated => self.alloc,
            SizeMode::Logical => self.logical,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SizeMode {
    /// 디스크에서 실제로 차지하는 크기 (삭제하면 비는 공간)
    #[default]
    Allocated,
    /// 파일 내용의 바이트 수
    Logical,
}

#[derive(Clone, Default)]
pub struct Tree {
    pub nodes: Vec<Node>,
    pub names: String,
    /// 삭제 등으로 내용이 바뀔 때마다 증가 (UI 캐시 무효화용)
    pub version: u64,
}

impl Tree {
    pub const ROOT: NodeId = 0;

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id as usize]
    }

    pub fn name(&self, id: NodeId) -> &str {
        let n = self.node(id);
        let start = n.name_off as usize;
        &self.names[start..start + n.name_len as usize]
    }

    pub fn children(&self, id: NodeId) -> Range<NodeId> {
        let n = self.node(id);
        n.first_child..n.first_child + n.child_count
    }

    /// 삭제되지 않은 자식
    pub fn live_children(&self, id: NodeId) -> impl Iterator<Item = NodeId> + '_ {
        self.children(id).filter(|&c| !self.node(c).has(flags::REMOVED))
    }

    pub fn parent(&self, id: NodeId) -> Option<NodeId> {
        let p = self.node(id).parent;
        (p != NO_NODE).then_some(p)
    }

    /// 루트부터 id까지 (양 끝 포함)
    pub fn ancestry(&self, id: NodeId) -> Vec<NodeId> {
        let mut v = vec![id];
        let mut cur = id;
        while let Some(p) = self.parent(cur) {
            v.push(p);
            cur = p;
        }
        v.reverse();
        v
    }

    pub fn depth(&self, id: NodeId) -> usize {
        let mut d = 0;
        let mut cur = id;
        while let Some(p) = self.parent(cur) {
            d += 1;
            cur = p;
        }
        d
    }

    pub fn is_ancestor(&self, ancestor: NodeId, mut id: NodeId) -> bool {
        loop {
            if id == ancestor {
                return true;
            }
            match self.parent(id) {
                Some(p) => id = p,
                None => return false,
            }
        }
    }

    /// 화면 표시용 전체 경로 (루트 노드의 이름은 스캔한 경로 그대로다, 예: `C:\`)
    pub fn path(&self, id: NodeId) -> String {
        let chain = self.ancestry(id);
        let mut s = String::from(self.name(chain[0]));
        for &n in &chain[1..] {
            if !s.ends_with('\\') {
                s.push('\\');
            }
            s.push_str(self.name(n));
        }
        s
    }

    /// 트리의 메모리 사용량 (바이트, 대략)
    pub fn heap_bytes(&self) -> usize {
        self.nodes.capacity() * std::mem::size_of::<Node>() + self.names.capacity()
    }

    /// 서브트리 전체를 REMOVED로 표시하고 값을 0으로 만든다. 조상 합계는 한 번 차감하며 자식 구간은 유지한다.
    /// 새로 지운 (파일 수, 폴더 수)를 돌려준다 (상태 바 통계 갱신용). 루트이거나 이미 지운 노드면 (0, 0).
    pub fn remove(&mut self, id: NodeId) -> (u64, u64) {
        if id == Self::ROOT || self.node(id).has(flags::REMOVED) {
            return (0, 0);
        }
        let (alloc, logical, files) = {
            let n = self.node(id);
            (n.alloc, n.logical, n.files)
        };
        let mut cur = self.node(id).parent;
        while cur != NO_NODE {
            let p = &mut self.nodes[cur as usize];
            p.alloc = p.alloc.saturating_sub(alloc);
            p.logical = p.logical.saturating_sub(logical);
            p.files = p.files.saturating_sub(files);
            cur = p.parent;
        }
        let mut dirs = 0;
        let mut stack = vec![id];
        while let Some(current) = stack.pop() {
            stack.extend(self.children(current));
            let n = &mut self.nodes[current as usize];
            // 먼저 지운 하위 폴더는 이미 셌다
            if n.kind == Kind::Dir && !n.has(flags::REMOVED) {
                dirs += 1;
            }
            n.flags |= flags::REMOVED;
            n.alloc = 0;
            n.logical = 0;
            n.files = 0;
        }
        self.version += 1;
        (files as u64, dirs)
    }
}

/// FILETIME(1601년 기준 100ns) → 2000-01-01 UTC 기준 초
pub fn filetime_to_secs2000(ft: i64) -> u32 {
    const OFFSET: i64 = 12_591_158_400; // 1601→2000 초
    let secs = ft / 10_000_000 - OFFSET;
    secs.clamp(0, u32::MAX as i64) as u32
}

/// 2000-01-01 UTC 기준 초 → Unix 시각(초)
pub fn secs2000_to_unix(s: u32) -> i64 {
    s as i64 + 946_684_800
}
