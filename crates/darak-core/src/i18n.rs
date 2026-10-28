//! 화면에 보이는 문구의 한국어·영어 고르기.
//!
//! 문구는 쓰는 자리에 두 언어를 나란히 적는다: 고정 문구는 `tr("한국어", "English")`,
//! 값이 들어가면 `tr!("파일 {n}개", "{n} files")`. 언어는 프로세스 전체에 하나이고,
//! 앱이 시작할 때 `set_lang`으로 정한다. 정하지 않으면 한국어다 (CLI와 테스트).

use std::sync::atomic::{AtomicU8, Ordering::Relaxed};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    Ko = 0,
    En = 1,
}

static LANG: AtomicU8 = AtomicU8::new(Lang::Ko as u8);

/// 지금 언어
pub fn lang() -> Lang {
    if LANG.load(Relaxed) == Lang::En as u8 { Lang::En } else { Lang::Ko }
}

/// 언어를 바꾼다. 이미 만든 문구(오류, 안내)는 그대로 남는다.
/// 테스트는 부르지 않는다 (병렬로 도는 다른 테스트가 한국어 문구를 찾는다).
pub fn set_lang(lang: Lang) {
    LANG.store(lang as u8, Relaxed);
}

/// Windows 표시 언어가 한국어면 한국어, 아니면 영어
pub fn system_lang() -> Lang {
    const LANG_KOREAN: u16 = 0x12;
    let id = unsafe { crate::sys::GetUserDefaultUILanguage() };
    if id & 0x3ff == LANG_KOREAN { Lang::Ko } else { Lang::En }
}

/// 지금 언어의 고정 문구
pub fn tr(ko: &'static str, en: &'static str) -> &'static str {
    tr_in(lang(), ko, en)
}

/// 정한 언어의 고정 문구
pub fn tr_in(lang: Lang, ko: &'static str, en: &'static str) -> &'static str {
    match lang {
        Lang::Ko => ko,
        Lang::En => en,
    }
}

/// 영어 단수·복수 (한국어는 바뀌지 않는다): `let w = plural(n, "file", "files");`
pub fn plural(n: u64, one: &'static str, many: &'static str) -> &'static str {
    if n == 1 { one } else { many }
}

/// 값이 들어가는 문구. `format!`과 같지만 언어마다 형식 문자열을 하나씩 받는다.
///
/// 값은 형식 문자열 안에 이름으로 적는다 (`"{n}"`, `"{path}"`). 두 문자열이 쓰는 값이
/// 달라도 되고 순서가 달라도 된다. 위치 인자(`"{}"` 뒤 `, x`)는 두 문자열이 모두
/// 써야 컴파일되므로 이름 쓰기를 기본으로 한다. 값이 없는 문구는 `tr()`을 쓴다.
#[macro_export]
macro_rules! tr {
    ($ko:literal, $en:literal $(, $($args:tt)+)?) => {
        match $crate::i18n::lang() {
            // 한쪽 언어에만 값이 없을 수 있다 ("모두", "all {n}")
            #[allow(clippy::useless_format)]
            $crate::i18n::Lang::Ko => format!($ko $(, $($args)+)?),
            #[allow(clippy::useless_format)]
            $crate::i18n::Lang::En => format!($en $(, $($args)+)?),
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_by_language() {
        assert_eq!(tr_in(Lang::Ko, "스캔", "Scan"), "스캔");
        assert_eq!(tr_in(Lang::En, "스캔", "Scan"), "Scan");
        assert_eq!(plural(1, "file", "files"), "file");
        assert_eq!(plural(0, "file", "files"), "files");
    }

    #[test]
    fn defaults_to_korean() {
        assert_eq!(lang(), Lang::Ko);
        let n = 3;
        assert_eq!(tr!("파일 {n}개", "{n} files"), "파일 3개");
    }
}
