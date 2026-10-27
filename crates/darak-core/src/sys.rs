//! 필요한 Win32 API만 직접 선언한다. (windows-sys의 feature 조합보다 단순하고 버전 변화에 강하다)
#![allow(non_snake_case, non_camel_case_types, clippy::upper_case_acronyms)]

use std::ffi::c_void;

pub type HANDLE = *mut c_void;
pub const INVALID_HANDLE_VALUE: HANDLE = -1isize as HANDLE;

pub const FILE_LIST_DIRECTORY: u32 = 0x0001;
pub const FILE_SHARE_ALL: u32 = 0x0001 | 0x0002 | 0x0004;
pub const OPEN_EXISTING: u32 = 3;
pub const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;

/// FILE_INFO_BY_HANDLE_CLASS::FileIdBothDirectoryInfo
pub const FILE_ID_BOTH_DIRECTORY_INFO: i32 = 10;
pub const ERROR_NO_MORE_FILES: i32 = 18;

pub const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0010;
pub const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;

pub const GENERIC_READ: u32 = 0x8000_0000;
pub const FILE_FLAG_NO_BUFFERING: u32 = 0x2000_0000;
pub const FILE_FLAG_SEQUENTIAL_SCAN: u32 = 0x0800_0000;
pub const FSCTL_GET_NTFS_VOLUME_DATA: u32 = 0x0009_0064;

pub const DRIVE_REMOVABLE: u32 = 2;
pub const DRIVE_FIXED: u32 = 3;
pub const DRIVE_REMOTE: u32 = 4;
/// SetErrorMode: 매체 없는 드라이브를 건드려도 '디스크 없음' 대화상자를 띄우지 않는다
pub const SEM_FAILCRITICALERRORS: u32 = 0x0001;

/// NUL을 포함한 UTF-16 경로 한도. 셸 API(휴지통 이동 등)는 이보다 긴 경로를 다루지 못한다
pub const MAX_PATH: usize = 260;

// 미리 정의된 레지스트리 루트. 32비트 값을 부호 확장해야 64비트 HKEY가 된다 (0x8000_0001 as isize는 틀린 값)
pub const HKEY_CURRENT_USER: HANDLE = 0x8000_0001u32 as i32 as isize as HANDLE;
pub const HKEY_LOCAL_MACHINE: HANDLE = 0x8000_0002u32 as i32 as isize as HANDLE;
/// RegGetValueW: REG_DWORD 값만 받는다
pub const RRF_RT_REG_DWORD: u32 = 0x0000_0010;

/// SHObjectProperties: 이름이 파일 시스템 경로
pub const SHOP_FILEPATH: u32 = 2;
/// SystemParametersInfoW: 창 안 애니메이션 효과가 켜져 있는지 (BOOL)
pub const SPI_GETCLIENTAREAANIMATION: u32 = 0x1042;

/// FILE_ID_BOTH_DIR_INFO (x64 기준 헤더 104바이트, 뒤에 FileName이 이어진다)
#[repr(C)]
pub struct FILE_ID_BOTH_DIR_INFO {
    pub NextEntryOffset: u32,
    pub FileIndex: u32,
    pub CreationTime: i64,
    pub LastAccessTime: i64,
    pub LastWriteTime: i64,
    pub ChangeTime: i64,
    pub EndOfFile: i64,
    pub AllocationSize: i64,
    pub FileAttributes: u32,
    pub FileNameLength: u32,
    pub EaSize: u32,
    pub ShortNameLength: i8,
    pub ShortName: [u16; 12],
    pub FileId: i64,
    pub FileName: [u16; 1],
}

const _: () = assert!(std::mem::offset_of!(FILE_ID_BOTH_DIR_INFO, FileId) == 96);
const _: () = assert!(std::mem::offset_of!(FILE_ID_BOTH_DIR_INFO, FileName) == 104);

#[repr(C)]
#[derive(Default)]
pub struct PROCESS_MEMORY_COUNTERS {
    pub cb: u32,
    pub PageFaultCount: u32,
    pub PeakWorkingSetSize: usize,
    pub WorkingSetSize: usize,
    pub QuotaPeakPagedPoolUsage: usize,
    pub QuotaPagedPoolUsage: usize,
    pub QuotaPeakNonPagedPoolUsage: usize,
    pub QuotaNonPagedPoolUsage: usize,
    pub PagefileUsage: usize,
    pub PeakPagefileUsage: usize,
}

#[repr(C)]
#[derive(Default, Debug, Clone, Copy)]
pub struct NTFS_VOLUME_DATA_BUFFER {
    pub VolumeSerialNumber: i64,
    pub NumberSectors: i64,
    pub TotalClusters: i64,
    pub FreeClusters: i64,
    pub TotalReserved: i64,
    pub BytesPerSector: u32,
    pub BytesPerCluster: u32,
    pub BytesPerFileRecordSegment: u32,
    pub ClustersPerFileRecordSegment: u32,
    pub MftValidDataLength: i64,
    pub MftStartLcn: i64,
    pub Mft2StartLcn: i64,
    pub MftZoneStart: i64,
    pub MftZoneEnd: i64,
}

#[repr(C)]
pub struct OVERLAPPED {
    pub Internal: usize,
    pub InternalHigh: usize,
    pub Offset: u32,
    pub OffsetHigh: u32,
    pub hEvent: HANDLE,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct SYSTEMTIME {
    pub wYear: u16,
    pub wMonth: u16,
    pub wDayOfWeek: u16,
    pub wDay: u16,
    pub wHour: u16,
    pub wMinute: u16,
    pub wSecond: u16,
    pub wMilliseconds: u16,
}

#[link(name = "kernel32")]
unsafe extern "system" {
    pub fn CreateFileW(
        name: *const u16,
        access: u32,
        share: u32,
        security: *const c_void,
        disposition: u32,
        flags: u32,
        template: HANDLE,
    ) -> HANDLE;
    pub fn GetFileInformationByHandleEx(h: HANDLE, class: i32, buf: *mut c_void, size: u32) -> i32;
    pub fn CloseHandle(h: HANDLE) -> i32;
    pub fn GetLogicalDrives() -> u32;
    pub fn GetDriveTypeW(root: *const u16) -> u32;
    pub fn GetDiskFreeSpaceExW(
        dir: *const u16,
        free_to_caller: *mut u64,
        total: *mut u64,
        total_free: *mut u64,
    ) -> i32;
    pub fn GetCurrentProcess() -> HANDLE;
    pub fn FileTimeToSystemTime(ft: *const i64, st: *mut SYSTEMTIME) -> i32;
    pub fn SystemTimeToTzSpecificLocalTime(tz: *const c_void, utc: *const SYSTEMTIME, local: *mut SYSTEMTIME) -> i32;
    pub fn DeviceIoControl(
        h: HANDLE,
        code: u32,
        in_buf: *const c_void,
        in_size: u32,
        out_buf: *mut c_void,
        out_size: u32,
        returned: *mut u32,
        overlapped: *mut OVERLAPPED,
    ) -> i32;
    pub fn ReadFile(h: HANDLE, buf: *mut c_void, len: u32, read: *mut u32, overlapped: *mut OVERLAPPED) -> i32;
    pub fn K32GetProcessMemoryInfo(p: HANDLE, c: *mut PROCESS_MEMORY_COUNTERS, cb: u32) -> i32;
    pub fn GetVolumePathNameW(path: *const u16, out: *mut u16, len: u32) -> i32;
    pub fn GetVolumeNameForVolumeMountPointW(mount: *const u16, out: *mut u16, len: u32) -> i32;
    pub fn GetVolumeInformationW(
        root: *const u16,
        volume_name: *mut u16,
        volume_name_size: u32,
        serial: *mut u32,
        max_component: *mut u32,
        fs_flags: *mut u32,
        fs_name: *mut u16,
        fs_name_size: u32,
    ) -> i32;
    pub fn GetSystemWindowsDirectoryW(buf: *mut u16, size: u32) -> u32;
    pub fn GetErrorMode() -> u32;
    pub fn SetErrorMode(mode: u32) -> u32;
}

#[link(name = "advapi32")]
unsafe extern "system" {
    pub fn RegGetValueW(
        hkey: HANDLE,
        sub: *const u16,
        value: *const u16,
        flags: u32,
        ty: *mut u32,
        data: *mut c_void,
        len: *mut u32,
    ) -> i32;
}

#[link(name = "user32")]
unsafe extern "system" {
    pub fn GetActiveWindow() -> HANDLE;
    pub fn SystemParametersInfoW(action: u32, param: u32, pv: *mut c_void, ini: u32) -> i32;
}

#[link(name = "shell32")]
unsafe extern "system" {
    pub fn IsUserAnAdmin() -> i32;
    pub fn ShellExecuteW(
        hwnd: HANDLE,
        op: *const u16,
        file: *const u16,
        params: *const u16,
        dir: *const u16,
        show: i32,
    ) -> HANDLE;
    pub fn SHObjectProperties(hwnd: HANDLE, kind: u32, name: *const u16, page: *const u16) -> i32;
}

/// 닫힐 때 CloseHandle을 호출하는 핸들
pub struct OwnedHandle(pub HANDLE);

// 커널 핸들은 스레드 간에 공유해도 안전하다 (읽기는 OVERLAPPED로 오프셋을 명시)
unsafe impl Send for OwnedHandle {}
unsafe impl Sync for OwnedHandle {}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

/// &str → NUL로 끝나는 UTF-16
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}
