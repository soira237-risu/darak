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
    Wildcard(Vec<u8>),
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
            let lower = tok.to_lowercase();
            let t = if let Some(v) = lower.strip_prefix("ext:") {
                Term::Ext(v.split([';', ',']).filter(|e| !e.is_empty()).map(|e| e.trim_start_matches('.').to_string()).collect())
            } else if let Some(v) = lower.strip_prefix("size:") {
                if let Some(n) = v.strip_prefix('>') {
                    Term::SizeGt(parse_size(n).ok_or(format!("크기를 알 수 없음: {n}"))?)
                } else if let Some(n) = v.strip_prefix('<') {
                    Term::SizeLt(parse_size(n).ok_or(format!("크기를 알 수 없음: {n}"))?)
                } else {
                    Term::SizeGt(parse_size(v).ok_or(format!("크기를 알 수 없음: {v}"))?)
                }
            } else if let Some(v) = lower.strip_prefix("dm:") {
                let (newer, n) = if let Some(n) = v.strip_prefix('<') {
                    (true, n)
                } else if let Some(n) = v.strip_prefix('>') {
                    (false, n)
                } else {
                    (true, v)
                };
                let secs = parse_age(n).ok_or(format!("기간을 알 수 없음: {n} (예: 7d, 2w, 3m, 1y)"))?;
                let cutoff = now_secs2000.saturating_sub(secs);
                if newer { Term::NewerThan(cutoff) } else { Term::OlderThan(cutoff) }
            } else if let Some(v) = lower.strip_prefix("path:") {
                Term::Path(v.to_string())
            } else if lower == "file:" {
                Term::FilesOnly
            } else if lower == "folder:" || lower == "dir:" {
                Term::FoldersOnly
            } else if let Some(v) = lower.strip_prefix('!') {
                if v.is_empty() {
                    continue;
                }
                Term::Contains(v.to_string(), true)
            } else if lower.contains(['*', '?']) {
                Term::Wildcard(lower.into_bytes())
            } else {
                Term::Contains(lower, false)
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
    let split = s.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(s.len());
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

/// `*`, `?` 와일드카드 (바이트 단위, ASCII 소문자로 비교)
fn wildcard(pat: &[u8], s: &[u8]) -> bool {
    let (mut p, mut i) = (0, 0);
    let (mut star, mut mark) = (usize::MAX, 0);
    while i < s.len() {
        if p < pat.len() && (pat[p] == b'?' || pat[p] == s[i].to_ascii_lowercase()) {
            p += 1;
            i += 1;
        } else if p < pat.len() && pat[p] == b'*' {
            star = p;
            mark = i;
            p += 1;
        } else if star != usize::MAX {
            p = star + 1;
            mark += 1;
            i = mark;
        } else {
            return false;
        }
    }
    while p < pat.len() && pat[p] == b'*' {
        p += 1;
    }
    p == pat.len()
}

fn ascii_lower_into<'a>(s: &str, buf: &'a mut Vec<u8>) -> &'a [u8] {
    buf.clear();
    buf.extend(s.bytes().map(|b| b.to_ascii_lowercase()));
    buf
}

pub struct SearchResult {
    /// 크기 내림차순, 최대 `limit`개
    pub hits: Vec<NodeId>,
    /// 조건에 맞는 전체 개수
    pub total: usize,
    pub total_size: u64,
}

/// `scope` 아래(포함)에서 검색한다. 비-ASCII 문자는 대소문자를 그대로 비교한다.
pub fn search(tree: &Tree, q: &Query, mode: SizeMode, scope: NodeId, limit: usize) -> SearchResult {
    let finders: Vec<(memmem::Finder<'_>, bool)> = q
        .terms
        .iter()
        .filter_map(|t| match t {
            Term::Contains(s, neg) => Some((memmem::Finder::new(s.as_bytes()), *neg)),
            _ => None,
        })
        .collect();

    let matches = |id: NodeId, buf: &mut Vec<u8>| -> bool {
        let n = tree.node(id);
        if n.has(flags::REMOVED) || id == Tree::ROOT {
            return false;
        }
        let name = ascii_lower_into(tree.name(id), buf);
        for (f, neg) in &finders {
            if f.find(name).is_some() == *neg {
                return false;
            }
        }
        for t in &q.terms {
            let ok = match t {
                Term::Contains(..) => true,
                Term::Wildcard(p) => wildcard(p, name),
                Term::Ext(exts) => match name.iter().rposition(|&c| c == b'.') {
                    Some(dot) if n.kind == Kind::File => exts.iter().any(|e| e.as_bytes() == &name[dot + 1..]),
                    _ => false,
                },
                Term::SizeGt(v) => n.size(mode) > *v,
                Term::SizeLt(v) => n.size(mode) < *v,
                Term::NewerThan(c) => n.kind == Kind::File && n.mtime >= *c,
                Term::OlderThan(c) => n.kind == Kind::File && n.mtime > 0 && n.mtime < *c,
                Term::FilesOnly => n.kind == Kind::File,
                Term::FoldersOnly => n.kind == Kind::Dir,
                Term::Path(_) => true, // 비싼 조건은 마지막에
            };
            if !ok {
                return false;
            }
        }
        for t in &q.terms {
            if let Term::Path(p) = t
                && !tree.path(id).to_lowercase().contains(p.as_str()) {
                    return false;
                }
        }
        if scope != Tree::ROOT && !tree.is_ancestor(scope, id) {
            return false;
        }
        true
    };

    let mut hits: Vec<NodeId> = (0..tree.len() as u32)
        .into_par_iter()
        .with_min_len(4096)
        .map_init(Vec::new, |buf, id| matches(id, buf).then_some(id))
        .flatten_iter()
        .collect();
    let total = hits.len();
    // 하위 항목이 함께 걸린 경우 크기를 두 번 세지 않도록, 결과 중 최상위 항목만 합산
    let total_size = {
        let set: std::collections::HashSet<NodeId> = hits.iter().copied().collect();
        hits.par_iter()
            .filter(|&&id| {
                let mut cur = tree.parent(id);
                while let Some(p) = cur {
                    if set.contains(&p) {
                        return false;
                    }
                    cur = tree.parent(p);
                }
                true
            })
            .map(|&id| tree.node(id).size(mode))
            .sum()
    };
    hits.par_sort_unstable_by_key(|&id| std::cmp::Reverse(tree.node(id).size(mode)));
    hits.truncate(limit);
    SearchResult { hits, total, total_size }
}

/// 현재 시각 (2000년 기준 초)
pub fn now_secs2000() -> u32 {
    let unix = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    unix.saturating_sub(946_684_800) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcards() {
        assert!(wildcard(b"*.mp4", b"Movie.MP4"));
        assert!(wildcard(b"a?c*", b"abcdef"));
        assert!(!wildcard(b"*.mp4", b"movie.mkv"));
        assert!(wildcard(b"*", b""));
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
}
