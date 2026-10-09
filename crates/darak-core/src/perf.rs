//! Deterministic synthetic corpus and measurement helpers, excluded from normal builds.
use crate::search::SearchResult;
use crate::{Kind, NO_NODE, Node, Tree};
use std::time::Duration;

#[derive(Clone, Copy, Debug)]
pub enum Shape {
    Wide,
    Deep,
    Balanced,
}
pub const FIXTURE_NOW_SECS2000: u32 = 844646400;
pub const FIXTURE_SEED: u64 = 20261007;

/// Includes ROOT. Balanced directories have at most 64 children; Deep caps depth at 1000.
pub fn make_fixture(nodes: usize, shape: Shape, unicode_percent: u8) -> Tree {
    assert!(
        nodes > 0 && nodes < u32::MAX as usize,
        "nodes must include ROOT and fit NodeId"
    );
    assert!(unicode_percent <= 100, "Unicode percentage must be 0..=100");
    let mut tree = Tree {
        nodes: Vec::with_capacity(nodes),
        ..Tree::default()
    };
    push(&mut tree, NO_NODE, Kind::Dir, unicode_percent);
    let mut parent = 0usize;
    while tree.len() < nodes {
        if tree.nodes[parent].kind != Kind::Dir {
            parent += 1;
            continue;
        }
        let remaining = nodes - tree.len();
        let count = match shape {
            Shape::Wide => remaining,
            Shape::Deep if parent < 999 => 1,
            Shape::Deep => remaining,
            Shape::Balanced => remaining.min(64),
        };
        tree.nodes[parent].first_child = tree.len() as u32;
        tree.nodes[parent].child_count = count as u32;
        for offset in 0..count {
            let kind = match shape {
                Shape::Deep if parent < 999 && tree.len() + 1 < nodes => Kind::Dir,
                Shape::Balanced if offset < 8 && tree.len() + 1 < nodes => Kind::Dir,
                _ => Kind::File,
            };
            push(&mut tree, parent as u32, kind, unicode_percent);
        }
        parent += 1;
    }
    for id in (1..tree.len()).rev() {
        let n = tree.nodes[id];
        let p = &mut tree.nodes[n.parent as usize];
        p.alloc += n.alloc;
        p.logical += n.logical;
        p.files += n.files;
    }
    tree
}

fn push(tree: &mut Tree, parent: u32, kind: Kind, unicode_percent: u8) {
    let id = tree.len() as u64;
    let random = id
        .wrapping_mul(6364136223846793005)
        .wrapping_add(FIXTURE_SEED);
    let unicode = random % 100 < unicode_percent as u64;
    let suffix = if unicode { "_자료_İ_Σ" } else { "_data" };
    let name = if parent == NO_NODE {
        "X:\\fixture".to_owned()
    } else if kind == Kind::Dir {
        format!("node_{id:08}{suffix}")
    } else {
        format!(
            "node_{id:08}{suffix}.{}",
            ["txt", "bin", "mp4", "rs", "log"][id as usize % 5]
        )
    };
    let logical = if kind == Kind::File {
        random % (32 * 1024 * 1024) + 1
    } else {
        0
    };
    let name_off = tree.names.len();
    assert!(
        name_off + name.len() <= u32::MAX as usize,
        "fixture name pool exceeds u32 offsets"
    );
    tree.names.push_str(&name);
    tree.nodes.push(Node {
        alloc: logical.div_ceil(4096) * 4096,
        logical,
        name_off: name_off as u32,
        parent,
        first_child: 0,
        child_count: 0,
        files: u32::from(kind == Kind::File),
        mtime: FIXTURE_NOW_SECS2000 - (id as u32 % 365) * 86400,
        name_len: name.len() as u16,
        kind,
        flags: if parent == NO_NODE {
            crate::flags::SYNTHETIC
        } else {
            0
        },
    });
}

struct Fnv(u64);
impl Fnv {
    fn new() -> Self {
        Self(0xcbf29ce484222325)
    }
    fn bytes(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 ^= b as u64;
            self.0 = self.0.wrapping_mul(0x100000001b3);
        }
    }
}

/// FNV-1a over LE node count, name-pool length/UTF-8 bytes, then each node's
/// fields in declaration order (LE integers, kind and flags as bytes). No ABI padding.
pub fn fingerprint(tree: &Tree) -> u64 {
    let mut h = Fnv::new();
    h.bytes(&(tree.len() as u64).to_le_bytes());
    h.bytes(&(tree.names.len() as u64).to_le_bytes());
    h.bytes(tree.names.as_bytes());
    for n in &tree.nodes {
        h.bytes(&n.alloc.to_le_bytes());
        h.bytes(&n.logical.to_le_bytes());
        for v in [
            n.name_off,
            n.parent,
            n.first_child,
            n.child_count,
            n.files,
            n.mtime,
        ] {
            h.bytes(&v.to_le_bytes());
        }
        h.bytes(&n.name_len.to_le_bytes());
        h.bytes(&[n.kind as u8, n.flags]);
    }
    h.0
}

/// Canonical NodeId set, followed by total and total_size as LE u64.
pub fn result_fingerprint(result: &SearchResult) -> u64 {
    let mut ids = result.hits.clone();
    ids.sort_unstable();
    ids.dedup();
    let mut h = Fnv::new();
    h.bytes(&(ids.len() as u64).to_le_bytes());
    for id in ids {
        h.bytes(&id.to_le_bytes());
    }
    h.bytes(&(result.total as u64).to_le_bytes());
    h.bytes(&result.total_size.to_le_bytes());
    h.0
}

/// Nearest-rank percentile, with p in [0, 1]. Empty input returns zero.
pub fn percentile(samples: &[Duration], p: f64) -> Duration {
    assert!(p.is_finite() && (0.0..=1.0).contains(&p));
    if samples.is_empty() {
        return Duration::ZERO;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    sorted[((p * sorted.len() as f64).ceil() as usize).saturating_sub(1)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Kind, NO_NODE, NodeId};
    #[test]
    fn fixtures_obey_arena_and_time_contracts() {
        for shape in [Shape::Wide, Shape::Deep, Shape::Balanced] {
            for percent in [0, 25, 100] {
                let tree = make_fixture(10001, shape, percent);
                assert_eq!(tree.len(), 10001);
                assert_eq!(tree.node(0).parent, NO_NODE);
                let mut depth = vec![0; tree.len()];
                let mut end = 0;
                let mut seen = vec![false; tree.len()];
                for (id, n) in tree.nodes.iter().enumerate() {
                    assert_eq!(n.name_off as usize, end);
                    end += n.name_len as usize;
                    assert!(tree.names.is_char_boundary(end));
                    if id > 0 {
                        assert!((n.parent as usize) < id);
                        depth[id] = depth[n.parent as usize] + 1;
                        assert!(depth[id] >= depth[id - 1]);
                        if n.kind == Kind::File {
                            assert_eq!(n.mtime, FIXTURE_NOW_SECS2000 - (id as u32 % 365) * 86400);
                        }
                    }
                    for child in tree.children(id as NodeId) {
                        assert!((child as usize) < tree.len());
                        assert_eq!(tree.node(child).parent, id as NodeId);
                        assert!(!seen[child as usize]);
                        seen[child as usize] = true;
                    }
                }
                assert!(seen[1..].iter().all(|x| *x));
                assert_eq!(end, tree.names.len());
                assert!(depth.iter().max().unwrap() <= &1000);
                assert_eq!(
                    fingerprint(&tree),
                    fingerprint(&make_fixture(10001, shape, percent))
                );
                if percent == 0 {
                    assert!(tree.names.is_ascii());
                }
                if percent == 100 {
                    assert!(!tree.name(1).is_ascii());
                }
                let files = tree.nodes.iter().filter(|n| n.kind == Kind::File);
                assert_eq!(tree.node(0).logical, files.map(|n| n.logical).sum::<u64>());
            }
        }
    }
    #[test]
    fn fixture_fingerprint_has_portable_byte_encoding() {
        // Golden encoding for a ROOT-only fixture, independent of Rust ABI padding.
        assert_eq!(
            fingerprint(&make_fixture(1, Shape::Wide, 0)),
            0x8e30601af0cc7388
        );
    }

    #[test]
    fn fingerprint_detects_name_and_metadata_changes() {
        let mut tree = make_fixture(12, Shape::Balanced, 25);
        let original = fingerprint(&tree);
        tree.nodes[1].mtime += 1;
        assert_ne!(fingerprint(&tree), original);
        tree.nodes[1].mtime -= 1;
        tree.names.push('x');
        assert_ne!(fingerprint(&tree), original);
    }
    #[test]
    fn result_fingerprint_ignores_tie_order_but_covers_accounting() {
        let a = SearchResult {
            hits: vec![2, 1],
            total: 2,
            total_size: 8,
        };
        let b = SearchResult {
            hits: vec![1, 2],
            total: 2,
            total_size: 8,
        };
        assert_eq!(result_fingerprint(&a), result_fingerprint(&b));
        assert_ne!(
            result_fingerprint(&a),
            result_fingerprint(&SearchResult { total_size: 9, ..b })
        );
    }
    #[test]
    fn percentile_uses_nearest_rank_and_does_not_reorder_input() {
        let s = [
            Duration::from_millis(3),
            Duration::from_millis(1),
            Duration::from_millis(2),
        ];
        assert_eq!(percentile(&s, 0.5), Duration::from_millis(2));
        assert_eq!(percentile(&s, 0.95), Duration::from_millis(3));
        assert_eq!(percentile(&[], 0.95), Duration::ZERO);
        assert_eq!(s[0], Duration::from_millis(3));
    }
}
