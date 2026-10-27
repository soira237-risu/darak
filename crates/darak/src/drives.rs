//! 드라이브 여유 공간과 파일 시스템. GetDiskFreeSpaceExW는 끊긴 네트워크 드라이브에서 수십 초 막힐 수 있어
//! UI 스레드에서 부르지 않는다. 경로마다 조회 스레드를 하나만 띄우고, 답이 오기 전에는 이전 값을 보여 준다.

use std::collections::HashMap;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use darak_core::scan::{disk_space, is_drive_root, plausible_space, volume_fs};
use eframe::egui;

/// 한 경로의 용량
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Space {
    /// 처음 묻는 중 (아직 답이 없음)
    Checking,
    /// (전체, 여유) 바이트
    Known(u64, u64),
    /// 답은 왔지만 믿을 수 없는 값 (클라우드·가상 드라이브의 2^63 B 같은 표지값)
    Unknown,
    /// 조회 실패 (매체 없음, 끊긴 네트워크 등)
    Unavailable,
}

/// 조회 한 번의 답
#[derive(Clone, Debug, Default)]
pub struct Probe {
    /// (전체, 여유) 바이트, 검사하지 않은 원래 값
    pub space: Option<(u64, u64)>,
    /// 파일 시스템 이름 (드라이브 루트만 묻는다)
    pub fs: Option<String>,
}

/// 경로 하나를 장치에 묻는 함수. 조회 스레드에서 부른다 (테스트는 가짜를 넘긴다)
pub type ProbeFn = Arc<dyn Fn(&str) -> Probe + Send + Sync>;

struct Entry {
    space: Space,
    fs: Option<String>,
    /// 마지막 답을 받은 시각, 조회 중이면 시작한 시각 (None이면 다음에 바로 묻는다)
    asked: Option<Instant>,
    /// 조회 스레드가 아직 답하지 않음
    busy: bool,
}

/// 경로별 용량·파일 시스템 캐시
pub struct Volumes {
    entries: HashMap<String, Entry>,
    tx: mpsc::Sender<(String, Probe)>,
    rx: mpsc::Receiver<(String, Probe)>,
    probe: ProbeFn,
    /// 답이 오면 다시 그리게 깨운다
    waker: Option<egui::Context>,
}

/// 같은 경로를 다시 묻기까지의 간격
pub const SPACE_TTL: Duration = Duration::from_secs(10);

impl Volumes {
    pub fn new(waker: Option<egui::Context>) -> Self {
        Self::with_probe(waker, Arc::new(probe))
    }

    pub fn with_probe(waker: Option<egui::Context>, probe: ProbeFn) -> Self {
        let (tx, rx) = mpsc::channel();
        Self { entries: HashMap::new(), tx, rx, probe, waker }
    }

    /// 조회 스레드의 답을 받는다 (프레임마다 한 번)
    pub fn poll(&mut self) {
        while let Ok((path, p)) = self.rx.try_recv() {
            self.apply(&path, p);
        }
    }

    fn apply(&mut self, path: &str, p: Probe) {
        let Some(e) = self.entries.get_mut(path) else { return };
        e.busy = false;
        // TTL은 답이 온 때부터 센다 (끊긴 네트워크 드라이브는 조회가 TTL보다 오래 걸려 쉬지 않고 다시 묻게 된다).
        // 조회 중에 mark_stale 했으면(None) 이 답은 바뀌기 전 값일 수 있으므로 다음에 다시 묻게 둔다
        if e.asked.is_some() {
            e.asked = Some(Instant::now());
        }
        e.space = space_of(p.space);
        if p.fs.is_some() {
            e.fs = p.fs;
        }
    }

    /// 지금 아는 용량. 기다리지 않고 바로 돌려주며, 처음이거나 SPACE_TTL이 지났으면 백그라운드에서 다시 묻는다.
    pub fn space(&mut self, path: &str) -> Space {
        let Self { entries, tx, probe, waker, .. } = self;
        if !entries.contains_key(path) {
            entries.insert(path.to_owned(), Entry { space: Space::Checking, fs: None, asked: None, busy: false });
        }
        let e = entries.get_mut(path).expect("방금 넣은 항목");
        if !e.busy && e.asked.is_none_or(|at| at.elapsed() >= SPACE_TTL) {
            e.busy = true;
            e.asked = Some(Instant::now());
            let (tx, probe, waker, path) = (tx.clone(), Arc::clone(probe), waker.clone(), path.to_owned());
            let spawned = std::thread::Builder::new().name("disk-space".into()).spawn(move || {
                let p = probe(&path);
                let _ = tx.send((path, p));
                if let Some(w) = waker {
                    w.request_repaint();
                }
            });
            if spawned.is_err() {
                e.busy = false;
                e.space = Space::Unavailable;
            }
        }
        e.space
    }

    /// 파일 시스템 이름 (드라이브 루트이고 답이 왔을 때)
    pub fn fs(&self, path: &str) -> Option<&str> {
        self.entries.get(path)?.fs.as_deref()
    }

    /// 값은 두고 다음에 볼 때 다시 묻게 한다 (삭제·스캔으로 여유 공간이 바뀌었을 때)
    pub fn mark_stale(&mut self) {
        for e in self.entries.values_mut() {
            e.asked = None;
        }
    }

    /// 테스트: 조회 스레드의 답 하나를 기다려 반영한다
    #[cfg(test)]
    pub fn wait_reply(&mut self) {
        let (path, p) = self.rx.recv_timeout(Duration::from_secs(10)).expect("조회 스레드가 답하지 않음");
        self.apply(&path, p);
    }
}

/// 실제 조회: 용량과 (드라이브 루트면) 파일 시스템
fn probe(path: &str) -> Probe {
    Probe { space: disk_space(path), fs: is_drive_root(path).then(|| volume_fs(path)).flatten() }
}

fn space_of(v: Option<(u64, u64)>) -> Space {
    match v {
        Some((total, free)) if plausible_space(total, free) => Space::Known(total, free),
        Some(_) => Space::Unknown,
        None => Space::Unavailable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

    const GB: u64 = 1 << 30;

    /// 같은 답만 돌려주는 가짜 조회
    fn fixed(space: Option<(u64, u64)>) -> Volumes {
        Volumes::with_probe(None, Arc::new(move |_| Probe { space, fs: None }))
    }

    #[test]
    fn blocked_probe_never_blocks_the_caller() {
        let calls = Arc::new(AtomicUsize::new(0));
        let (open, gate) = mpsc::channel::<()>();
        let gate = Mutex::new(gate);
        let counted = Arc::clone(&calls);
        let mut v = Volumes::with_probe(None, Arc::new(move |path| {
            counted.fetch_add(1, Relaxed);
            // 끊긴 네트워크 드라이브처럼 풀릴 때까지 막힌다
            let _ = gate.lock().unwrap().recv_timeout(Duration::from_secs(10));
            Probe { space: Some((476 * GB, 112 * GB)), fs: is_drive_root(path).then(|| "NTFS".into()) }
        }));
        let started = Instant::now();
        for _ in 0..100 {
            assert_eq!(v.space("C:\\"), Space::Checking);
            v.poll();
        }
        assert!(started.elapsed() < Duration::from_secs(5), "the UI thread waited for the probe");
        assert_eq!(v.fs("C:\\"), None);
        open.send(()).unwrap();
        v.wait_reply();
        assert_eq!(v.space("C:\\"), Space::Known(476 * GB, 112 * GB));
        assert_eq!(v.fs("C:\\"), Some("NTFS"));
        assert_eq!(calls.load(Relaxed), 1, "one probe per path while it is busy or fresh");
    }

    #[test]
    fn classifies_replies() {
        for (reply, expected) in [
            (Some((1 << 63, 1 << 63)), Space::Unknown),
            (Some((0, 0)), Space::Unknown),
            (None, Space::Unavailable),
            (Some((64 * GB, 8 * GB)), Space::Known(64 * GB, 8 * GB)),
        ] {
            let mut v = fixed(reply);
            assert_eq!(v.space("Y:\\"), Space::Checking);
            v.wait_reply();
            assert_eq!(v.space("Y:\\"), expected, "{reply:?}");
        }
    }

    #[test]
    fn stale_or_expired_entries_keep_old_value_while_asking_again() {
        let reply = Arc::new(Mutex::new(Some((100 * GB, 50 * GB))));
        let shared = Arc::clone(&reply);
        let mut v = Volumes::with_probe(None, Arc::new(move |_| Probe { space: *shared.lock().unwrap(), fs: None }));
        v.space("D:\\");
        v.wait_reply();
        assert_eq!(v.space("D:\\"), Space::Known(100 * GB, 50 * GB));
        // 아직 새 값이면 다시 묻지 않는다
        assert!(!v.entries["D:\\"].busy);
        *reply.lock().unwrap() = Some((100 * GB, 70 * GB));
        for expire in [false, true] {
            if expire {
                v.entries.get_mut("D:\\").unwrap().asked = Some(Instant::now() - SPACE_TTL);
            } else {
                v.mark_stale();
            }
            // 답이 오기 전에는 이전 값을 그대로 보여 준다
            let before = v.space("D:\\");
            assert!(v.entries["D:\\"].busy, "expire={expire}");
            assert!(matches!(before, Space::Known(_, _)), "expire={expire}");
            v.wait_reply();
            assert_eq!(v.space("D:\\"), Space::Known(100 * GB, 70 * GB));
        }
    }

    #[test]
    fn ttl_counts_from_the_reply_unless_marked_stale_while_asking() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&calls);
        let mut v = Volumes::with_probe(None, Arc::new(move |_| {
            counted.fetch_add(1, Relaxed);
            Probe { space: None, fs: None }
        }));
        // 끊긴 네트워크 드라이브처럼 답이 TTL보다 늦게 와도 받자마자 다시 묻지 않는다
        v.space("Z:\\");
        v.entries.get_mut("Z:\\").unwrap().asked = Some(Instant::now() - SPACE_TTL);
        v.wait_reply();
        assert_eq!(v.space("Z:\\"), Space::Unavailable);
        assert!(!v.entries["Z:\\"].busy && calls.load(Relaxed) == 1, "asked again right after a slow reply");
        // 조회 중에 바뀐 것으로 표시하면 그 답을 받은 뒤 다시 묻는다
        v.mark_stale();
        v.space("Z:\\");
        v.mark_stale();
        v.wait_reply();
        v.space("Z:\\");
        assert!(v.entries["Z:\\"].busy, "a reply that started before mark_stale is not fresh");
        v.wait_reply();
        assert_eq!(calls.load(Relaxed), 3);
    }
}
