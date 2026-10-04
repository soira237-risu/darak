//! 삭제: 보호 목록 검사, 휴지통으로 이동, 영구 삭제.

use std::io;

use crate::sys::wide;
use crate::tree::{Kind, NodeId, Tree};

/// 이 노드를 지우면 안 되는 이유. 지워도 되면 None.
pub fn protected_reason(tree: &Tree, id: NodeId) -> Option<&'static str> {
    if id == Tree::ROOT {
        return Some("스캔한 루트 폴더");
    }
    let n = tree.node(id);
    if n.kind == Kind::Link {
        return Some("junction/심볼릭 링크 (대상 폴더가 지워질 수 있음)");
    }
    let path = tree.path(id);
    let lower = path.to_lowercase();
    let lower = lower.trim_end_matches('\\');
    if lower.len() <= 3 {
        return Some("드라이브 루트");
    }

    let sys_drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into()).to_lowercase();
    let windir = std::env::var("WINDIR").unwrap_or_else(|_| format!("{sys_drive}\\windows")).to_lowercase();
    let home = std::env::var("USERPROFILE").unwrap_or_default().to_lowercase();

    let inside = |base: &str| !base.is_empty() && (lower == base || lower.starts_with(&format!("{base}\\")));
    if inside(&windir) {
        return Some("Windows 시스템 폴더");
    }
    for name in ["system volume information", "$recycle.bin", "recovery", "boot", "$windows.~bt", "$windows.~ws"] {
        if lower.len() > 3 && inside(&format!("{}\\{name}", &lower[..2])) {
            return Some("시스템 보호 폴더");
        }
    }
    let exact = [
        format!("{sys_drive}\\program files"),
        format!("{sys_drive}\\program files (x86)"),
        format!("{sys_drive}\\programdata"),
        format!("{sys_drive}\\users"),
        format!("{sys_drive}\\users\\public"),
        format!("{sys_drive}\\users\\default"),
        home.clone(),
        format!("{home}\\appdata"),
        format!("{home}\\appdata\\local"),
        format!("{home}\\appdata\\roaming"),
        format!("{home}\\documents"),
        format!("{home}\\desktop"),
        format!("{home}\\downloads"),
    ];
    if exact.iter().any(|e| !e.is_empty() && lower == e) {
        return Some("시스템/사용자 기본 폴더");
    }
    // 드라이브 바로 아래의 시스템 파일과 NTFS 메타 파일
    if tree.parent(id) == Some(Tree::ROOT) && tree.name(Tree::ROOT).len() <= 3 {
        let name = tree.name(id).to_lowercase();
        if name.starts_with('$') || matches!(name.as_str(), "pagefile.sys" | "hiberfil.sys" | "swapfile.sys" | "dumpstack.log") {
            return Some("시스템 파일");
        }
    }
    None
}

#[repr(C)]
#[allow(non_snake_case)]
struct SHFILEOPSTRUCTW {
    hwnd: *mut std::ffi::c_void,
    wFunc: u32,
    pFrom: *const u16,
    pTo: *const u16,
    fFlags: u16,
    fAnyOperationsAborted: i32,
    hNameMappings: *mut std::ffi::c_void,
    lpszProgressTitle: *const u16,
}

#[link(name = "shell32")]
unsafe extern "system" {
    fn SHFileOperationW(op: *mut SHFILEOPSTRUCTW) -> i32;
}

const FO_DELETE: u32 = 3;
const FOF_SILENT: u16 = 0x0004;
const FOF_NOCONFIRMATION: u16 = 0x0010;
const FOF_ALLOWUNDO: u16 = 0x0040;
const FOF_NOERRORUI: u16 = 0x0400;

/// 휴지통으로 이동한다.
pub fn recycle(path: &str) -> io::Result<()> {
    if path.encode_utf16().count() >= 260 {
        return Err(io::Error::other("경로가 너무 길어 휴지통으로 보낼 수 없습니다 (영구 삭제만 가능)"));
    }
    let mut from = wide(path);
    from.push(0); // 이중 NUL
    let mut op = SHFILEOPSTRUCTW {
        hwnd: std::ptr::null_mut(),
        wFunc: FO_DELETE,
        pFrom: from.as_ptr(),
        pTo: std::ptr::null(),
        fFlags: FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_SILENT | FOF_NOERRORUI,
        fAnyOperationsAborted: 0,
        hNameMappings: std::ptr::null_mut(),
        lpszProgressTitle: std::ptr::null(),
    };
    let r = unsafe { SHFileOperationW(&mut op) };
    if r != 0 {
        return Err(io::Error::other(format!("휴지통 이동 실패 (코드 0x{r:X})")));
    }
    if op.fAnyOperationsAborted != 0 {
        return Err(io::Error::other("취소됨"));
    }
    if std::fs::symlink_metadata(path).is_ok() {
        return Err(io::Error::other("일부 항목을 휴지통으로 옮기지 못했습니다 (사용 중인 파일일 수 있음)"));
    }
    Ok(())
}

/// 되돌릴 수 없게 지운다. 디렉터리 안의 junction은 따라가지 않는다.
pub fn delete_permanently(path: &str, is_dir: bool) -> io::Result<()> {
    let long = if path.starts_with(r"\\") { path.to_string() } else { format!(r"\\?\{path}") };
    if is_dir { std::fs::remove_dir_all(&long) } else { std::fs::remove_file(&long) }
}

/// 표시 집합에서, 다른 표시 항목 아래에 있는 항목을 뺀다. (상위 표시가 하위 표시를 흡수)
pub fn top_level_marks(tree: &Tree, marks: &[NodeId]) -> Vec<NodeId> {
    let set: std::collections::HashSet<NodeId> = marks.iter().copied().collect();
    let mut out: Vec<NodeId> = marks
        .iter()
        .copied()
        .filter(|&m| {
            let mut cur = tree.parent(m);
            while let Some(p) = cur {
                if set.contains(&p) {
                    return false;
                }
                cur = tree.parent(p);
            }
            true
        })
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}
