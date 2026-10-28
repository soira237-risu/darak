//! egui 기본 폰트에는 한글이 없으므로 Windows 시스템 폰트를 불러온다.
//! 순서는 「글자」 보드: Segoe UI → 맑은 고딕 → Yu Gothic → Microsoft YaHei → Segoe UI Symbol,
//! egui 내장 글꼴(이모지 글리프 포함)은 맨 끝 대체용.

use std::sync::Arc;

use eframe::egui::{Context, FontData, FontDefinitions, FontFamily, Id};

/// 굵기 600 글꼴 가족 이름 (`FontFamily::Name`)
pub const SEMIBOLD: &str = "semibold";

fn semibold_flag() -> Id {
    Id::new("darak.fonts.semibold")
}

/// 굵은 글꼴 가족. `install` 전(테스트의 기본 Context 등)에는 등록되지 않은 가족을 쓰면 egui가 멈추므로 보통 글꼴을 준다.
pub fn semibold_family(ctx: &Context) -> FontFamily {
    if ctx.data(|d| d.get_temp::<bool>(semibold_flag())).unwrap_or(false) {
        FontFamily::Name(SEMIBOLD.into())
    } else {
        FontFamily::Proportional
    }
}

fn load(fonts: &mut FontDefinitions, dir: &str, file: &str) -> bool {
    let Ok(bytes) = std::fs::read(format!("{dir}{file}")) else { return false };
    fonts.font_data.insert(file.to_string(), Arc::new(FontData::from_owned(bytes)));
    true
}

pub fn install(ctx: &Context) {
    let dir = std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".into()) + r"\Fonts\";
    let mut fonts = FontDefinitions::default();
    let mut pick = |files: &[&str]| -> Vec<String> {
        files.iter().filter(|f| load(&mut fonts, &dir, f)).map(|f| f.to_string()).collect()
    };
    // 맑은 고딕을 Segoe UI보다 앞에 두면 `\`가 `₩`로 그려지므로 Segoe UI가 먼저 와야 한다. 굵은 글꼴도 같다.
    let sans = pick(&["segoeui.ttf", "malgun.ttf", "YuGothM.ttc", "msyh.ttc", "seguisym.ttf"]);
    // 600: Segoe UI Semibold → 맑은 고딕 Bold(맑은 고딕엔 600이 없다). 없는 글자는 보통 글꼴로 대체된다.
    let bold = pick(&["seguisb.ttf", "malgunbd.ttf"]);
    // 고정폭: Cascadia Mono → Consolas. 한글 등은 sans로 대체된다.
    let mono = pick(&["CascadiaMono.ttf", "consola.ttf"]);

    let egui_prop = fonts.families.remove(&FontFamily::Proportional).unwrap_or_default();
    let egui_mono = fonts.families.remove(&FontFamily::Monospace).unwrap_or_default();
    let prop: Vec<String> = sans.into_iter().chain(egui_prop).collect();
    let semibold: Vec<String> = bold.into_iter().chain(prop.iter().cloned()).collect();
    let mono: Vec<String> = mono.into_iter().chain(egui_mono).chain(prop.iter().cloned()).collect();
    let has_semibold = !semibold.is_empty();
    fonts.families.insert(FontFamily::Proportional, prop);
    fonts.families.insert(FontFamily::Monospace, mono);
    if has_semibold {
        fonts.families.insert(FontFamily::Name(SEMIBOLD.into()), semibold);
    }

    ctx.set_fonts(fonts);
    if has_semibold {
        ctx.data_mut(|d| d.insert_temp(semibold_flag(), true));
    }
}
