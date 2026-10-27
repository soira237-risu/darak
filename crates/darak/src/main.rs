#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod delete;
mod doc;
mod drives;
mod fonts;
#[cfg(feature = "perf")]
mod perf_trace;
mod preview_worker;
mod search_panel;
mod search_worker;
mod theme;
mod treemap;

fn main() -> eframe::Result {
    // 매체 없는 카드 리더의 용량을 물어도 시스템 '디스크 없음' 창이 뜨지 않게
    darak_core::scan::suppress_drive_dialogs();
    #[cfg(feature = "perf")]
    let fixture = std::env::var("DARAK_PERF_FIXTURE").ok().map(|s| {
        let parts: Vec<_> = s.split(',').collect();
        assert_eq!(parts.len(), 3, "DARAK_PERF_FIXTURE must be nodes,shape,unicode-percent");
        let nodes = parts[0].parse().expect("invalid fixture node count");
        let shape = match parts[1] { "wide" => darak_core::perf::Shape::Wide, "deep" => darak_core::perf::Shape::Deep, "balanced" => darak_core::perf::Shape::Balanced, _ => panic!("invalid fixture shape") };
        let unicode = parts[2].parse().expect("invalid fixture Unicode percentage");
        let tree = darak_core::perf::make_fixture(nodes, shape, unicode);
        let id = format!("seed{}-{}-n{nodes}-u{unicode}-{:016x}", darak_core::perf::FIXTURE_SEED, parts[1], darak_core::perf::fingerprint(&tree));
        let files = tree.node(0).files as u64;
        let stats = darak_core::ScanStats { elapsed: std::time::Duration::ZERO, files, dirs: tree.len() as u64 - files, errors: 0, hardlink_dups: 0, cancelled: false, method: darak_core::ScanMethod::Walk, fallback_reason: None };
        (doc::Doc::new(1, tree, stats), id)
    });
    #[cfg(feature = "perf")]
    let (trace, runtime) = match std::env::var_os("DARAK_PERF_TRACE_PATH") {
        Some(path) => { let (trace, runtime) = perf_trace::PerfTraceRuntime::new(path.into()); (Some(trace), Some(runtime)) },
        None => (None, None),
    };
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            // 한국어 Windows의 '관리자: 명령 프롬프트'와 같은 관례
            .with_title(if darak_core::scan::is_elevated() { "관리자: Darak" } else { "Darak" })
            .with_inner_size([1280.0, 820.0])
            .with_min_inner_size([640.0, 400.0])
            .with_icon(
                eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon-256.png"))
                    .expect("창 아이콘 PNG를 읽지 못함"),
            ),
        ..Default::default()
    };
    let (search, search_runtime) = search_worker::SearchRuntime::new();
    let (preview, preview_runtime) = preview_worker::PreviewRuntime::new();
    let result = eframe::run_native("darak", options, Box::new(move |cc| Ok(Box::new(app::App::new(
        cc, search, preview,
        #[cfg(feature = "perf")] fixture,
        #[cfg(feature = "perf")] trace,
    )))));
    preview_runtime.shutdown_and_join();
    search_runtime.shutdown_and_join();
    #[cfg(feature = "perf")]
    if let Some(runtime) = runtime && let Err(error) = runtime.write_csv() {
        eprintln!("performance trace write failed: {error}");
    }
    result
}
