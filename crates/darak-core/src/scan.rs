//! 일반 경로 스캐너: `GetFileInformationByHandleEx(FileIdBothDirectoryInfo)`로 디렉터리마다
//! 이름·크기·파일 ID를 한 번에 받고, rayon으로 하위 디렉터리를 병렬 순회한다.
//!
//! 순회 중에는 디렉터리마다 독립된 `DirBuf`를 만들고(공유 상태 없음),
//! 끝나면 BFS로 한 번에 아레나 트리로 펼친다.

use std::cell::RefCell;
use std::collections::{HashSet, VecDeque};
use std::io;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant};

use rayon::prelude::*;

use crate::sys::*;
use crate::tree::{Kind, NO_NODE, Node, Tree, filetime_to_secs2000, flags};

#[derive(Clone, Debug)]
pub struct ScanOptions {
    /// 같은 파일 ID(하드링크)는 한 번만 센다
    pub dedupe_hardlinks: bool,
    pub threads: Option<usize>,
    /// 드라이브 루트이고 관리자 권한이 있으면 $MFT를 직접 읽는다
    pub use_mft: bool,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self { dedupe_hardlinks: true, threads: None, use_mft: true }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanMethod {
    /// 디렉터리를 하나씩 여는 일반 순회
    Walk,
    /// $MFT 직접 읽기
    Mft,
}

/// 스캔 스레드와 UI가 공유하는 진행 상태
#[derive(Default)]
pub struct ScanProgress {
    pub files: AtomicU64,
    pub dirs: AtomicU64,
    pub bytes: AtomicU64,
    pub errors: AtomicU64,
    pub cancel: AtomicBool,
    pub current: Mutex<String>,
    /// 일반 순회 중 미리 보기용 얕은 트리 (MFT 스캔에서는 비어 있다)
    pub live: Mutex<LiveTree>,
    /// MFT 스캔일 때 전체 레코드 수와 처리한 레코드 수 (진행률 표시용, 일반 순회는 0)
    pub total_hint: AtomicU64,
    pub done_hint: AtomicU64,
}

impl ScanProgress {
    pub fn cancel(&self) {
        self.cancel.store(true, Relaxed);
    }
    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Relaxed)
    }
}

#[derive(Debug, Clone)]
pub struct ScanStats {
    pub elapsed: Duration,
    pub files: u64,
    pub dirs: u64,
    pub errors: u64,
    pub hardlink_dups: u64,
    pub cancelled: bool,
    pub method: ScanMethod,
    /// MFT를 시도했다가 일반 순회로 바꾼 이유
    pub fallback_reason: Option<String>,
}

pub struct ScanResult {
    pub tree: Tree,
    pub stats: ScanStats,
}

/// `root`(예: `C:\`, `D:\Games`)를 스캔한다.
pub fn scan(root: &str, opts: &ScanOptions, progress: &ScanProgress) -> io::Result<ScanResult> {
    let display = normalize_root(root);
    let mut fallback_reason = None;
    if opts.use_mft && is_drive_root(&display) {
        match crate::mft::scan_volume(&display, progress) {
            Ok((tree, stats)) => return Ok(ScanResult { tree, stats }),
            Err(e) => {
                fallback_reason = Some(if e.kind() == io::ErrorKind::PermissionDenied {
                    "관리자 권한이 없어 일반 스캔을 사용".to_string()
                } else {
                    format!("MFT 읽기 실패, 일반 스캔 사용: {e}")
                });
                progress.total_hint.store(0, Relaxed);
                progress.done_hint.store(0, Relaxed);
                progress.files.store(0, Relaxed);
                progress.dirs.store(0, Relaxed);
                progress.bytes.store(0, Relaxed);
            }
        }
    }

    let start = Instant::now();
    let os_path = to_os_path(&display);

    // 루트가 열리는지 먼저 확인해서 오류를 바로 돌려준다
    open_dir(&os_path)?;

    let mut builder = rayon::ThreadPoolBuilder::new().stack_size(8 << 20);
    if let Some(n) = opts.threads {
        builder = builder.num_threads(n);
    }
    let pool = builder.build().map_err(io::Error::other)?;

    let ctx = Ctx {
        progress,
        dedupe: opts.dedupe_hardlinks.then(|| (0..SHARDS).map(|_| Mutex::new(HashSet::new())).collect()),
        hardlink_dups: AtomicU64::new(0),
    };
    let root_buf = pool.install(|| scan_dir(os_path, &display, NO_NODE, 0, &ctx));
    drop(ctx.dedupe);

    let tree = build_tree(&display, root_buf);
    let stats = ScanStats {
        elapsed: start.elapsed(),
        files: progress.files.load(Relaxed),
        dirs: progress.dirs.load(Relaxed),
        errors: progress.errors.load(Relaxed),
        hardlink_dups: ctx.hardlink_dups.load(Relaxed),
        cancelled: progress.is_cancelled(),
        method: ScanMethod::Walk,
        fallback_reason,
    };
    Ok(ScanResult { tree, stats })
}

/// `C:\` 형태인지
pub fn is_drive_root(display: &str) -> bool {
    let b = display.as_bytes();
    b.len() == 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'\\'
}

/// 현재 프로세스가 관리자 권한(상승됨)으로 실행 중인지
pub fn is_elevated() -> bool {
    unsafe { IsUserAnAdmin() != 0 }
}

/// 기본 앱으로 연다 (탐색기에서 더블클릭한 것과 같다)
pub fn shell_open(path: &str) -> bool {
    let op = wide("open");
    let file = wide(path);
    let r = unsafe { ShellExecuteW(std::ptr::null_mut(), op.as_ptr(), file.as_ptr(), std::ptr::null(), std::ptr::null(), 1) };
    r as isize > 32
}

/// 같은 실행 파일을 관리자 권한으로 다시 실행한다. 성공하면 true (호출한 쪽은 종료하면 된다)
pub fn relaunch_as_admin(args: &str) -> bool {
    let Ok(exe) = std::env::current_exe() else { return false };
    let exe = wide(&exe.to_string_lossy());
    let op = wide("runas");
    let params = wide(args);
    let r = unsafe { ShellExecuteW(std::ptr::null_mut(), op.as_ptr(), exe.as_ptr(), params.as_ptr(), std::ptr::null(), 1) };
    r as isize > 32
}

/// `c:` → `C:\`, 끝의 `\` 정리
pub fn normalize_root(root: &str) -> String {
    let mut s = root.trim().replace('/', "\\");
    if s.len() == 2 && s.as_bytes()[1] == b':' {
        s.push('\\');
    }
    if s.len() >= 2 && s.as_bytes()[1] == b':' {
        s[..1].make_ascii_uppercase();
    }
    while s.len() > 3 && s.ends_with('\\') {
        s.pop();
    }
    s
}

/// 표시용 경로 → 긴 경로를 허용하는 `\\?\` 경로 (NUL 없음)
pub fn to_os_path(display: &str) -> Vec<u16> {
    let p = if display.starts_with(r"\\?\") {
        display.to_string()
    } else if let Some(unc) = display.strip_prefix(r"\\") {
        format!(r"\\?\UNC\{unc}")
    } else {
        format!(r"\\?\{display}")
    };
    p.encode_utf16().collect()
}

// ---------------------------------------------------------------------------

const SHARDS: usize = 64;
const BUF_WORDS: usize = 16 * 1024; // 128KB

thread_local! {
    static DIR_BUF: RefCell<Vec<u64>> = RefCell::new(vec![0u64; BUF_WORDS]);
}

struct Ctx<'a> {
    progress: &'a ScanProgress,
    dedupe: Option<Vec<Mutex<HashSet<u64>>>>,
    hardlink_dups: AtomicU64,
}

impl Ctx<'_> {
    /// 이 파일 ID를 처음 보면 true
    fn first_seen(&self, id: u64) -> bool {
        let Some(shards) = &self.dedupe else { return true };
        if id == 0 {
            return true;
        }
        let h = id.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        let shard = (h >> 58) as usize % SHARDS;
        shards[shard].lock().unwrap().insert(id)
    }
}

#[derive(Clone, Copy)]
struct RawEntry {
    name_off: u32,
    name_len: u16,
    kind: Kind,
    flags: u8,
    alloc: u64,
    logical: u64,
    mtime: u32,
}

#[derive(Default)]
struct DirBuf {
    names: String,
    files: Vec<RawEntry>,
    dirs: Vec<(RawEntry, DirBuf)>,
    alloc: u64,
    logical: u64,
    file_count: u64,
    denied: bool,
}

fn open_dir(path_no_nul: &[u16]) -> io::Result<OwnedHandle> {
    let mut p = Vec::with_capacity(path_no_nul.len() + 1);
    p.extend_from_slice(path_no_nul);
    p.push(0);
    let h = unsafe {
        CreateFileW(
            p.as_ptr(),
            FILE_LIST_DIRECTORY,
            FILE_SHARE_ALL,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        Err(io::Error::last_os_error())
    } else {
        Ok(OwnedHandle(h))
    }
}

/// 디렉터리 하나를 읽어 파일은 `d.files`에, 하위 디렉터리는 `subdirs`에 넣는다.
fn read_dir(path: &[u16], d: &mut DirBuf, subdirs: &mut Vec<RawEntry>, ctx: &Ctx) -> io::Result<()> {
    let h = open_dir(path)?;
    DIR_BUF.with_borrow_mut(|buf| {
        loop {
            let ok = unsafe {
                GetFileInformationByHandleEx(
                    h.0,
                    FILE_ID_BOTH_DIRECTORY_INFO,
                    buf.as_mut_ptr().cast(),
                    (buf.len() * 8) as u32,
                )
            };
            if ok == 0 {
                let err = io::Error::last_os_error();
                return if err.raw_os_error() == Some(ERROR_NO_MORE_FILES) { Ok(()) } else { Err(err) };
            }
            let base = buf.as_ptr() as *const u8;
            let mut off = 0usize;
            loop {
                // SAFETY: 커널이 채운 버퍼 안의 항목이며, NextEntryOffset은 8바이트 정렬이다
                let info = unsafe { &*(base.add(off) as *const FILE_ID_BOTH_DIR_INFO) };
                let name = unsafe {
                    std::slice::from_raw_parts(info.FileName.as_ptr(), info.FileNameLength as usize / 2)
                };
                if name != [b'.' as u16] && name != [b'.' as u16, b'.' as u16] {
                    push_entry(info, name, d, subdirs, ctx);
                }
                if info.NextEntryOffset == 0 {
                    break;
                }
                off += info.NextEntryOffset as usize;
            }
        }
    })
}

fn push_entry(info: &FILE_ID_BOTH_DIR_INFO, name: &[u16], d: &mut DirBuf, subdirs: &mut Vec<RawEntry>, ctx: &Ctx) {
    let name_off = d.names.len() as u32;
    let mut entry_flags = 0u8;
    for c in char::decode_utf16(name.iter().copied()) {
        match c {
            Ok(c) => d.names.push(c),
            Err(_) => {
                d.names.push(char::REPLACEMENT_CHARACTER);
                entry_flags |= flags::LOSSY_NAME;
            }
        }
    }
    let name_len = (d.names.len() as u32 - name_off) as u16;
    let attrs = info.FileAttributes;
    let reparse = attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0;
    let mut e = RawEntry {
        name_off,
        name_len,
        kind: Kind::File,
        flags: entry_flags,
        alloc: info.AllocationSize.max(0) as u64,
        logical: info.EndOfFile.max(0) as u64,
        mtime: filetime_to_secs2000(info.LastWriteTime),
    };
    if attrs & FILE_ATTRIBUTE_DIRECTORY != 0 {
        e.alloc = 0;
        e.logical = 0;
        if reparse {
            e.kind = Kind::Link;
            d.files.push(e);
        } else {
            e.kind = Kind::Dir;
            subdirs.push(e);
        }
        return;
    }
    if reparse {
        e.flags |= flags::REPARSE;
    }
    if !ctx.first_seen(info.FileId as u64) {
        e.flags |= flags::HARDLINK_DUP;
        e.alloc = 0;
        e.logical = 0;
        ctx.hardlink_dups.fetch_add(1, Relaxed);
    }
    d.alloc += e.alloc;
    d.logical += e.logical;
    d.file_count += 1;
    d.files.push(e);
}

fn scan_dir(path: Vec<u16>, name: &str, live_parent: u32, depth: u16, ctx: &Ctx) -> DirBuf {
    let mut d = DirBuf::default();
    if ctx.progress.is_cancelled() {
        return d;
    }
    let mut subdirs = Vec::new();
    if read_dir(&path, &mut d, &mut subdirs, ctx).is_err() {
        d.denied = true;
        ctx.progress.errors.fetch_add(1, Relaxed);
    }
    let p = ctx.progress;
    p.files.fetch_add(d.file_count, Relaxed);
    p.bytes.fetch_add(d.alloc, Relaxed);
    let n = p.dirs.fetch_add(1, Relaxed);
    if n.is_multiple_of(64)
        && let Ok(mut cur) = p.current.try_lock() {
            *cur = String::from_utf16_lossy(&path[4..]); // `\\?\` 제거
        }
    let live_idx = p.live.lock().unwrap().add(live_parent, depth, name, &d);

    let children: Vec<DirBuf> = subdirs
        .par_iter()
        .map(|e| {
            let name = &d.names[e.name_off as usize..][..e.name_len as usize];
            let mut child = Vec::with_capacity(path.len() + 1 + name.len());
            child.extend_from_slice(&path);
            if child.last() != Some(&(b'\\' as u16)) {
                child.push(b'\\' as u16);
            }
            child.extend(name.encode_utf16());
            scan_dir(child, name, live_idx, depth + 1, ctx)
        })
        .collect();

    for c in &children {
        d.alloc += c.alloc;
        d.logical += c.logical;
        d.file_count += c.file_count;
    }
    d.dirs = subdirs.into_iter().zip(children).collect();
    d
}

// ---------------------------------------------------------------------------
// 스캔 중 미리 보기

/// 미리 보기에 담을 최대 깊이 (그보다 깊은 내용은 이 깊이의 폴더 크기에 더한다)
pub const LIVE_DEPTH: u16 = 5;

#[derive(Default)]
pub struct LiveTree {
    pub dirs: Vec<LiveDir>,
    /// 바뀔 때마다 증가
    pub version: u64,
}

pub struct LiveDir {
    pub parent: u32,
    pub depth: u16,
    pub name: String,
    /// 지금까지 스캔한 하위 전체
    pub alloc: u64,
    pub logical: u64,
    pub files: u64,
    /// 이 폴더 바로 안의 파일
    pub direct_alloc: u64,
    pub direct_logical: u64,
    pub direct_files: u64,
}

impl LiveTree {
    /// 디렉터리 하나를 다 읽었을 때 호출. 이 디렉터리를 가리키는 인덱스를 돌려준다.
    fn add(&mut self, parent: u32, depth: u16, name: &str, d: &DirBuf) -> u32 {
        let idx = if depth <= LIVE_DEPTH {
            self.dirs.push(LiveDir {
                parent,
                depth,
                name: name.to_string(),
                alloc: 0,
                logical: 0,
                files: 0,
                direct_alloc: d.alloc,
                direct_logical: d.logical,
                direct_files: d.file_count,
            });
            (self.dirs.len() - 1) as u32
        } else {
            parent
        };
        let mut cur = idx;
        while cur != NO_NODE {
            let e = &mut self.dirs[cur as usize];
            e.alloc += d.alloc;
            e.logical += d.logical;
            e.files += d.file_count;
            cur = e.parent;
        }
        self.version += 1;
        idx
    }

    /// 지금까지의 결과를 트리로 만든다. 폴더마다 바로 안의 파일은 "파일 n개" 하나로 묶는다.
    pub fn snapshot(&self) -> Option<Tree> {
        let n = self.dirs.len();
        if n == 0 {
            return None;
        }
        let mut kids: Vec<Vec<u32>> = vec![Vec::new(); n];
        for (i, d) in self.dirs.iter().enumerate().skip(1) {
            kids[d.parent as usize].push(i as u32);
        }
        let mut tree = Tree::default();
        let root = &self.dirs[0];
        tree.names.push_str(&root.name);
        tree.nodes.push(Node {
            alloc: root.alloc,
            logical: root.logical,
            name_off: 0,
            parent: NO_NODE,
            first_child: 0,
            child_count: 0,
            files: clamp32(root.files),
            mtime: 0,
            name_len: root.name.len() as u16,
            kind: Kind::Dir,
            flags: 0,
        });
        let mut queue = VecDeque::from([(0u32, 0u32)]);
        let mut order: Vec<(u64, Option<u32>)> = Vec::new();
        while let Some((node_id, li)) = queue.pop_front() {
            let d = &self.dirs[li as usize];
            if d.depth >= LIVE_DEPTH {
                continue; // 더 깊은 내용은 크기에만 들어 있다
            }
            order.clear();
            order.extend(kids[li as usize].iter().map(|&c| (self.dirs[c as usize].alloc, Some(c))));
            if d.direct_files > 0 {
                order.push((d.direct_alloc, None));
            }
            order.sort_unstable_by(|a, b| b.0.cmp(&a.0));
            let first = tree.nodes.len() as u32;
            tree.nodes[node_id as usize].first_child = first;
            tree.nodes[node_id as usize].child_count = order.len() as u32;
            for &(_, c) in &order {
                let id = tree.nodes.len() as u32;
                let name_off = tree.names.len() as u32;
                let node = match c {
                    Some(c) => {
                        let e = &self.dirs[c as usize];
                        tree.names.push_str(&e.name);
                        queue.push_back((id, c));
                        Node {
                            alloc: e.alloc,
                            logical: e.logical,
                            name_off,
                            parent: node_id,
                            first_child: 0,
                            child_count: 0,
                            files: clamp32(e.files),
                            mtime: 0,
                            name_len: e.name.len() as u16,
                            kind: Kind::Dir,
                            flags: 0,
                        }
                    }
                    None => {
                        let label = format!("파일 {}개", crate::thousands(d.direct_files));
                        tree.names.push_str(&label);
                        Node {
                            alloc: d.direct_alloc,
                            logical: d.direct_logical,
                            name_off,
                            parent: node_id,
                            first_child: 0,
                            child_count: 0,
                            files: clamp32(d.direct_files),
                            mtime: 0,
                            name_len: label.len() as u16,
                            kind: Kind::File,
                            flags: flags::SYNTHETIC,
                        }
                    }
                };
                tree.nodes.push(node);
            }
        }
        tree.version = self.version;
        Some(tree)
    }
}

/// DirBuf 트리를 BFS로 아레나에 펼친다. 자식은 할당 크기 내림차순.
fn build_tree(root_name: &str, root: DirBuf) -> Tree {
    let mut tree = Tree::default();
    tree.nodes.reserve((root.file_count as usize).saturating_add(root.file_count as usize / 8));
    tree.names.push_str(root_name);
    tree.nodes.push(Node {
        alloc: root.alloc,
        logical: root.logical,
        name_off: 0,
        parent: NO_NODE,
        first_child: 0,
        child_count: 0,
        files: clamp32(root.file_count),
        mtime: 0,
        name_len: root_name.len() as u16,
        kind: Kind::Dir,
        flags: if root.denied { flags::DENIED } else { 0 },
    });

    let mut queue = VecDeque::new();
    queue.push_back((0u32, root));
    let mut order: Vec<(u64, u32)> = Vec::new(); // (크기, 인덱스: 파일은 그대로, 디렉터리는 files.len() + i)
    let mut dir_ids: Vec<u32> = Vec::new();

    while let Some((id, d)) = queue.pop_front() {
        let nf = d.files.len();
        order.clear();
        order.extend(d.files.iter().enumerate().map(|(i, e)| (e.alloc, i as u32)));
        order.extend(d.dirs.iter().enumerate().map(|(i, (_, b))| (b.alloc, (nf + i) as u32)));
        order.sort_unstable_by(|a, b| b.0.cmp(&a.0));

        let first = tree.nodes.len() as u32;
        tree.nodes[id as usize].first_child = first;
        tree.nodes[id as usize].child_count = order.len() as u32;
        dir_ids.clear();
        dir_ids.resize(d.dirs.len(), 0);

        for &(_, idx) in &order {
            let idx = idx as usize;
            let node_id = tree.nodes.len() as u32;
            let (e, buf) = if idx < nf {
                (&d.files[idx], None)
            } else {
                dir_ids[idx - nf] = node_id;
                let (e, b) = &d.dirs[idx - nf];
                (e, Some(b))
            };
            let name = &d.names[e.name_off as usize..][..e.name_len as usize];
            let name_off = tree.names.len() as u32;
            tree.names.push_str(name);
            let mut n = Node {
                alloc: e.alloc,
                logical: e.logical,
                name_off,
                parent: id,
                first_child: 0,
                child_count: 0,
                files: if e.kind == Kind::File { 1 } else { 0 },
                mtime: e.mtime,
                name_len: e.name_len,
                kind: e.kind,
                flags: e.flags,
            };
            if let Some(b) = buf {
                n.alloc = b.alloc;
                n.logical = b.logical;
                n.files = clamp32(b.file_count);
                if b.denied {
                    n.flags |= flags::DENIED;
                }
            }
            tree.nodes.push(n);
        }

        for (i, (_, b)) in d.dirs.into_iter().enumerate() {
            queue.push_back((dir_ids[i], b));
        }
    }
    tree.nodes.shrink_to_fit();
    tree.names.shrink_to_fit();
    tree
}

fn clamp32(v: u64) -> u32 {
    v.min(u32::MAX as u64) as u32
}

// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct Drive {
    /// `C:\`
    pub root: String,
    pub kind: u32,
    pub total: u64,
    pub free: u64,
}

impl Drive {
    pub fn kind_label(&self) -> &'static str {
        match self.kind {
            DRIVE_FIXED => "로컬 디스크",
            DRIVE_REMOVABLE => "이동식",
            DRIVE_REMOTE => "네트워크",
            _ => "기타",
        }
    }
}

/// 사용 가능한 드라이브 목록 (고정/이동식/네트워크)
pub fn list_drives() -> Vec<Drive> {
    let mask = unsafe { GetLogicalDrives() };
    (0..26u8)
        .filter(|i| mask & (1 << i) != 0)
        .filter_map(|i| {
            let root = format!("{}:\\", (b'A' + i) as char);
            let w = wide(&root);
            let kind = unsafe { GetDriveTypeW(w.as_ptr()) };
            if !matches!(kind, DRIVE_FIXED | DRIVE_REMOVABLE | DRIVE_REMOTE) {
                return None;
            }
            let (total, free) = disk_space(&root)?;
            Some(Drive { root, kind, total, free })
        })
        .collect()
}

/// (전체, 여유) 바이트
pub fn disk_space(path: &str) -> Option<(u64, u64)> {
    let w = wide(path);
    let (mut avail, mut total, mut free) = (0u64, 0u64, 0u64);
    let ok = unsafe { GetDiskFreeSpaceExW(w.as_ptr(), &mut avail, &mut total, &mut free) };
    (ok != 0).then_some((total, free))
}

/// (현재 working set, 최대 working set) 바이트
pub fn process_memory() -> (usize, usize) {
    let mut c = PROCESS_MEMORY_COUNTERS { cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32, ..Default::default() };
    unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) };
    (c.WorkingSetSize, c.PeakWorkingSetSize)
}
