//! Everything식 즉시 검색. 메모리의 이름 전체를 rayon으로 병렬 탐색한다.
//!
//! 문법 (공백으로 구분, 모두 AND):
//! - `abc`        이름에 abc 포함 (대소문자 무시)
//! - `*.mp4`      와일드카드 (`*`, `?`) — 이름 전체와 비교
//! - `ext:mp4;mkv` 확장자
//! - `size:>1gb`  `size:<10mb`  크기 (현재 크기 기준)
//! - `dm:<7d`     최근 7일 안에 수정, `dm:>1y` 1년보다 오래됨 (d/w/m/y)
//! - `path:abc`   전체 경로에 abc 포함
//! - `file:` / `folder:`  종류
//! - `!abc`       이름에 abc가 없는 것

use memchr::memmem;
use rayon::prelude::*;

use crate::tree::{Kind, NodeId, SizeMode, Tree, flags};

#[derive(Debug, Clone)]
enum Term {
    Contains(String, bool),
    Wildcard(String),
    Ext(Vec<String>),
    SizeGt(u64),
    SizeLt(u64),
    NewerThan(u32),
    OlderThan(u32),
    Path(String),
    FilesOnly,
    FoldersOnly,
}

#[derive(Debug, Clone, Default)]
pub struct Query {
    terms: Vec<Term>,
}

impl Query {
    pub fn parse(s: &str, now_secs2000: u32) -> Result<Query, String> {
        let mut terms = Vec::new();
        for tok in s.split_whitespace() {
            // 문법은 ASCII로 인식하고, Unicode lowercase는 분리한 피연산자에 적용한다.
            let syntax = tok.to_ascii_lowercase();
            let t = if let Some(v) = syntax.strip_prefix("ext:") {
                Term::Ext(
                    v.split([';', ','])
                        .filter(|e| !e.is_empty())
                        .map(|e| e.trim_start_matches('.').to_lowercase())
                        .collect(),
                )
            } else if let Some(v) = syntax.strip_prefix("size:") {
                if let Some(n) = v.strip_prefix('>') {
                    Term::SizeGt(parse_size(n).ok_or(format!("크기를 알 수 없음: {n}"))?)
                } else if let Some(n) = v.strip_prefix('<') {
                    Term::SizeLt(parse_size(n).ok_or(format!("크기를 알 수 없음: {n}"))?)
                } else {
                    Term::SizeGt(parse_size(v).ok_or(format!("크기를 알 수 없음: {v}"))?)
                }
            } else if let Some(v) = syntax.strip_prefix("dm:") {
                let (newer, n) = if let Some(n) = v.strip_prefix('<') {
                    (true, n)
                } else if let Some(n) = v.strip_prefix('>') {
                    (false, n)
                } else {
                    (true, v)
                };
                let secs =
                    parse_age(n).ok_or(format!("기간을 알 수 없음: {n} (예: 7d, 2w, 3m, 1y)"))?;
                let cutoff = now_secs2000.saturating_sub(secs);
                if newer {
                    Term::NewerThan(cutoff)
                } else {
                    Term::OlderThan(cutoff)
                }
            } else if let Some(v) = syntax.strip_prefix("path:") {
                Term::Path(v.to_lowercase())
            } else if syntax == "file:" {
                Term::FilesOnly
            } else if syntax == "folder:" || syntax == "dir:" {
                Term::FoldersOnly
            } else if let Some(v) = syntax.strip_prefix('!') {
                if v.is_empty() {
                    continue;
                }
                Term::Contains(v.to_lowercase(), true)
            } else if syntax.contains(['*', '?']) {
                Term::Wildcard(tok.to_string())
            } else {
                Term::Contains(tok.to_lowercase(), false)
            };
            terms.push(t);
        }
        Ok(Query { terms })
    }

    pub fn is_empty(&self) -> bool {
        self.terms.is_empty()
    }
}

fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim();
    let split = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let v: f64 = num.parse().ok()?;
    let mul = match unit {
        "" | "b" => 1.0,
        "k" | "kb" => 1024.0,
        "m" | "mb" => 1024.0 * 1024.0,
        "g" | "gb" => 1024.0 * 1024.0 * 1024.0,
        "t" | "tb" => 1024.0f64.powi(4),
        _ => return None,
    };
    Some((v * mul) as u64)
}

fn parse_age(s: &str) -> Option<u32> {
    let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let v: u32 = num.parse().ok()?;
    let day = 86_400u32;
    let mul = match unit {
        "d" | "" => day,
        "w" => day * 7,
        "m" => day * 30,
        "y" => day * 365,
        "h" => 3600,
        _ => return None,
    };
    Some(v.saturating_mul(mul))
}

/// `?`는 원본 Unicode scalar 하나, `*`는 0개 이상을 소비한다.
/// 리터럴은 scalar별 lowercase 결과를 비교하며 정규화하지 않는다.
#[cfg(test)]
fn wildcard(pat: &str, name: &str) -> bool {
    wildcard_with_cancel(pat, name, &|| false).unwrap()
}

fn checkpoint(
    steps: &mut usize,
    cancelled: &(dyn Fn() -> bool + Sync),
) -> Result<(), SearchCancelled> {
    if steps.is_multiple_of(256) && cancelled() {
        return Err(SearchCancelled);
    }
    *steps += 1;
    Ok(())
}

fn wildcard_with_cancel(
    pat: &str,
    name: &str,
    cancelled: &(dyn Fn() -> bool + Sync),
) -> Result<bool, SearchCancelled> {
    if pat.is_ascii() && name.is_ascii() {
        return wildcard_ascii(pat.as_bytes(), name.as_bytes(), cancelled);
    }
    let (mut p, mut i) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    let mut steps = 0;
    while i < name.len() {
        checkpoint(&mut steps, cancelled)?;
        let current = name[i..].chars().next().unwrap();
        let literal = pat[p..].chars().next();
        if literal == Some('*') {
            p += 1;
            star = Some(p);
            mark = i;
        } else if let Some(c) = literal
            && (c == '?' || c.to_lowercase().eq(current.to_lowercase()))
        {
            p += c.len_utf8();
            i += current.len_utf8();
        } else if let Some(after_star) = star {
            mark += name[mark..].chars().next().unwrap().len_utf8();
            i = mark;
            p = after_star;
        } else {
            return Ok(false);
        }
    }
    for c in pat[p..].chars() {
        checkpoint(&mut steps, cancelled)?;
        if c != '*' {
            return Ok(false);
        }
    }
    Ok(true)
}

/// ASCII 경로는 할당 없이 원본 패턴과 이름을 대소문자 무시로 비교한다.
fn wildcard_ascii(
    pat: &[u8],
    s: &[u8],
    cancelled: &(dyn Fn() -> bool + Sync),
) -> Result<bool, SearchCancelled> {
    let (mut p, mut i) = (0, 0);
    let (mut star, mut mark) = (usize::MAX, 0);
    let mut steps = 0;
    while i < s.len() {
        checkpoint(&mut steps, cancelled)?;
        if p < pat.len() && pat[p] == b'*' {
            star = p;
            mark = i;
            p += 1;
        } else if p < pat.len() && (pat[p] == b'?' || pat[p].eq_ignore_ascii_case(&s[i])) {
            p += 1;
            i += 1;
        } else if star != usize::MAX {
            p = star + 1;
            mark += 1;
            i = mark;
        } else {
            return Ok(false);
        }
    }
    while p < pat.len() && pat[p] == b'*' {
        checkpoint(&mut steps, cancelled)?;
        p += 1;
    }
    Ok(p == pat.len())
}

fn lower_into<'a>(s: &str, buf: &'a mut Vec<u8>) -> &'a [u8] {
    buf.clear();
    if s.is_ascii() {
        buf.extend(s.bytes().map(|b| b.to_ascii_lowercase()));
    } else {
        // 쿼리와 동일한 문자열 lowercase 규칙 (확장 및 문맥 의존 변환 포함).
        buf.extend_from_slice(s.to_lowercase().as_bytes());
    }
    buf
}

// Reused within one 4096-node matching run, retaining only its longest path.
#[derive(Default)]
struct MatchScratch {
    lower: Vec<u8>,
    chain: Vec<NodeId>,
    path: String,
}

pub struct SearchResult {
    /// 크기 내림차순, 최대 `limit`개
    pub hits: Vec<NodeId>,
    /// 조건에 맞는 전체 개수
    pub total: usize,
    pub total_size: u64,
}

/// A cancelled search never returns partial results.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchCancelled;

/// `scope` 아래(포함)에서 검색한다. 이름·확장자·경로는 Unicode lowercase로 비교한다.
pub fn search(tree: &Tree, q: &Query, mode: SizeMode, scope: NodeId, limit: usize) -> SearchResult {
    search_with_cancel(tree, q, mode, scope, limit, &|| false).unwrap()
}

pub fn search_with_cancel(
    tree: &Tree,
    q: &Query,
    mode: SizeMode,
    scope: NodeId,
    limit: usize,
    cancelled: &(dyn Fn() -> bool + Sync),
) -> Result<SearchResult, SearchCancelled> {
    if cancelled() {
        return Err(SearchCancelled);
    }
    let finders: Vec<(memmem::Finder<'_>, bool)> = q
        .terms
        .iter()
        .filter_map(|t| match t {
            Term::Contains(s, neg) => Some((memmem::Finder::new(s.as_bytes()), *neg)),
            _ => None,
        })
        .collect();
    let path_terms: Vec<&str> = q
        .terms
        .iter()
        .filter_map(|t| match t {
            Term::Path(p) => Some(p.as_str()),
            _ => None,
        })
        .collect();
    let matches = |id: NodeId, scratch: &mut MatchScratch| -> Result<bool, SearchCancelled> {
        let MatchScratch { lower, chain, path } = scratch;
        let n = tree.node(id);
        if n.has(flags::REMOVED) || id == Tree::ROOT {
            return Ok(false);
        }
        // Reject using metadata before looking up or folding names.
        for t in &q.terms {
            let ok = match t {
                Term::SizeGt(v) => n.size(mode) > *v,
                Term::SizeLt(v) => n.size(mode) < *v,
                Term::NewerThan(c) => n.kind == Kind::File && n.mtime >= *c,
                Term::OlderThan(c) => n.kind == Kind::File && n.mtime > 0 && n.mtime < *c,
                Term::FilesOnly => n.kind == Kind::File,
                Term::FoldersOnly => n.kind == Kind::Dir,
                Term::Ext(_) => n.kind == Kind::File,
                _ => true,
            };
            if !ok {
                return Ok(false);
            }
        }
        if !finders.is_empty() {
            let name = lower_into(tree.name(id), lower);
            for (f, neg) in &finders {
                if f.find(name).is_some() == *neg {
                    return Ok(false);
                }
            }
        }
        for t in &q.terms {
            let ok = match t {
                Term::Wildcard(p) => wildcard_with_cancel(p, tree.name(id), cancelled)?,
                Term::Ext(exts) => match tree.name(id).rsplit_once('.') {
                    Some((_, ext)) => {
                        if ext.is_ascii() {
                            exts.iter().any(|e| ext.eq_ignore_ascii_case(e))
                        } else {
                            // Lowercase the complete extension operand, retaining contextual Sigma.
                            exts.contains(&ext.to_lowercase())
                        }
                    }
                    _ => false,
                },
                _ => true,
            };
            if !ok {
                return Ok(false);
            }
        }
        if !path_terms.is_empty() {
            // Reuse storage, but fold the complete path once to preserve contextual lowercase.
            chain.clear();
            chain.push(id);
            let mut cur = id;
            let mut steps = 0;
            while let Some(parent) = tree.parent(cur) {
                checkpoint(&mut steps, cancelled)?;
                chain.push(parent);
                cur = parent;
            }
            path.clear();
            path.push_str(tree.name(*chain.last().unwrap()));
            for &part in chain.iter().rev().skip(1) {
                checkpoint(&mut steps, cancelled)?;
                if !path.ends_with('\\') {
                    path.push('\\');
                }
                path.push_str(tree.name(part));
            }
            let folded = lower_into(path, lower);
            if path_terms
                .iter()
                .any(|term| memmem::find(folded, term.as_bytes()).is_none())
            {
                return Ok(false);
            }
        }
        Ok(true)
    };
    // Root uses the arena directly; a scope visits only its inclusive subtree.
    let runs: Result<Vec<Vec<NodeId>>, SearchCancelled> = if scope == Tree::ROOT {
        (0..tree.len().div_ceil(4096))
            .into_par_iter()
            .map(|run| {
                let start = run * 4096;
                collect_matches(
                    (start as NodeId)..((start + 4096).min(tree.len()) as NodeId),
                    &matches,
                    cancelled,
                )
            })
            .collect()
    } else {
        let mut candidates = if (scope as usize) < tree.len() {
            vec![scope]
        } else {
            Vec::new()
        };
        let mut cursor = 0;
        let mut steps = 0;
        while cursor < candidates.len() {
            checkpoint(&mut steps, cancelled)?;
            for child in tree.children(candidates[cursor]) {
                checkpoint(&mut steps, cancelled)?;
                candidates.push(child);
            }
            cursor += 1;
        }
        candidates
            .par_chunks(4096)
            .map(|ids| collect_matches(ids.iter().copied(), &matches, cancelled))
            .collect()
    };
    // Rayon has released all tree borrows before any cancellation is returned.
    let runs = runs?;
    let total = runs.iter().map(Vec::len).sum();
    let mut hits = Vec::with_capacity(total);
    let mut membership = vec![0u64; tree.len().div_ceil(64)];
    let mut steps = 0;
    for run in runs {
        for id in run {
            checkpoint(&mut steps, cancelled)?;
            membership[id as usize / 64] |= 1 << (id % 64);
            hits.push(id);
        }
    }
    // Count only matches without a matching ancestor, before applying the limit.
    let totals: Result<Vec<u64>, SearchCancelled> = hits
        .par_chunks(4096)
        .map(|ids| {
            let mut sum = 0;
            let mut steps = 0;
            for &id in ids {
                checkpoint(&mut steps, cancelled)?;
                let mut cur = tree.parent(id);
                let mut covered = false;
                while let Some(parent) = cur {
                    checkpoint(&mut steps, cancelled)?;
                    if membership[parent as usize / 64] & (1 << (parent % 64)) != 0 {
                        covered = true;
                        break;
                    }
                    cur = tree.parent(parent);
                }
                if !covered {
                    sum += tree.node(id).size(mode);
                }
            }
            Ok(sum)
        })
        .collect();
    let total_size = totals?.into_iter().sum();
    sort_hits_with_cancel(tree, &mut hits, mode, cancelled)?;
    hits.truncate(limit);
    if cancelled() {
        return Err(SearchCancelled);
    }
    Ok(SearchResult {
        hits,
        total,
        total_size,
    })
}

fn collect_matches(
    ids: impl Iterator<Item = NodeId>,
    matches: &impl Fn(NodeId, &mut MatchScratch) -> Result<bool, SearchCancelled>,
    cancelled: &(dyn Fn() -> bool + Sync),
) -> Result<Vec<NodeId>, SearchCancelled> {
    let mut hits = Vec::new();
    let mut scratch = MatchScratch::default();
    let mut steps = 0;
    for id in ids {
        checkpoint(&mut steps, cancelled)?;
        if matches(id, &mut scratch)? {
            hits.push(id);
        }
    }
    Ok(hits)
}

fn sort_hits_with_cancel(
    tree: &Tree,
    hits: &mut Vec<NodeId>,
    mode: SizeMode,
    cancelled: &(dyn Fn() -> bool + Sync),
) -> Result<(), SearchCancelled> {
    // (크기 내림차순, ID) key는 모두 달라서 결과 순서가 하나로 정해진다.
    crate::sort::sort_by_key(
        hits,
        |&id| (std::cmp::Reverse(tree.node(id).size(mode)), id),
        crate::sort::Exec::Parallel,
        cancelled,
    )
    .map_err(|_| SearchCancelled)
}

/// 현재 시각 (2000년 기준 초)
pub fn now_secs2000() -> u32 {
    let unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    unix.saturating_sub(946_684_800) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcards() {
        assert!(wildcard("*.mp4", "Movie.MP4"));
        assert!(wildcard("a?c*", "abcdef"));
        assert!(!wildcard("*.mp4", "movie.mkv"));
        assert!(wildcard("*", ""));
    }

    #[test]
    fn wildcards_consume_original_scalars() {
        for (pattern, name, expected) in [
            ("A*.TXT", "abc.txt", true),
            ("?.txt", "가.txt", true),
            ("??.txt", "가.txt", false),
            ("?.txt", "😀.txt", true),
            ("?.txt", "e\u{301}.txt", false),
            ("??.txt", "e\u{301}.txt", true),
            ("*가?.txt", "앞가나.txt", true),
            ("?.txt", "İ.txt", true),
            ("İ*.txt", "İ.txt", true),
            ("i\u{307}*.txt", "İ.txt", false),
        ] {
            assert_eq!(wildcard(pattern, name), expected, "{pattern:?} / {name:?}");
        }
    }

    #[test]
    fn wildcard_stars_are_operators() {
        assert!(wildcard("*?", "*"));
        assert!(wildcard("*?", "*가"));
    }

    #[test]
    fn sizes_and_ages() {
        assert_eq!(parse_size("1gb"), Some(1 << 30));
        assert_eq!(parse_size("1.5k"), Some(1536));
        assert_eq!(parse_size("10"), Some(10));
        assert_eq!(parse_size("3x"), None);
        assert_eq!(parse_age("2w"), Some(14 * 86_400));
        assert!(Query::parse("size:>1gb ext:mp4;mkv dm:<30d foo !bar *.x", 1_000_000_000).is_ok());
        assert!(Query::parse("size:>huge", 0).is_err());
    }
    fn fixture(rows: &[(&str, Kind, NodeId, u64, u64, u8)]) -> Tree {
        use crate::tree::{NO_NODE, Node};
        let mut tree = Tree::default();
        for &(name, kind, parent, alloc, logical, flags) in rows {
            tree.nodes.push(Node {
                alloc,
                logical,
                name_off: tree.names.len() as u32,
                name_len: name.len() as u16,
                parent,
                first_child: 0,
                child_count: 0,
                files: 1,
                mtime: 1,
                kind,
                flags,
            });
            tree.names.push_str(name);
        }
        for id in 1..tree.len() {
            let parent = tree.nodes[id].parent;
            if parent != NO_NODE {
                let n = &mut tree.nodes[parent as usize];
                if n.child_count == 0 {
                    n.first_child = id as NodeId;
                }
                n.child_count += 1;
            }
        }
        tree
    }

    #[test]
    fn scoped_search_preserves_all_result_semantics() {
        use crate::tree::NO_NODE;
        let tree = fixture(&[
            // ROOT also matches the query, making its exclusion observable.
            ("C:\\match\\", Kind::Dir, NO_NODE, 90, 60, 0),
            ("match", Kind::Dir, 0, 40, 30, 0),
            ("match-outside", Kind::File, 0, 50, 30, 0),
            ("match-a", Kind::File, 1, 20, 10, 0),
            ("match-b", Kind::File, 1, 20, 10, 0),
            ("match-removed", Kind::File, 1, 0, 0, flags::REMOVED),
        ]);
        let q = Query::parse("match", 0).unwrap();
        for mode in [SizeMode::Allocated, SizeMode::Logical] {
            for scope in [Tree::ROOT, 1] {
                let mut expected = if scope == Tree::ROOT {
                    vec![1, 2, 3, 4]
                } else {
                    vec![1, 3, 4]
                };
                expected
                    .sort_unstable_by_key(|&id| (std::cmp::Reverse(tree.node(id).size(mode)), id));
                for limit in [0, 1, usize::MAX] {
                    let result = search(&tree, &q, mode, scope, limit);
                    assert_eq!(result.total, expected.len());
                    assert_eq!(
                        result.total_size,
                        tree.node(1).size(mode)
                            + if scope == Tree::ROOT {
                                tree.node(2).size(mode)
                            } else {
                                0
                            }
                    );
                    assert_eq!(result.hits, expected[..limit.min(expected.len())]);
                }
            }
        }
    }

    #[test]
    fn path_terms_share_full_path_semantics() {
        use crate::tree::NO_NODE;
        let tree = fixture(&[
            ("C:\\", Kind::Dir, NO_NODE, 6, 6, 0),
            ("AΣ", Kind::Dir, 0, 6, 6, 0),
            ("x.Σ", Kind::File, 1, 3, 3, 0),
            ("skip.Σ", Kind::File, 1, 2, 2, 0),
            ("x.txt", Kind::File, 1, 1, 1, 0),
        ]);
        for (query, expected) in [
            ("path:AΣ path:Σ", vec![]),
            ("path:AΣ path:x !skip ext:Σ ?.Σ", vec![2]),
            ("path:AΣ path:x", vec![2, 4]),
        ] {
            let q = Query::parse(query, 0).unwrap();
            let actual = search(&tree, &q, SizeMode::Logical, Tree::ROOT, usize::MAX);
            assert_eq!(actual.hits, expected, "{query}");
        }
    }

    #[test]
    fn cancelled_search_never_returns_partial_results() {
        use crate::tree::NO_NODE;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let mut rows = vec![("C:\\", Kind::Dir, NO_NODE, 20000, 20000, 0)];
        rows.extend((0..20000).map(|_| ("match.txt", Kind::File, 0, 1, 1, 0)));
        let tree = fixture(&rows);
        let q = Query::parse("match", 0).unwrap();
        for after in [0, 2, 10, 80, 160] {
            let calls = AtomicUsize::new(0);
            let result = search_with_cancel(&tree, &q, SizeMode::Logical, Tree::ROOT, 1, &|| {
                calls.fetch_add(1, Ordering::Relaxed) >= after
            });
            assert!(matches!(result, Err(SearchCancelled)), "after {after}");
        }
        let complete = search_with_cancel(
            &tree,
            &q,
            SizeMode::Logical,
            Tree::ROOT,
            usize::MAX,
            &|| false,
        )
        .unwrap();
        assert_eq!(complete.hits, (1..=20000).collect::<Vec<NodeId>>());
        assert_eq!(complete.total, 20000);
        assert_eq!(complete.total_size, 20000);
    }

    #[test]
    fn cancellation_reaches_deep_paths_and_wildcard_retries() {
        use crate::tree::NO_NODE;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let mut rows = vec![("C:\\", Kind::Dir, NO_NODE, 1, 1, 0)];
        rows.extend((1..=1024).map(|id| ("x", Kind::Dir, id - 1, 1, 1, 0)));
        let mut tree = fixture(&rows);
        let q = Query::parse("path:C:", 0).unwrap();
        let calls = AtomicUsize::new(0);
        assert!(matches!(
            search_with_cancel(&tree, &q, SizeMode::Logical, 1024, usize::MAX, &|| calls
                .fetch_add(1, Ordering::Relaxed)
                >= 4),
            Err(SearchCancelled)
        ));
        tree.nodes[1024].kind = Kind::File;
        let q = Query::parse("file:", 0).unwrap();
        let calls = AtomicUsize::new(0);
        assert!(matches!(
            search_with_cancel(&tree, &q, SizeMode::Logical, 1024, usize::MAX, &|| calls
                .fetch_add(1, Ordering::Relaxed)
                >= 5),
            Err(SearchCancelled)
        ));
        for name in ["a".repeat(8192), "가".repeat(8192)] {
            let calls = AtomicUsize::new(0);
            assert!(matches!(
                wildcard_with_cancel("*aab", &name, &|| calls.fetch_add(1, Ordering::Relaxed)
                    >= 2),
                Err(SearchCancelled)
            ));
        }
    }

    #[test]
    fn cancelled_merge_stops_mid_run() {
        use crate::tree::NO_NODE;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let mut rows = vec![("C:\\", Kind::Dir, NO_NODE, 0, 0, 0)];
        rows.extend((1..=10000).map(|id| ("x", Kind::File, 0, 1, id % 101, 0)));
        let tree = fixture(&rows);
        let mut hits: Vec<NodeId> = (1..=10000).rev().collect();
        let calls = AtomicUsize::new(0);
        // Initial check and both checks for each of three sorted runs precede merging.
        assert!(matches!(
            sort_hits_with_cancel(&tree, &mut hits, SizeMode::Logical, &|| calls
                .fetch_add(1, Ordering::Relaxed)
                >= 10),
            Err(SearchCancelled)
        ));
        sort_hits_with_cancel(&tree, &mut hits, SizeMode::Logical, &|| false).unwrap();
        let mut expected: Vec<NodeId> = (1..=10000).collect();
        expected.sort_unstable_by_key(|&id| (std::cmp::Reverse(tree.node(id).logical), id));
        assert_eq!(hits, expected);
    }
    #[test]
    fn reused_path_buffers_preserve_long_unicode_operands() {
        use crate::tree::NO_NODE;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let names = ["AΣİ😀e\u{301}".repeat(64), "leafΣ".repeat(48)];
        let mut rows = vec![("C:\\", Kind::Dir, NO_NODE, 1, 1, 0)];
        rows.extend(
            (1..=300).map(|id| (names[id as usize % 2].as_str(), Kind::Dir, id - 1, 1, 1, 0)),
        );
        let tree = fixture(&rows);
        for text in ["path:AΣİ😀e\u{301} path:leafΣ", "path:Σ", "path:C:"] {
            let query = Query::parse(text, 0).unwrap();
            let expected: Vec<NodeId> = (1..=300)
                .filter(|&id| {
                    let path = tree.path(id).to_lowercase();
                    query.terms.iter().all(|term| match term {
                        Term::Path(needle) => path.contains(needle),
                        _ => unreachable!(),
                    })
                })
                .collect();
            let result = search(&tree, &query, SizeMode::Logical, Tree::ROOT, usize::MAX);
            assert_eq!(result.hits, expected, "{text}");
        }
        let query = Query::parse("path:C:", 0).unwrap();
        let calls = AtomicUsize::new(0);
        assert!(matches!(
            search_with_cancel(&tree, &query, SizeMode::Logical, 300, usize::MAX, &|| {
                calls.fetch_add(1, Ordering::Relaxed) >= 3
            }),
            Err(SearchCancelled)
        ));
    }

    #[test]
    fn parallel_merge_partitions_cover_ties_and_short_runs() {
        use crate::tree::NO_NODE;
        let mut rows = vec![("C:\\", Kind::Dir, NO_NODE, 0, 0, 0)];
        rows.extend((1..=65539).map(|id| ("x", Kind::File, 0, 1, id % 17, 0)));
        let tree = fixture(&rows);
        for threads in [1, 4] {
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .build()
                .unwrap();
            for count in [4095, 4096, 4097, 8191, 8192, 8193, 16385, 65539] {
                for mode in [SizeMode::Allocated, SizeMode::Logical] {
                    let mut hits: Vec<NodeId> = (1..=count).rev().collect();
                    let mut expected = hits.clone();
                    expected.sort_unstable_by_key(|&id| {
                        (std::cmp::Reverse(tree.node(id).size(mode)), id)
                    });
                    pool.install(|| sort_hits_with_cancel(&tree, &mut hits, mode, &|| false))
                        .unwrap();
                    assert_eq!(hits, expected, "{threads} threads, {count} hits, {mode:?}");
                }
            }
        }
    }

    #[test]
    fn large_scope_includes_its_matching_ancestor_before_limit() {
        use crate::tree::NO_NODE;
        let mut rows = vec![
            ("C:\\", Kind::Dir, NO_NODE, 20001, 40002, 0),
            ("match", Kind::Dir, 0, 20000, 40000, 0),
            ("match-outside", Kind::File, 0, 1, 2, 0),
        ];
        rows.extend((0..20000).map(|_| ("match", Kind::File, 1, 1, 2, 0)));
        let tree = fixture(&rows);
        let query = Query::parse("match", 0).unwrap();
        for mode in [SizeMode::Allocated, SizeMode::Logical] {
            for limit in [0, 1, usize::MAX] {
                let result = search(&tree, &query, mode, 1, limit);
                let expected: Vec<_> = std::iter::once(1).chain(3..20003).take(limit).collect();
                assert_eq!(result.hits, expected);
                assert_eq!(result.total, 20001);
                assert_eq!(result.total_size, tree.node(1).size(mode));
            }
        }
    }
}
