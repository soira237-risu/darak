//! 취소 가능한 정렬: 검색 결과와 스캔 미리 보기가 함께 쓴다.
//!
//! 최대 4,096개짜리 run을 정렬한 뒤, 출력 위치 기준(merge path)으로 나눈 청크를 병합한다.
//! 취소 확인 사이에는 run 정렬 하나 또는 출력 256개만 실행된다. 취소되면 `values`는
//! 정렬 전 순서이거나 이전 병합 단계까지만 반영된 상태이며, 호출자는 그 값을 쓰지 않는다.

use rayon::prelude::*;

/// 한 번에 정렬하는 run 길이이자 병합 출력 청크 길이.
pub(crate) const RUN: usize = 4096;
/// 병합 중 취소를 확인하는 출력 간격.
const CHECK_EVERY: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Cancelled;

/// run 정렬과 병합 청크를 rayon pool에 나눌지, 호출한 스레드에서만 처리할지.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Exec {
    Parallel,
    Sequential,
}

fn check(cancelled: &(dyn Fn() -> bool + Sync)) -> Result<(), Cancelled> {
    if cancelled() { Err(Cancelled) } else { Ok(()) }
}

/// `key` 오름차순으로 정렬한다. 같은 key의 상대 순서는 보장하지 않는다.
pub(crate) fn sort_by_key<T, K>(
    values: &mut Vec<T>,
    key: impl Fn(&T) -> K + Sync,
    exec: Exec,
    cancelled: &(dyn Fn() -> bool + Sync),
) -> Result<(), Cancelled>
where
    T: Copy + Send + Sync,
    K: Ord,
{
    check(cancelled)?;
    let sort_run = |run: &mut [T]| {
        check(cancelled)?;
        run.sort_unstable_by_key(&key);
        check(cancelled)
    };
    match exec {
        Exec::Parallel => values.par_chunks_mut(RUN).try_for_each(sort_run)?,
        Exec::Sequential => values.chunks_mut(RUN).try_for_each(sort_run)?,
    }
    if values.len() <= RUN {
        return Ok(());
    }
    let mut buffer = values.clone();
    let mut width = RUN;
    while width < values.len() {
        let source: &[T] = values;
        let merge = |(chunk, outputs): (usize, &mut [T])| {
            merge_chunk(source, width, chunk * RUN, outputs, &key, cancelled)
        };
        match exec {
            Exec::Parallel => buffer.par_chunks_mut(RUN).enumerate().try_for_each(merge)?,
            Exec::Sequential => buffer.chunks_mut(RUN).enumerate().try_for_each(merge)?,
        }
        std::mem::swap(values, &mut buffer);
        width *= 2;
        check(cancelled)?;
    }
    Ok(())
}

/// `source`의 정렬된 `width` run 쌍을 병합한 결과 중 `offset`부터 `outputs.len()`개를 쓴다.
fn merge_chunk<T: Copy, K: Ord>(
    source: &[T],
    width: usize,
    offset: usize,
    outputs: &mut [T],
    key: &impl Fn(&T) -> K,
    cancelled: &(dyn Fn() -> bool + Sync),
) -> Result<(), Cancelled> {
    check(cancelled)?;
    let start = offset / (width * 2) * (width * 2);
    let middle = (start + width).min(source.len());
    let end = (start + width * 2).min(source.len());
    let diagonal = offset - start;
    // 이 청크 앞의 출력 중 왼쪽 run이 낸 개수를 찾는다. 같은 key는 왼쪽이 먼저 나간다.
    let mut low = diagonal.saturating_sub(end - middle);
    let mut high = diagonal.min(middle - start);
    while low < high {
        let left = low + (high - low) / 2;
        let right = diagonal - left;
        if right > 0 && key(&source[start + left]) <= key(&source[middle + right - 1]) {
            low = left + 1;
        } else {
            high = left;
        }
    }
    let (mut left, mut right) = (start + low, middle + diagonal - low);
    // key 계산이 비쌀 수 있으므로(트리 노드 조회) 두 경계의 key를 캐시한다.
    let mut left_key = (left < middle).then(|| key(&source[left]));
    let mut right_key = (right < end).then(|| key(&source[right]));
    for (i, output) in outputs.iter_mut().enumerate() {
        if i.is_multiple_of(CHECK_EVERY) {
            check(cancelled)?;
        }
        if left_key.is_some() && (right_key.is_none() || left_key <= right_key) {
            *output = source[left];
            left += 1;
            left_key = (left < middle).then(|| key(&source[left]));
        } else {
            *output = source[right];
            right += 1;
            right_key = (right < end).then(|| key(&source[right]));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};

    fn pseudo_random(n: usize, modulo: u64) -> Vec<(u64, usize)> {
        let mut x = 0x2545_f491_4f6c_dd1du64;
        (0..n)
            .map(|i| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                (x % modulo, i)
            })
            .collect()
    }

    #[test]
    fn sorts_every_length_and_mode_including_ties() {
        for threads in [1, 4] {
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .build()
                .unwrap();
            for exec in [Exec::Parallel, Exec::Sequential] {
                for n in [
                    0, 1, 4095, 4096, 4097, 8191, 8192, 8193, 12289, 16385, 40000,
                ] {
                    for modulo in [3, 1 << 40] {
                        let mut values = pseudo_random(n, modulo);
                        let mut expected = values.clone();
                        expected.sort_by_key(|v| v.0);
                        pool.install(|| sort_by_key(&mut values, |v| v.0, exec, &|| false))
                            .unwrap();
                        // 같은 key의 순서는 정해지지 않으므로 key 순서와 원소 집합을 따로 확인한다.
                        assert!(
                            values.windows(2).all(|w| w[0].0 <= w[1].0),
                            "{exec:?} {n} {modulo}"
                        );
                        let mut ids: Vec<_> = values.iter().map(|v| v.1).collect();
                        ids.sort_unstable();
                        assert_eq!(ids, (0..n).collect::<Vec<_>>(), "{exec:?} {n} {modulo}");
                        assert_eq!(
                            values.iter().map(|v| v.0).collect::<Vec<_>>(),
                            expected.iter().map(|v| v.0).collect::<Vec<_>>()
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn unique_keys_give_exact_order() {
        let mut values = pseudo_random(20000, 7);
        let mut expected = values.clone();
        expected.sort_unstable();
        sort_by_key(&mut values, |&v| v, Exec::Parallel, &|| false).unwrap();
        assert_eq!(values, expected);
    }

    #[test]
    fn sequential_cancellation_stops_after_one_bounded_run() {
        let cancelled = AtomicBool::new(false);
        let largest_seen = AtomicUsize::new(0);
        let mut values: Vec<_> = (0..8_193).map(|i| (8_193 - i, i)).collect();
        let result = sort_by_key(
            &mut values,
            |v| {
                largest_seen.fetch_max(v.1, Relaxed);
                cancelled.store(true, Relaxed);
                v.0
            },
            Exec::Sequential,
            &|| cancelled.load(Relaxed),
        );
        assert_eq!(result, Err(Cancelled));
        assert!(
            largest_seen.load(Relaxed) < RUN,
            "another sort run started after cancellation"
        );
    }

    #[test]
    fn merge_checks_cancellation_within_256_outputs() {
        for exec in [Exec::Parallel, Exec::Sequential] {
            // run 정렬이 끝난 뒤(초기 1회 + run 2개 × 2회)부터 key 호출을 센다.
            let checks = AtomicUsize::new(0);
            let merge_keys = AtomicUsize::new(0);
            let mut values: Vec<_> = (0..8_192).map(|i| (i % 4_096, i)).collect();
            let result = sort_by_key(
                &mut values,
                |v| {
                    if checks.load(Relaxed) >= 5 {
                        merge_keys.fetch_add(1, Relaxed);
                    }
                    v.0
                },
                exec,
                &|| {
                    checks.fetch_add(1, Relaxed);
                    merge_keys.load(Relaxed) > 0
                },
            );
            assert_eq!(result, Err(Cancelled));
            // 청크마다 경계 탐색 log2(4096)회 × 2, 경계 key 2개, 출력 하나당 key 1개.
            let limit = 2 * (12 * 2 + 2 + CHECK_EVERY);
            assert!(
                (1..=limit).contains(&merge_keys.load(Relaxed)),
                "{exec:?}: {}",
                merge_keys.load(Relaxed)
            );
        }
    }
}
