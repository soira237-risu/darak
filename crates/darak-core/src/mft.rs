//! 빠른 경로 스캐너: NTFS 볼륨의 `$MFT`를 직접 순차로 읽어 트리를 재구성한다. (관리자 권한 필요)
//!
//! 1. `FSCTL_GET_NTFS_VOLUME_DATA`로 클러스터/레코드 크기와 `$MFT` 위치를 얻는다.
//! 2. `$MFT`의 레코드 0(자기 자신)에서 `$DATA`의 data run을 읽는다.
//! 3. 리더 스레드가 data run을 따라 큰 덩어리로 읽고, rayon이 레코드를 병렬로 파싱한다.
//! 4. 레코드 번호로 인덱싱된 배열에 모은 뒤 부모 참조로 트리를 만든다.

use std::io;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::mpsc;
use std::time::Instant;

use rayon::prelude::*;

use crate::scan::{ScanMethod, ScanProgress, ScanStats};
use crate::sys::*;
use crate::tree::{Kind, NO_NODE, Node, Tree, filetime_to_secs2000, flags};

const ROOT_RECORD: u32 = 5;
const CHUNK_BYTES: usize = 8 << 20;
const BATCH_RECORDS: usize = 2048;

const ATTR_STANDARD_INFORMATION: u32 = 0x10;
const ATTR_FILE_NAME: u32 = 0x30;
const ATTR_DATA: u32 = 0x80;
const ATTR_END: u32 = 0xFFFF_FFFF;

const RECORD_IN_USE: u16 = 0x01;
const RECORD_IS_DIR: u16 = 0x02;

/// `root`가 `X:\` 형태의 NTFS 볼륨 루트일 때만 쓴다.
pub fn scan_volume(root: &str, progress: &ScanProgress) -> io::Result<(Tree, ScanStats)> {
    let start = Instant::now();
    let letter = root.chars().next().filter(|c| c.is_ascii_alphabetic()).ok_or_else(|| invalid("드라이브 루트가 아님"))?;
    let vol = open_volume(letter)?;
    let vd = volume_data(&vol)?;
    let rec_size = vd.BytesPerFileRecordSegment as usize;
    let cluster = vd.BytesPerCluster as u64;
    if !(512..=65536).contains(&rec_size) || !rec_size.is_multiple_of(512) || cluster == 0 {
        return Err(invalid("지원하지 않는 NTFS 레코드 크기"));
    }

    // 레코드 0 = $MFT 자신. 클러스터 단위로 읽어 정렬 조건을 맞춘다.
    let mut first = AlignedBuf::new(rec_size.max(cluster as usize));
    read_at(&vol, vd.MftStartLcn as u64 * cluster, first.as_mut())?;
    let rec0 = &mut first.as_mut()[..rec_size];
    if !apply_fixup(rec0) {
        return Err(invalid("$MFT 레코드 0 손상"));
    }
    let runs = mft_runs(rec0).ok_or_else(|| invalid("$MFT data run을 찾지 못함 (조각이 너무 많을 수 있음)"))?;
    let mft_bytes = vd.MftValidDataLength as u64;
    let total_records = (mft_bytes / rec_size as u64) as usize;
    let run_bytes: u64 = runs.iter().map(|r| r.1 * cluster).sum();
    if run_bytes < mft_bytes {
        return Err(invalid("$MFT가 여러 레코드에 걸쳐 있음 (attribute list 미지원)"));
    }

    let mut recs = Records::new(total_records);
    progress.total_hint.store(total_records as u64, Relaxed);
    *progress.current.lock().unwrap() = format!("{root} $MFT 읽는 중 ({} 레코드)", crate::thousands(total_records as u64));

    // 리더 스레드: data run을 따라 CHUNK_BYTES씩 읽어 보낸다
    let (tx, rx) = mpsc::sync_channel::<io::Result<(usize, AlignedBuf, usize)>>(3);
    let read_err = std::thread::scope(|s| {
        s.spawn(|| {
            let tx = tx; // 송신자를 이 스레드가 소유해야 끝날 때 채널이 닫힌다
            let mut rec_no = 0usize;
            let mut remaining = mft_bytes;
            'runs: for &(lcn, clusters) in &runs {
                let mut off = lcn * cluster;
                let mut left = (clusters * cluster).min(remaining);
                while left > 0 {
                    if progress.is_cancelled() {
                        break 'runs;
                    }
                    let want = (left as usize).min(CHUNK_BYTES);
                    let aligned = want.next_multiple_of(cluster as usize);
                    let mut buf = AlignedBuf::new(aligned);
                    if let Err(e) = read_at(&vol, off, buf.as_mut()) {
                        let _ = tx.send(Err(e));
                        break 'runs;
                    }
                    let n = want / rec_size;
                    if tx.send(Ok((rec_no, buf, n))).is_err() {
                        break 'runs;
                    }
                    rec_no += n;
                    off += want as u64;
                    left -= want as u64;
                    remaining -= want as u64;
                }
            }
        });

        // 받는 쪽: 덩어리를 배치로 나눠 병렬 파싱 후 순서대로 병합
        for msg in rx {
            let (first_rec, mut buf, n) = match msg {
                Ok(m) => m,
                Err(e) => return Some(e),
            };
            let data = &mut buf.as_mut()[..n * rec_size];
            let batches: Vec<Batch> = data
                .par_chunks_mut(rec_size * BATCH_RECORDS)
                .enumerate()
                .map(|(i, chunk)| parse_batch(chunk, rec_size, (first_rec + i * BATCH_RECORDS) as u32, cluster))
                .collect();
            for b in batches {
                recs.merge(b);
            }
            progress.files.store(recs.files, Relaxed);
            progress.dirs.store(recs.dirs, Relaxed);
            progress.bytes.store(recs.bytes, Relaxed);
            progress.done_hint.store((first_rec + n) as u64, Relaxed);
        }
        None
    });
    if let Some(e) = read_err {
        return Err(e);
    }

    *progress.current.lock().unwrap() = format!("{root} 트리 구성 중");
    let display = format!("{}:\\", letter.to_ascii_uppercase());
    let (tree, orphans, dups) = recs.build_tree(&display);
    let root_node = tree.node(Tree::ROOT);
    let stats = ScanStats {
        elapsed: start.elapsed(),
        files: root_node.files as u64,
        dirs: tree.nodes.iter().filter(|n| n.kind == Kind::Dir).count() as u64,
        errors: orphans,
        hardlink_dups: dups,
        cancelled: progress.is_cancelled(),
        method: ScanMethod::Mft,
        fallback_reason: None,
    };
    Ok((tree, stats))
}

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

fn open_volume(letter: char) -> io::Result<OwnedHandle> {
    let path = wide(&format!(r"\\.\{letter}:"));
    let h = unsafe {
        CreateFileW(
            path.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_ALL,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_NO_BUFFERING | FILE_FLAG_SEQUENTIAL_SCAN,
            std::ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE { Err(io::Error::last_os_error()) } else { Ok(OwnedHandle(h)) }
}

fn volume_data(vol: &OwnedHandle) -> io::Result<NTFS_VOLUME_DATA_BUFFER> {
    let mut vd = NTFS_VOLUME_DATA_BUFFER::default();
    let mut ret = 0u32;
    let ok = unsafe {
        DeviceIoControl(
            vol.0,
            FSCTL_GET_NTFS_VOLUME_DATA,
            std::ptr::null(),
            0,
            (&mut vd as *mut NTFS_VOLUME_DATA_BUFFER).cast(),
            std::mem::size_of::<NTFS_VOLUME_DATA_BUFFER>() as u32,
            &mut ret,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 { Err(io::Error::last_os_error()) } else { Ok(vd) }
}

fn read_at(vol: &OwnedHandle, offset: u64, buf: &mut [u8]) -> io::Result<()> {
    let mut done = 0usize;
    while done < buf.len() {
        let pos = offset + done as u64;
        let mut ov = OVERLAPPED {
            Internal: 0,
            InternalHigh: 0,
            Offset: pos as u32,
            OffsetHigh: (pos >> 32) as u32,
            hEvent: std::ptr::null_mut(),
        };
        let want = (buf.len() - done).min(64 << 20) as u32;
        let mut read = 0u32;
        let ok = unsafe { ReadFile(vol.0, buf[done..].as_mut_ptr().cast(), want, &mut read, &mut ov) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        if read == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "볼륨 읽기가 끝에 도달"));
        }
        done += read as usize;
    }
    Ok(())
}

/// FILE_FLAG_NO_BUFFERING용 4KB 정렬 버퍼
struct AlignedBuf {
    ptr: *mut u8,
    len: usize,
}

unsafe impl Send for AlignedBuf {}

impl AlignedBuf {
    fn layout(len: usize) -> std::alloc::Layout {
        std::alloc::Layout::from_size_align(len.max(4096), 4096).unwrap()
    }
    fn new(len: usize) -> Self {
        let ptr = unsafe { std::alloc::alloc_zeroed(Self::layout(len)) };
        if ptr.is_null() {
            std::alloc::handle_alloc_error(Self::layout(len));
        }
        Self { ptr, len }
    }
    fn as_mut(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        unsafe { std::alloc::dealloc(self.ptr, Self::layout(self.len)) };
    }
}

// ---------------------------------------------------------------------------
// 레코드 파싱

fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn u64_at(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

/// update sequence(fixup)를 되돌린다. 512바이트마다 마지막 2바이트가 USN과 같아야 한다.
fn apply_fixup(rec: &mut [u8]) -> bool {
    if rec.len() < 48 || &rec[0..4] != b"FILE" {
        return false;
    }
    let usa_off = u16_at(rec, 4) as usize;
    let usa_count = u16_at(rec, 6) as usize;
    if usa_count == 0 || usa_off + usa_count * 2 > rec.len() || (usa_count - 1) * 512 > rec.len() {
        return false;
    }
    let usn = [rec[usa_off], rec[usa_off + 1]];
    for i in 1..usa_count {
        let end = i * 512 - 2;
        if rec[end..end + 2] != usn {
            return false;
        }
        rec[end] = rec[usa_off + i * 2];
        rec[end + 1] = rec[usa_off + i * 2 + 1];
    }
    true
}

/// 레코드의 속성들: (type, 속성 전체 바이트)
fn attributes(rec: &[u8]) -> impl Iterator<Item = (u32, &[u8])> {
    let used = (u32_at(rec, 0x18) as usize).min(rec.len());
    let mut off = u16_at(rec, 0x14) as usize;
    std::iter::from_fn(move || {
        if off + 16 > used {
            return None;
        }
        let ty = u32_at(rec, off);
        if ty == ATTR_END {
            return None;
        }
        let len = u32_at(rec, off + 4) as usize;
        if len < 16 || off + len > used {
            return None;
        }
        let a = &rec[off..off + len];
        off += len;
        Some((ty, a))
    })
}

fn resident_value(a: &[u8]) -> Option<&[u8]> {
    if a[8] != 0 || a.len() < 0x18 {
        return None;
    }
    let len = u32_at(a, 0x10) as usize;
    let off = u16_at(a, 0x14) as usize;
    a.get(off..off + len)
}

/// 비상주 속성의 data run을 푼다: (LCN, 클러스터 수). sparse run은 LCN이 None.
fn data_runs(a: &[u8]) -> impl Iterator<Item = Option<(Option<u64>, u64)>> + '_ {
    let mut off = u16_at(a, 0x20) as usize;
    let mut lcn: i64 = 0;
    let mut broken = false;
    std::iter::from_fn(move || {
        if broken || off >= a.len() || a[off] == 0 {
            return None;
        }
        let h = a[off];
        let (ln, on) = ((h & 0x0F) as usize, (h >> 4) as usize);
        off += 1;
        if ln == 0 || ln > 8 || on > 8 || off + ln + on > a.len() {
            broken = true;
            return Some(None);
        }
        let mut len = 0u64;
        for i in 0..ln {
            len |= (a[off + i] as u64) << (8 * i);
        }
        off += ln;
        if on == 0 {
            return Some(Some((None, len)));
        }
        let mut delta = 0i64;
        for i in 0..on {
            delta |= (a[off + i] as i64) << (8 * i);
        }
        let shift = 64 - 8 * on as u32;
        delta = (delta << shift) >> shift; // 부호 확장
        off += on;
        lcn += delta;
        Some(Some((Some(lcn as u64), len)))
    })
}

/// sparse가 아닌 run의 클러스터 합
fn allocated_clusters(a: &[u8]) -> u64 {
    data_runs(a).map_while(|r| r).filter(|(lcn, _)| lcn.is_some()).map(|(_, len)| len).sum()
}

/// 레코드 0의 이름 없는 $DATA에서 (LCN, 클러스터 수) 목록
fn mft_runs(rec0: &[u8]) -> Option<Vec<(u64, u64)>> {
    let (_, a) = attributes(rec0).find(|(ty, a)| *ty == ATTR_DATA && a[9] == 0 && a[8] != 0)?;
    let mut runs = Vec::new();
    for r in data_runs(a) {
        match r? {
            (Some(lcn), len) => runs.push((lcn, len)),
            (None, _) => return None, // $MFT에 sparse run은 없어야 한다
        }
    }
    (!runs.is_empty()).then_some(runs)
}

#[derive(Clone, Copy)]
struct Rec {
    record: u32,
    seq: u16,
    is_dir: bool,
    attrs: u32,
    mtime: u32,
}

#[derive(Clone, Copy)]
struct NameFact {
    /// 이 이름이 속한 (기본) 레코드
    record: u32,
    parent: u32,
    parent_seq: u16,
    name_off: u32,
    name_len: u16,
}

#[derive(Clone, Copy)]
struct SizeFact {
    record: u32,
    alloc: u64,
    logical: u64,
}

#[derive(Default)]
struct Batch {
    names: String,
    recs: Vec<Rec>,
    name_facts: Vec<NameFact>,
    size_facts: Vec<SizeFact>,
}

fn parse_batch(chunk: &mut [u8], rec_size: usize, first_rec: u32, cluster: u64) -> Batch {
    let mut b = Batch::default();
    for (i, rec) in chunk.chunks_exact_mut(rec_size).enumerate() {
        let rec_no = first_rec + i as u32;
        if !apply_fixup(rec) {
            continue;
        }
        let fl = u16_at(rec, 0x16);
        if fl & RECORD_IN_USE == 0 {
            continue;
        }
        let base_ref = u64_at(rec, 0x20) & 0x0000_FFFF_FFFF_FFFF;
        let owner = if base_ref == 0 { rec_no } else { base_ref as u32 };
        let mut info = Rec { record: rec_no, seq: u16_at(rec, 0x10), is_dir: fl & RECORD_IS_DIR != 0, attrs: 0, mtime: 0 };

        for (ty, a) in attributes(rec) {
            match ty {
                ATTR_STANDARD_INFORMATION => {
                    if let Some(v) = resident_value(a).filter(|v| v.len() >= 0x24) {
                        info.mtime = filetime_to_secs2000(u64_at(v, 0x08) as i64);
                        info.attrs = u32_at(v, 0x20);
                    }
                }
                ATTR_FILE_NAME => {
                    let Some(v) = resident_value(a).filter(|v| v.len() >= 0x42) else { continue };
                    let namespace = v[0x41];
                    if namespace == 2 {
                        continue; // DOS 8.3 이름
                    }
                    let n = v[0x40] as usize;
                    let Some(raw) = v.get(0x42..0x42 + n * 2) else { continue };
                    let parent_ref = u64_at(v, 0);
                    let name_off = b.names.len() as u32;
                    let units = raw.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes([c[0], c[1]]));
                    for c in char::decode_utf16(units) {
                        b.names.push(c.unwrap_or(char::REPLACEMENT_CHARACTER));
                    }
                    b.name_facts.push(NameFact {
                        record: owner,
                        parent: (parent_ref & 0x0000_FFFF_FFFF_FFFF) as u32,
                        parent_seq: (parent_ref >> 48) as u16,
                        name_off,
                        name_len: (b.names.len() as u32 - name_off) as u16,
                    });
                }
                ATTR_DATA => {
                    let named = a[9] != 0;
                    if a[8] == 0 {
                        // 상주(resident): 내용이 MFT 레코드 안에 있으므로 클러스터를 따로 쓰지 않는다
                        if !named {
                            let len = u32_at(a, 0x10) as u64;
                            b.size_facts.push(SizeFact { record: owner, alloc: 0, logical: len });
                        }
                    } else if a.len() >= 0x40 {
                        // 비상주: 할당은 모든 조각의 data run에서 실제 클러스터를 더하고(sparse 제외),
                        // 논리 크기는 첫 조각(VCN 0)에만 있다
                        let alloc = allocated_clusters(a) * cluster;
                        let logical = if !named && u64_at(a, 0x10) == 0 { u64_at(a, 0x30) } else { 0 };
                        b.size_facts.push(SizeFact { record: owner, alloc, logical });
                    }
                }
                _ => {}
            }
        }
        if base_ref == 0 {
            b.recs.push(info);
        }
    }
    b
}

// ---------------------------------------------------------------------------
// 병합과 트리 구성

const NO_NAME: u32 = u32::MAX;

struct Records {
    in_use: Vec<bool>,
    seq: Vec<u16>,
    is_dir: Vec<bool>,
    attrs: Vec<u32>,
    mtime: Vec<u32>,
    alloc: Vec<u64>,
    logical: Vec<u64>,
    parent: Vec<u32>,
    parent_seq: Vec<u16>,
    name_off: Vec<u32>,
    name_len: Vec<u16>,
    names: String,
    /// 하드링크의 추가 이름들
    links: Vec<NameFact>,
    files: u64,
    dirs: u64,
    bytes: u64,
}

impl Records {
    fn new(n: usize) -> Self {
        Self {
            in_use: vec![false; n],
            seq: vec![0; n],
            is_dir: vec![false; n],
            attrs: vec![0; n],
            mtime: vec![0; n],
            alloc: vec![0; n],
            logical: vec![0; n],
            parent: vec![NO_NODE; n],
            parent_seq: vec![0; n],
            name_off: vec![NO_NAME; n],
            name_len: vec![0; n],
            names: String::new(),
            links: Vec::new(),
            files: 0,
            dirs: 0,
            bytes: 0,
        }
    }

    fn merge(&mut self, b: Batch) {
        let n = self.in_use.len();
        for r in &b.recs {
            let i = r.record as usize;
            if i >= n {
                continue;
            }
            self.in_use[i] = true;
            self.seq[i] = r.seq;
            self.is_dir[i] = r.is_dir;
            self.attrs[i] = r.attrs;
            self.mtime[i] = r.mtime;
            if r.is_dir { self.dirs += 1 } else { self.files += 1 }
        }
        for s in &b.size_facts {
            let i = s.record as usize;
            if i < n {
                self.alloc[i] += s.alloc;
                self.logical[i] += s.logical;
                self.bytes += s.alloc;
            }
        }
        for f in &b.name_facts {
            let i = f.record as usize;
            if i >= n {
                continue;
            }
            let name = &b.names[f.name_off as usize..][..f.name_len as usize];
            let off = self.names.len() as u32;
            self.names.push_str(name);
            if self.name_off[i] == NO_NAME {
                self.name_off[i] = off;
                self.name_len[i] = f.name_len;
                self.parent[i] = f.parent;
                self.parent_seq[i] = f.parent_seq;
            } else if !(self.parent[i] == f.parent && &self.names[self.name_off[i] as usize..][..self.name_len[i] as usize] == name) {
                self.links.push(NameFact { name_off: off, ..*f });
            }
        }
    }

    fn valid_parent(&self, parent: u32, parent_seq: u16) -> bool {
        let p = parent as usize;
        p < self.in_use.len() && self.in_use[p] && self.is_dir[p] && self.seq[p] == parent_seq
    }

    /// (트리, 고아 수, 하드링크 중복 수)
    fn build_tree(mut self, root_name: &str) -> (Tree, u64, u64) {
        let n = self.in_use.len();
        // 항목: 기본 이름 + 하드링크 추가 이름. (레코드, 부모, 이름 위치, 하드링크 중복 여부)
        struct Entry {
            record: u32,
            name_off: u32,
            name_len: u16,
            dup: bool,
        }
        let mut entries: Vec<Entry> = Vec::with_capacity(n);
        let mut entry_parent: Vec<u32> = Vec::with_capacity(n);
        let mut orphans = 0u64;
        for i in 0..n {
            if !self.in_use[i] || i as u32 == ROOT_RECORD || self.name_off[i] == NO_NAME {
                continue;
            }
            if !self.valid_parent(self.parent[i], self.parent_seq[i]) {
                orphans += 1;
                continue;
            }
            entries.push(Entry { record: i as u32, name_off: self.name_off[i], name_len: self.name_len[i], dup: false });
            entry_parent.push(self.parent[i]);
        }
        let mut dups = 0u64;
        for l in std::mem::take(&mut self.links) {
            if self.in_use[l.record as usize] && self.valid_parent(l.parent, l.parent_seq) && !self.is_dir[l.record as usize] {
                entries.push(Entry { record: l.record, name_off: l.name_off, name_len: l.name_len, dup: true });
                entry_parent.push(l.parent);
                dups += 1;
            }
        }

        // 디렉터리 레코드별 자식 항목 (CSR)
        let mut start = vec![0u32; n + 1];
        for &p in &entry_parent {
            start[p as usize + 1] += 1;
        }
        for i in 0..n {
            start[i + 1] += start[i];
        }
        let mut fill = start.clone();
        let mut kids = vec![0u32; entries.len()];
        for (e, &p) in entry_parent.iter().enumerate() {
            kids[fill[p as usize] as usize] = e as u32;
            fill[p as usize] += 1;
        }
        drop(fill);
        drop(entry_parent);

        // 후위 순회로 디렉터리 크기 합산. 디렉터리는 항목이 하나뿐이므로 레코드 배열을 그대로 쓴다.
        let mut files = vec![0u32; n];
        for e in &entries {
            let r = e.record as usize;
            if self.is_dir[r] {
                self.alloc[r] = 0;
                self.logical[r] = 0;
            }
        }
        let mut visited = vec![false; n];
        let mut stack: Vec<(u32, u32)> = vec![(ROOT_RECORD, start[ROOT_RECORD as usize])];
        visited[ROOT_RECORD as usize] = true;
        self.alloc[ROOT_RECORD as usize] = 0;
        self.logical[ROOT_RECORD as usize] = 0;
        while let Some(&mut (dir, ref mut pos)) = stack.last_mut() {
            if *pos < start[dir as usize + 1] {
                let e = &entries[kids[*pos as usize] as usize];
                *pos += 1;
                let r = e.record as usize;
                if self.is_dir[r] && !e.dup && !visited[r] {
                    visited[r] = true;
                    stack.push((e.record, start[r]));
                }
            } else {
                stack.pop();
                let d = dir as usize;
                let (mut a, mut l, mut f) = (0u64, 0u64, 0u64);
                for k in start[d]..start[d + 1] {
                    let e = &entries[kids[k as usize] as usize];
                    let r = e.record as usize;
                    if e.dup {
                        continue;
                    }
                    if self.is_dir[r] {
                        a += self.alloc[r];
                        l += self.logical[r];
                        f += files[r] as u64;
                    } else {
                        a += self.alloc[r];
                        l += self.logical[r];
                        f += 1;
                    }
                }
                self.alloc[d] = a;
                self.logical[d] = l;
                files[d] = f.min(u32::MAX as u64) as u32;
            }
        }

        // BFS로 아레나에 펼치기 (자식은 할당 크기 내림차순)
        let mut tree = Tree::default();
        tree.nodes.reserve(entries.len() + 1);
        tree.names.reserve(self.names.len());
        tree.names.push_str(root_name);
        let root = ROOT_RECORD as usize;
        tree.nodes.push(Node {
            alloc: self.alloc[root],
            logical: self.logical[root],
            name_off: 0,
            parent: NO_NODE,
            first_child: 0,
            child_count: 0,
            files: files[root],
            mtime: self.mtime[root],
            name_len: root_name.len() as u16,
            kind: Kind::Dir,
            flags: 0,
        });
        let mut queue = std::collections::VecDeque::new();
        queue.push_back((0u32, ROOT_RECORD));
        let mut order: Vec<(u64, u32)> = Vec::new();
        let mut emitted = vec![false; n];
        emitted[root] = true;
        while let Some((node_id, dir)) = queue.pop_front() {
            let d = dir as usize;
            order.clear();
            for k in start[d]..start[d + 1] {
                let ei = kids[k as usize];
                let e = &entries[ei as usize];
                let size = if e.dup { 0 } else { self.alloc[e.record as usize] };
                order.push((size, ei));
            }
            order.sort_unstable_by(|a, b| b.0.cmp(&a.0));
            let first = tree.nodes.len() as u32;
            tree.nodes[node_id as usize].first_child = first;
            tree.nodes[node_id as usize].child_count = order.len() as u32;
            for &(_, ei) in &order {
                let e = &entries[ei as usize];
                let r = e.record as usize;
                let id = tree.nodes.len() as u32;
                let reparse = self.attrs[r] & FILE_ATTRIBUTE_REPARSE_POINT != 0;
                let is_dir = self.is_dir[r];
                let kind = if is_dir && reparse {
                    Kind::Link
                } else if is_dir {
                    Kind::Dir
                } else {
                    Kind::File
                };
                let mut fl = 0u8;
                if !is_dir && reparse {
                    fl |= flags::REPARSE;
                }
                if e.dup {
                    fl |= flags::HARDLINK_DUP;
                }
                let name = &self.names[e.name_off as usize..][..e.name_len as usize];
                let name_off = tree.names.len() as u32;
                tree.names.push_str(name);
                let (alloc, logical, nfiles) = if e.dup {
                    (0, 0, 0)
                } else if is_dir {
                    (self.alloc[r], self.logical[r], files[r])
                } else {
                    (self.alloc[r], self.logical[r], 1)
                };
                tree.nodes.push(Node {
                    alloc,
                    logical,
                    name_off,
                    parent: node_id,
                    first_child: 0,
                    child_count: 0,
                    files: nfiles,
                    mtime: self.mtime[r],
                    name_len: e.name_len,
                    kind,
                    flags: fl,
                });
                if kind == Kind::Dir && !emitted[r] {
                    emitted[r] = true;
                    queue.push_back((id, e.record));
                }
            }
        }
        tree.nodes.shrink_to_fit();
        tree.names.shrink_to_fit();
        (tree, orphans, dups)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_runs_with_sparse() {
        // 비상주 속성 헤더(0x40바이트) 뒤에 data run: [len 0x10 @ lcn 0x100], [sparse 0x20], [len 0x08 @ -0x10]
        let mut a = vec![0u8; 0x40];
        a[8] = 1;
        a[0x20..0x22].copy_from_slice(&0x40u16.to_le_bytes());
        a.extend_from_slice(&[0x21, 0x10, 0x00, 0x01]);
        a.extend_from_slice(&[0x01, 0x20]);
        a.extend_from_slice(&[0x11, 0x08, 0xF0]);
        a.push(0);
        let runs: Vec<_> = data_runs(&a).map(|r| r.unwrap()).collect();
        assert_eq!(runs, vec![(Some(0x100), 0x10), (None, 0x20), (Some(0xF0), 0x08)]);
        assert_eq!(allocated_clusters(&a), 0x18);
    }

    #[test]
    fn fixup_roundtrip() {
        let mut rec = vec![0u8; 1024];
        rec[0..4].copy_from_slice(b"FILE");
        rec[4..6].copy_from_slice(&0x30u16.to_le_bytes());
        rec[6..8].copy_from_slice(&3u16.to_le_bytes());
        rec[0x30..0x32].copy_from_slice(&[0xAB, 0xCD]); // USN
        rec[0x32..0x34].copy_from_slice(&[1, 2]); // 섹터 1 원래 값
        rec[0x34..0x36].copy_from_slice(&[3, 4]); // 섹터 2 원래 값
        rec[510..512].copy_from_slice(&[0xAB, 0xCD]);
        rec[1022..1024].copy_from_slice(&[0xAB, 0xCD]);
        assert!(apply_fixup(&mut rec));
        assert_eq!(&rec[510..512], &[1, 2]);
        assert_eq!(&rec[1022..1024], &[3, 4]);
        // USN이 맞지 않으면 실패
        rec[510..512].copy_from_slice(&[0, 0]);
        rec[0x30..0x32].copy_from_slice(&[0xAB, 0xCD]);
        assert!(!apply_fixup(&mut rec));
    }
}
