//! exe 파일에 앱 아이콘을 리소스로 넣는다 (탐색기·작업 표시줄·바로 가기에 보이는 아이콘).
//! 아이콘은 `tools/make-icon.ps1`로 다시 만든다.

fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        res.compile().expect("Windows 리소스(아이콘) 컴파일 실패");
    }
}
