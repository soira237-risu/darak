//! 삭제: 보호 목록 검사, 휴지통 사전 점검, 휴지통으로 이동, 영구 삭제.

use std::io;

use crate::sys::{
    DRIVE_FIXED, DRIVE_REMOTE, DRIVE_REMOVABLE, GetActiveWindow, GetDriveTypeW, GetVolumeNameForVolumeMountPointW,
    GetVolumePathNameW, HANDLE, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, MAX_PATH, RRF_RT_REG_DWORD, RegGetValueW, wide,
};
use crate::tree::{Kind, NodeId, Tree, flags};

/// 보호 비교에만 쓰는 키. 실행 경로는 바꾸지 않는다.
fn protection_key(path: &str) -> Option<String> {
    let path = path.strip_prefix(r"\\?\").unwrap_or(path);
    let bytes = path.as_bytes();
    if bytes.len() < 3 || !bytes[0].is_ascii_alphabetic() || bytes[1] != b':' || bytes[2] != b'\\' {
        return None;
    }
    let tail = path[3..].trim_end_matches('\\');
    if !tail.is_empty()
        && tail.split('\\').any(|part| {
            part.is_empty()
                || matches!(part, "." | "..")
                || part.ends_with(['.', ' '])
                || part.chars().any(|c| {
                    c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '/' | '|' | '?' | '*')
                })
        })
    {
        return None;
    }
    let key = if tail.is_empty() {
        &path[..3]
    } else {
        path.trim_end_matches('\\')
    };
    Some(key.to_lowercase())
}

/// 삭제 보호 수준. 관리자 권한이면 NTFS 권한이 막아 주던 곳을 직접 막는다.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Guard {
    /// 일반 권한
    #[default]
    User,
    /// 관리자 권한: 모든 사용자의 프로필 루트·기본 폴더, Program Files·(x86)와 ProgramData의 직속 하위까지 막는다
    Admin,
}

/// 일반 권한 기준으로 이 노드를 지우면 안 되는 이유. 지워도 되면 None.
pub fn protected_reason(tree: &Tree, id: NodeId) -> Option<&'static str> {
    protected_reason_as(tree, id, Guard::User)
}

/// 보호 수준 guard에서 이 노드를 지우면 안 되는 이유. 지워도 되면 None.
pub fn protected_reason_as(tree: &Tree, id: NodeId, guard: Guard) -> Option<&'static str> {
    if id == Tree::ROOT {
        return Some("스캔한 루트 폴더");
    }
    let n = tree.node(id);
    if n.kind == Kind::Link {
        return Some("junction/심볼릭 링크 (대상 폴더가 지워질 수 있음)");
    }
    let mut cur = Some(id);
    while let Some(ancestor) = cur {
        if tree.node(ancestor).has(flags::LOSSY_NAME) {
            return Some("이름을 정확히 복원할 수 없는 항목 또는 상위 폴더");
        }
        cur = tree.parent(ancestor);
    }
    let Some(lower) = protection_key(&tree.path(id)) else {
        return Some("삭제 보호를 확인할 수 없는 경로 (UNC/장치/상대 경로 등)");
    };
    if lower.len() == 3 {
        return Some("드라이브 루트");
    }

    let sys_drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
    let Some(sys_root) = protection_key(&format!("{}\\", sys_drive.trim_end_matches('\\'))) else {
        return Some("시스템 경로의 삭제 보호를 확인할 수 없음");
    };
    let sys_drive = &sys_root[..2];
    let windir = std::env::var("WINDIR").unwrap_or_else(|_| format!("{sys_drive}\\windows"));
    let Some(windir) = protection_key(&windir) else {
        return Some("시스템 경로의 삭제 보호를 확인할 수 없음");
    };
    let home = match std::env::var("USERPROFILE") {
        Ok(home) if !home.is_empty() => match protection_key(&home) {
            Some(home) => home,
            None => return Some("사용자 경로의 삭제 보호를 확인할 수 없음"),
        },
        _ => String::new(),
    };

    // 이 항목이 있는 드라이브 (`d:`)
    let drive = &lower[..2];
    let inside =
        |base: &str| !base.is_empty() && (lower == base || lower.starts_with(&format!("{base}\\")));
    if inside(&windir) {
        return Some("Windows 시스템 폴더");
    }
    for name in [
        "system volume information",
        "$recycle.bin",
        "recovery",
        "boot",
        "$windows.~bt",
        "$windows.~ws",
    ] {
        if inside(&format!("{drive}\\{name}")) {
            return Some("시스템 보호 폴더");
        }
    }
    let exact = [
        format!("{drive}\\program files"),
        format!("{drive}\\program files (x86)"),
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
    if exact.iter().any(|e| !e.is_empty() && lower == *e) {
        return Some("시스템/사용자 기본 폴더");
    }
    if guard == Guard::Admin
        && let Some(reason) = admin_reason(&lower, sys_drive)
    {
        return Some(reason);
    }
    // 드라이브 바로 아래의 시스템 파일과 NTFS 메타 파일
    if tree.parent(id) == Some(Tree::ROOT)
        && protection_key(tree.name(Tree::ROOT)).is_some_and(|root| root.len() == 3)
    {
        let name = tree.name(id).to_lowercase();
        if name.starts_with('$')
            || matches!(
                name.as_str(),
                "pagefile.sys" | "hiberfil.sys" | "swapfile.sys" | "dumpstack.log"
            )
        {
            return Some("시스템 파일");
        }
    }
    None
}

/// 관리자 권한에서만 더 막는 곳 (일반 권한이면 NTFS 권한이 막아 준다). lower는 protection_key 결과이고,
/// 더 깊은 하위 항목(`program files\app\cache`, 다른 프로필의 `downloads\big.iso`)은 막지 않는다.
fn admin_reason(lower: &str, sys_drive: &str) -> Option<&'static str> {
    let (parent, _) = lower.rsplit_once('\\')?;
    // 부모가 어느 드라이브든 `x:\program files` 또는 `x:\program files (x86)`
    if matches!(parent.get(3..), Some("program files" | "program files (x86)")) {
        return Some("설치된 프로그램 폴더 (관리자 권한)");
    }
    if parent == format!("{sys_drive}\\programdata") {
        return Some("공용 프로그램 데이터 (관리자 권한)");
    }
    // {sys}\users\<이름> 아래의 꼬리
    let tail: Vec<&str> = lower.strip_prefix(&format!("{sys_drive}\\users\\"))?.split('\\').skip(1).collect();
    match tail[..] {
        [] | ["appdata"] | ["appdata", "local" | "roaming"] | ["documents" | "desktop" | "downloads"] => {
            Some("사용자 프로필 기본 폴더 (관리자 권한)")
        }
        _ => None,
    }
}

/// 볼륨의 휴지통 상태
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecycleBin {
    /// 휴지통이 있다. capacity는 설정된 최대 크기 (바이트, 읽지 못하면 None)
    Available { capacity: Option<u64> },
    /// 휴지통으로 보낼 수 없다 (이유)
    Unavailable(&'static str),
}

/// 드라이브 종류(GetDriveTypeW)와 휴지통 설정으로 휴지통 상태를 정한다.
/// max_mb는 볼륨의 MaxCapacity(MB)이고, 0은 뜻이 분명하지 않아 모름으로 본다.
pub fn classify_bin(kind: u32, nuke: bool, max_mb: Option<u32>, policy_off: bool) -> RecycleBin {
    match kind {
        DRIVE_REMOTE => RecycleBin::Unavailable("네트워크 드라이브에는 휴지통이 없습니다"),
        DRIVE_REMOVABLE => RecycleBin::Unavailable("이동식 드라이브에는 휴지통이 없습니다"),
        DRIVE_FIXED if nuke || policy_off => RecycleBin::Unavailable("이 드라이브는 휴지통을 쓰지 않도록 설정되어 있습니다"),
        DRIVE_FIXED => RecycleBin::Available { capacity: max_mb.filter(|&m| m > 0).map(|m| u64::from(m) << 20) },
        _ => RecycleBin::Unavailable("휴지통을 쓸 수 있는 드라이브인지 확인할 수 없습니다"),
    }
}

/// 스캔 루트가 있는 볼륨의 휴지통 상태. 스캔은 마운트 지점·링크를 따라가지 않으므로 한 문서의 항목은 모두
/// 루트와 같은 볼륨에 있고, 문서마다 한 번 확인하면 된다.
/// 네트워크 위치는 장치에 묻지 않고 바로 판정한다 (끊긴 네트워크 드라이브에서 UI 스레드가 멈추지 않게).
/// 레지스트리 값은 Explorer의 설정 저장 위치라 공개 API가 아니다. 놓친 경우는 recycle의 백스톱이 잡는다.
pub fn recycle_bin_for(root: &str) -> RecycleBin {
    let plain = root.strip_prefix(r"\\?\").unwrap_or(root);
    if plain.starts_with(r"UNC\") || (plain.starts_with(r"\\") && !plain.starts_with(r"\\.\")) {
        return classify_bin(DRIVE_REMOTE, false, None, false);
    }
    let letter = plain
        .as_bytes()
        .get(..2)
        .filter(|b| b[0].is_ascii_alphabetic() && b[1] == b':')
        .map(|b| format!("{}:\\", b[0] as char));
    if letter.as_deref().is_some_and(|l| drive_type(&wide(l)) == DRIVE_REMOTE) {
        return classify_bin(DRIVE_REMOTE, false, None, false);
    }
    // 마운트 지점 아래면 그 볼륨의 루트 (예: C:\mnt\usb\). 찾지 못하면 드라이브 문자 루트로 판정한다
    let query = if letter.is_some() { plain } else { root };
    let Some(vol) = volume_root(query).or_else(|| letter.as_deref().map(wide)) else {
        return classify_bin(0, false, None, false);
    };
    let kind = drive_type(&vol);
    if kind != DRIVE_FIXED {
        return classify_bin(kind, false, None, false);
    }
    let key = volume_guid(&vol).and_then(|v| bitbucket_key(&v));
    let setting = |name: &str| key.as_deref().and_then(|k| reg_dword(HKEY_CURRENT_USER, k, name));
    let policy = |hkey| reg_dword(hkey, POLICY_KEY, "NoRecycleFiles").is_some_and(|v| v != 0);
    classify_bin(
        kind,
        setting("NukeOnDelete").is_some_and(|v| v != 0),
        setting("MaxCapacity"),
        policy(HKEY_CURRENT_USER) || policy(HKEY_LOCAL_MACHINE),
    )
}

const POLICY_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Policies\Explorer";

/// root는 NUL로 끝나는 UTF-16
fn drive_type(root: &[u16]) -> u32 {
    unsafe { GetDriveTypeW(root.as_ptr()) }
}

/// 경로가 있는 볼륨의 루트 (마운트 지점이면 그 폴더). NUL로 끝나는 UTF-16이고 끝에 `\`가 있다
fn volume_root(path: &str) -> Option<Vec<u16>> {
    let path = wide(path);
    let mut buf = [0u16; 1024];
    let ok = unsafe { GetVolumePathNameW(path.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) };
    let end = buf.iter().position(|&c| c == 0)?;
    (ok != 0 && end > 0).then(|| buf[..=end].to_vec())
}

/// 볼륨 루트(NUL로 끝나는 UTF-16) → `\\?\Volume{GUID}\`
fn volume_guid(mount: &[u16]) -> Option<String> {
    let mut buf = [0u16; 64];
    let ok = unsafe { GetVolumeNameForVolumeMountPointW(mount.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) };
    let end = buf.iter().position(|&c| c == 0)?;
    (ok != 0).then(|| String::from_utf16_lossy(&buf[..end]))
}

/// `\\?\Volume{GUID}\` → 그 볼륨의 휴지통 설정 키 (HKCU 아래, Explorer가 볼륨마다 저장한다)
fn bitbucket_key(volume: &str) -> Option<String> {
    let guid = volume.strip_prefix(r"\\?\Volume")?.trim_end_matches('\\');
    (guid.len() > 2 && guid.starts_with('{') && guid.ends_with('}'))
        .then(|| format!(r"Software\Microsoft\Windows\CurrentVersion\Explorer\BitBucket\Volume\{guid}"))
}

/// 레지스트리 DWORD 값 (키나 값이 없거나 읽지 못하면 None)
fn reg_dword(hkey: HANDLE, key: &str, value: &str) -> Option<u32> {
    let (key, value) = (wide(key), wide(value));
    let (mut data, mut len) = (0u32, size_of::<u32>() as u32);
    let r = unsafe {
        RegGetValueW(hkey, key.as_ptr(), value.as_ptr(), RRF_RT_REG_DWORD, std::ptr::null_mut(), (&mut data as *mut u32).cast(), &mut len)
    };
    (r == 0).then_some(data)
}

/// 항목 하나를 휴지통으로 보낼 수 없는 이유
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecycleBlock {
    /// 볼륨 전체에 휴지통이 없다
    NoBin(&'static str),
    /// 경로가 MAX_PATH 이상이라 셸이 다루지 못한다
    LongPath,
    /// 휴지통 최대 크기보다 크다
    TooLarge { capacity: u64 },
}

impl RecycleBlock {
    /// 검토 창에 보여 줄 문장
    pub fn reason(&self) -> String {
        match self {
            Self::NoBin(r) => (*r).into(),
            Self::LongPath => "경로가 260자 이상이라 휴지통으로 보낼 수 없습니다".into(),
            Self::TooLarge { capacity } => format!("휴지통 최대 크기({})보다 큽니다", crate::human_size(*capacity)),
        }
    }
}

/// 이 항목을 휴지통으로 보낼 수 없는 이유 (보낼 수 있으면 None). bin은 문서 루트의 recycle_bin_for 결과다
/// (한 문서의 항목은 모두 루트와 같은 볼륨에 있다).
pub fn recycle_block(tree: &Tree, id: NodeId, bin: RecycleBin) -> Option<RecycleBlock> {
    let capacity = match bin {
        RecycleBin::Unavailable(r) => return Some(RecycleBlock::NoBin(r)),
        RecycleBin::Available { capacity } => capacity,
    };
    if tree.path(id).encode_utf16().count() >= MAX_PATH {
        return Some(RecycleBlock::LongPath);
    }
    let n = tree.node(id);
    capacity.filter(|&c| n.alloc.max(n.logical) > c).map(|capacity| RecycleBlock::TooLarge { capacity })
}

#[repr(C)]
#[allow(non_snake_case)]
#[allow(clippy::upper_case_acronyms, reason = "Win32 ABI structure name")]
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
/// 휴지통에 넣지 못해 영구 삭제하게 되면 FOF_NOCONFIRMATION이 있어도 묻는다
const FOF_WANTNUKEWARNING: u16 = 0x4000;

/// 지금 활성화된 이 스레드의 창 (recycle의 owner로 넘긴다). 창을 가진 UI 스레드에서만 불러야 한다.
pub fn owner_window() -> isize {
    unsafe { GetActiveWindow() as isize }
}

/// 휴지통으로 이동한다. 휴지통에 넣을 수 없는 항목이면 Windows가 owner 창(HWND, 없으면 0) 위에서
/// 영구 삭제할지 묻는다 (사전 점검 recycle_block이 놓친 경우의 백스톱).
/// 작업자 스레드에서 불러도 경고창이 owner에 소유되어 두 스레드의 입력 큐가 붙으므로,
/// 경고창이 떠 있는 동안 owner 창은 입력을 받지 않는다 (정상).
pub fn recycle(path: &str, owner: isize) -> io::Result<()> {
    if path.encode_utf16().count() >= MAX_PATH {
        return Err(io::Error::other("경로가 너무 길어 휴지통으로 보낼 수 없습니다 (영구 삭제만 가능)"));
    }
    let mut from = wide(path);
    from.push(0); // 이중 NUL
    let mut op = SHFILEOPSTRUCTW {
        hwnd: owner as HANDLE,
        wFunc: FO_DELETE,
        pFrom: from.as_ptr(),
        pTo: std::ptr::null(),
        fFlags: FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_SILENT | FOF_NOERRORUI | FOF_WANTNUKEWARNING,
        fAnyOperationsAborted: 0,
        hNameMappings: std::ptr::null_mut(),
        lpszProgressTitle: std::ptr::null(),
    };
    let r = unsafe { SHFileOperationW(&mut op) };
    if r != 0 {
        return Err(match describe(r) {
            Some((kind, message)) => io::Error::new(kind, message),
            None => io::Error::other(format!("휴지통으로 옮기지 못했습니다 (코드 0x{r:X})")),
        });
    }
    if op.fAnyOperationsAborted != 0 {
        return Err(io::Error::new(io::ErrorKind::Interrupted, "Windows의 영구 삭제 확인에서 취소해 지우지 않았습니다"));
    }
    if std::fs::symlink_metadata(path).is_ok() {
        return Err(io::Error::other("일부 항목을 휴지통으로 옮기지 못했습니다 (사용 중인 파일일 수 있음)"));
    }
    Ok(())
}

const MISSING: &str = "항목을 찾을 수 없습니다 (이미 옮겨졌거나 이름이 바뀌었을 수 있음)";
const DENIED: &str = "권한이 없습니다 (관리자 권한이 필요하거나 시스템이 보호하는 파일)";
const TOO_LONG: &str = "경로가 너무 깁니다";
const CANCELLED: &str = "취소해서 지우지 않았습니다";

/// SHFileOperationW 실패 코드를 문장으로 바꾼다. DE_* 코드는 일부 Win32 코드와 값이 겹치며
/// (0x79 = ERROR_SEM_TIMEOUT 등) 여기서는 DE_* 뜻이 먼저다. 모르는 코드면 None.
fn describe(code: i32) -> Option<(io::ErrorKind, &'static str)> {
    use io::ErrorKind::{Interrupted, NotFound, Other, PermissionDenied};
    Some(match code {
        // DE_INVALIDFILES
        0x7C => (NotFound, MISSING),
        // DE_ACCESSDENIEDSRC
        0x78 => (PermissionDenied, DENIED),
        // DE_PATHTOODEEP, DE_FILENAMETOOLONG, DE_ERROR_MAX
        0x79 | 0x81 | 0xB7 => (Other, TOO_LONG),
        // DE_OPCANCELLED
        0x75 => (Interrupted, CANCELLED),
        _ => return win32_message(code),
    })
}

/// Win32 오류 코드만 문장으로 바꾼다. std 파일 작업의 오류는 DE_* 코드가 아니므로 이 표만 쓴다.
fn win32_message(code: i32) -> Option<(io::ErrorKind, &'static str)> {
    use io::ErrorKind::{Interrupted, NotFound, Other, PermissionDenied};
    Some(match code {
        // ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND
        2 | 3 => (NotFound, MISSING),
        // ERROR_ACCESS_DENIED
        5 => (PermissionDenied, DENIED),
        // ERROR_SHARING_VIOLATION, ERROR_LOCK_VIOLATION
        32 | 33 => (Other, "다른 프로그램이 사용 중인 파일이 있습니다"),
        // ERROR_DIR_NOT_EMPTY
        145 => (Other, "폴더 안의 일부 항목을 지우지 못했습니다"),
        // ERROR_WRITE_PROTECT
        19 => (Other, "쓰기 금지된 드라이브입니다"),
        // ERROR_FILENAME_EXCED_RANGE
        206 => (Other, TOO_LONG),
        // ERROR_CANCELLED
        1223 => (Interrupted, CANCELLED),
        _ => return None,
    })
}

/// io 오류의 Win32 코드를 문장으로 바꾼다 (종류는 그대로 둔다)
fn humanize(e: io::Error) -> io::Error {
    match e.raw_os_error().and_then(win32_message) {
        Some((_, message)) => io::Error::new(e.kind(), message),
        None => e,
    }
}

/// 긴 경로를 허용하는 `\\?\` 경로 (`\\`로 시작하면 그대로)
fn long_path(path: &str) -> String {
    if path.starts_with(r"\\") { path.to_string() } else { format!(r"\\?\{path}") }
}

/// 되돌릴 수 없게 지운다. 디렉터리 안의 junction은 따라가지 않는다.
pub fn delete_permanently(path: &str, is_dir: bool) -> io::Result<()> {
    let long = long_path(path);
    let r = if is_dir { std::fs::remove_dir_all(&long) } else { std::fs::remove_file(&long) };
    r.map_err(humanize)
}

/// 실행 직전에 항목이 스캔한 때와 같은지 확인한다 (있는지와 종류만, 크기·시각은 보지 않는다).
/// reparse는 스캔에서 reparse 속성이 있던 파일(flags::REPARSE)이면 true다 (파일 symlink를 바뀐 것으로 보지 않게).
pub fn still_there(path: &str, is_dir: bool, reparse: bool) -> io::Result<()> {
    let ft = match std::fs::symlink_metadata(long_path(path)) {
        Ok(m) => m.file_type(),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(io::Error::new(io::ErrorKind::NotFound, "이미 없는 항목입니다 (다른 곳에서 지워졌거나 이름이 바뀌었을 수 있음)"));
        }
        Err(e) => return Err(humanize(e)),
    };
    // Windows에서 junction과 symlink는 is_symlink()이고 is_dir()이 아니다
    if !reparse && ft.is_symlink() {
        return Err(io::Error::other("스캔한 뒤 링크로 바뀐 항목입니다"));
    }
    if ft.is_dir() != is_dir {
        return Err(io::Error::other("스캔한 뒤 종류가 바뀐 항목입니다"));
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_recycle_bin() {
        const NONE: &str = "휴지통을 쓸 수 있는 드라이브인지 확인할 수 없습니다";
        const OFF: &str = "이 드라이브는 휴지통을 쓰지 않도록 설정되어 있습니다";
        let cases = [
            (DRIVE_REMOTE, false, Some(1024), false, RecycleBin::Unavailable("네트워크 드라이브에는 휴지통이 없습니다")),
            (DRIVE_REMOVABLE, false, None, false, RecycleBin::Unavailable("이동식 드라이브에는 휴지통이 없습니다")),
            (DRIVE_FIXED, true, Some(1024), false, RecycleBin::Unavailable(OFF)),
            (DRIVE_FIXED, false, Some(1024), true, RecycleBin::Unavailable(OFF)),
            (DRIVE_FIXED, false, Some(1024), false, RecycleBin::Available { capacity: Some(1 << 30) }),
            (DRIVE_FIXED, false, None, false, RecycleBin::Available { capacity: None }),
            (DRIVE_FIXED, false, Some(0), false, RecycleBin::Available { capacity: None }),
            (DRIVE_FIXED, false, Some(u32::MAX), false, RecycleBin::Available { capacity: Some(u64::from(u32::MAX) << 20) }),
            (0, false, None, false, RecycleBin::Unavailable(NONE)),
            (1, false, None, false, RecycleBin::Unavailable(NONE)),
            (5, false, None, false, RecycleBin::Unavailable(NONE)), // CD-ROM
            (6, false, None, false, RecycleBin::Unavailable(NONE)), // RAM 디스크
        ];
        for (kind, nuke, max_mb, policy, expected) in cases {
            assert_eq!(classify_bin(kind, nuke, max_mb, policy), expected, "{kind} {nuke} {max_mb:?} {policy}");
        }
    }

    #[test]
    fn describes_common_failures() {
        use io::ErrorKind::{Interrupted, NotFound, Other, PermissionDenied};
        let permission = Some((PermissionDenied, "권한이 없습니다 (관리자 권한이 필요하거나 시스템이 보호하는 파일)"));
        let missing = Some((NotFound, "항목을 찾을 수 없습니다 (이미 옮겨졌거나 이름이 바뀌었을 수 있음)"));
        let busy = Some((Other, "다른 프로그램이 사용 중인 파일이 있습니다"));
        let cancelled = Some((Interrupted, "취소해서 지우지 않았습니다"));
        let long = Some((Other, "경로가 너무 깁니다"));
        for (code, expected) in [
            (0x78, permission),
            (5, permission),
            (32, busy),
            (33, busy),
            (0x7C, missing),
            (2, missing),
            (3, missing),
            (0x75, cancelled),
            (1223, cancelled),
            (206, long),
            (0x79, long),
            (0x81, long),
            (0xB7, long),
            (145, Some((Other, "폴더 안의 일부 항목을 지우지 못했습니다"))),
            (19, Some((Other, "쓰기 금지된 드라이브입니다"))),
            (0, None),
            (0x402, None),
            (0x10000, None),
        ] {
            assert_eq!(describe(code), expected, "0x{code:X}");
        }
    }

    #[test]
    fn humanize_keeps_kind() {
        let e = humanize(io::Error::from_raw_os_error(5));
        assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(e.to_string(), "권한이 없습니다 (관리자 권한이 필요하거나 시스템이 보호하는 파일)");
        let e = humanize(io::Error::from_raw_os_error(145));
        assert_eq!(e.kind(), io::ErrorKind::DirectoryNotEmpty);
        assert_eq!(e.to_string(), "폴더 안의 일부 항목을 지우지 못했습니다");
        // DE_* 값과 겹치는 Win32 코드는 DE_* 뜻으로 바꾸지 않는다 (ERROR_SEM_TIMEOUT, ERROR_ALREADY_EXISTS …)
        for code in [87, 0x75, 0x78, 0x79, 0x7C, 0x81, 0xB7] {
            assert_eq!(humanize(io::Error::from_raw_os_error(code)).raw_os_error(), Some(code), "0x{code:X}");
        }
        let custom = humanize(io::Error::other("그대로"));
        assert_eq!(custom.to_string(), "그대로");
    }

    #[test]
    fn builds_bitbucket_key_from_volume_name() {
        assert_eq!(
            bitbucket_key(r"\\?\Volume{02d24f59-5d71-4adb-922a-47e8577df03c}\").as_deref(),
            Some(r"Software\Microsoft\Windows\CurrentVersion\Explorer\BitBucket\Volume\{02d24f59-5d71-4adb-922a-47e8577df03c}")
        );
        assert!(bitbucket_key(r"\\?\Volume{02d24f59-5d71-4adb-922a-47e8577df03c}").is_some());
        for bad in [r"C:\", r"\\?\Volume\", r"\\?\Volume{}\", r"\\?\Volume02d24f59\", r"Volume{1}\", ""] {
            assert_eq!(bitbucket_key(bad), None, "{bad}");
        }
    }

    #[test]
    fn network_roots_skip_device_queries() {
        for root in [r"\\server\share", r"\\server\share\folder", r"\\?\UNC\server\share"] {
            assert_eq!(recycle_bin_for(root), RecycleBin::Unavailable("네트워크 드라이브에는 휴지통이 없습니다"), "{root}");
        }
    }

    #[test]
    fn long_path_prefix() {
        assert_eq!(long_path(r"C:\a\b"), r"\\?\C:\a\b");
        assert_eq!(long_path(r"\\?\C:\a"), r"\\?\C:\a");
        assert_eq!(long_path(r"\\server\share\a"), r"\\server\share\a");
    }
}
