//! egui 기본 폰트에는 한글이 없으므로 Windows 시스템 폰트를 불러온다.

use std::sync::Arc;

use eframe::egui::{Context, FontData, FontDefinitions, FontFamily};

pub fn install(ctx: &Context) {
    let dir = std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".into()) + r"\Fonts\";
    let mut fonts = FontDefinitions::default();

    // (파일, 기본 글꼴로 쓸지): Latin은 Segoe UI, 한글은 맑은 고딕, 나머지는 한자·가나·기호 대체용.
    // 맑은 고딕을 맨 앞에 두면 `\`가 `₩`로 그려지므로 Segoe UI가 먼저 와야 한다.
    let candidates = [
        ("segoeui.ttf", true),
        ("malgun.ttf", true),
        ("YuGothM.ttc", false),
        ("msyh.ttc", false),
        ("seguisym.ttf", false),
    ];
    let mut primary = Vec::new();
    let mut fallback = Vec::new();
    for (file, is_primary) in candidates {
        let Ok(bytes) = std::fs::read(format!("{dir}{file}")) else { continue };
        fonts.font_data.insert(file.to_string(), Arc::new(FontData::from_owned(bytes)));
        if is_primary { primary.push(file.to_string()) } else { fallback.push(file.to_string()) }
    }

    let prop = fonts.families.entry(FontFamily::Proportional).or_default();
    for (i, name) in primary.iter().enumerate() {
        prop.insert(i, name.clone());
    }
    prop.extend(fallback.iter().cloned());
    let mono = fonts.families.entry(FontFamily::Monospace).or_default();
    mono.extend(primary.iter().chain(&fallback).cloned());

    ctx.set_fonts(fonts);
}
