//! darak 핵심 로직: 스캔, 트리, 레이아웃. UI에 의존하지 않는다.

pub mod category;
pub mod layout;
pub mod mft;
pub mod remove;
pub mod scan;
pub mod search;
pub mod sys;
pub mod tree;

pub use scan::{ScanMethod, ScanOptions, ScanProgress, ScanResult, ScanStats, scan};
pub use tree::{Kind, NO_NODE, Node, NodeId, SizeMode, Tree, flags};

/// 사람이 읽기 쉬운 크기 (1024 단위)
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if v >= 100.0 {
        format!("{v:.0} {}", UNITS[u])
    } else if v >= 10.0 {
        format!("{v:.1} {}", UNITS[u])
    } else {
        format!("{v:.2} {}", UNITS[u])
    }
}

/// 2000년 기준 초 → 로컬 시각 `2026-10-03 16:42`
pub fn format_time(secs2000: u32) -> String {
    if secs2000 == 0 {
        return "-".into();
    }
    let ft: i64 = (secs2000 as i64 + 12_591_158_400) * 10_000_000;
    let mut utc = sys::SYSTEMTIME::default();
    let mut local = sys::SYSTEMTIME::default();
    unsafe {
        if sys::FileTimeToSystemTime(&ft, &mut utc) == 0
            || sys::SystemTimeToTzSpecificLocalTime(std::ptr::null(), &utc, &mut local) == 0
        {
            return "-".into();
        }
    }
    format!("{:04}-{:02}-{:02} {:02}:{:02}", local.wYear, local.wMonth, local.wDay, local.wHour, local.wMinute)
}

/// 천 단위 쉼표
pub fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1536), "1.50 KB");
        assert_eq!(human_size(250 * 1024 * 1024 * 1024), "250 GB");
        assert_eq!(thousands(1234567), "1,234,567");
        assert_eq!(thousands(12), "12");
    }

    #[test]
    fn normalize() {
        assert_eq!(scan::normalize_root("c:"), "C:\\");
        assert_eq!(scan::normalize_root("d:/games/"), "D:\\games");
    }
}
