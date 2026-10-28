//! 사용자가 고른 화면 언어. Darak이 디스크에 남기는 설정은 이것 하나다.
//!
//! `%APPDATA%\Darak\settings.toml`에 `language = "auto" | "ko" | "en"` 한 줄로 저장한다.
//! 파일은 언어를 바꿀 때만 쓰고, 없거나 읽지 못하면 Windows 표시 언어를 따른다.

use std::path::PathBuf;
use std::{fs, io};

use darak_core::i18n::{self, Lang, tr};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LangPref {
    /// Windows 표시 언어를 따른다 (한국어가 아니면 영어)
    Auto,
    Ko,
    En,
}

impl LangPref {
    pub const ALL: [LangPref; 3] = [LangPref::Auto, LangPref::Ko, LangPref::En];

    pub fn resolve(self) -> Lang {
        match self {
            LangPref::Auto => i18n::system_lang(),
            LangPref::Ko => Lang::Ko,
            LangPref::En => Lang::En,
        }
    }

    /// 고르는 목록의 이름. 언어 이름은 그 언어로 적는다 (지금 언어를 읽지 못해도 찾게).
    pub fn label(self) -> &'static str {
        match self {
            LangPref::Auto => tr("Windows 언어", "Windows language"),
            LangPref::Ko => "한국어",
            LangPref::En => "English",
        }
    }

    fn key(self) -> &'static str {
        match self {
            LangPref::Auto => "auto",
            LangPref::Ko => "ko",
            LangPref::En => "en",
        }
    }
}

fn path() -> Option<PathBuf> {
    let dir = std::env::var_os("APPDATA").filter(|d| !d.is_empty())?;
    Some(PathBuf::from(dir).join("Darak").join("settings.toml"))
}

/// 저장한 언어. 파일이 없거나 모르는 값이면 Auto.
pub fn load() -> LangPref {
    path().and_then(|p| fs::read_to_string(p).ok()).map_or(LangPref::Auto, |text| parse(&text))
}

pub fn save(pref: LangPref) -> io::Result<()> {
    let path = path().ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "APPDATA"))?;
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(path, format!("language = \"{}\"\n", pref.key()))
}

fn parse(text: &str) -> LangPref {
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else { continue };
        if key.trim() != "language" {
            continue;
        }
        return match value.trim().trim_matches('"') {
            "ko" => LangPref::Ko,
            "en" => LangPref::En,
            _ => LangPref::Auto,
        };
    }
    LangPref::Auto
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_saved_language() {
        assert_eq!(parse("language = \"en\"\n"), LangPref::En);
        assert_eq!(parse("language=\"ko\""), LangPref::Ko);
        assert_eq!(parse("language = \"auto\"\n"), LangPref::Auto);
        assert_eq!(parse("language = \"fr\"\n"), LangPref::Auto);
        assert_eq!(parse(""), LangPref::Auto);
        assert_eq!(parse("# 주석\nother = 1\nlanguage = \"en\""), LangPref::En);
    }

    #[test]
    fn saved_text_round_trips() {
        for pref in LangPref::ALL {
            assert_eq!(parse(&format!("language = \"{}\"\n", pref.key())), pref);
        }
    }
}
