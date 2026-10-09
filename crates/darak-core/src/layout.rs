//! Squarified treemap 레이아웃 (Bruls, Huizing, van Wijk 2000).

#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }
    pub fn area(&self) -> f32 {
        self.w * self.h
    }
}

/// `values`(내림차순, 모두 0보다 큼)를 `rect` 안에 배치한다. `out`에 같은 순서로 사각형을 추가한다.
pub fn squarify(values: &[f64], rect: Rect, out: &mut Vec<Rect>) {
    run(values, rect, out, None, None);
}

/// squarify를 하면서 각 줄에 몇 개가 들어갔는지(`rows`)도 돌려준다
pub fn squarify_with_rows(values: &[f64], rect: Rect, out: &mut Vec<Rect>, rows: &mut Vec<usize>) {
    rows.clear();
    run(values, rect, out, None, Some(rows));
}

/// 줄 구성을 고정한 채 배치한다. 값이 바뀌어도 항목이 다른 줄로 옮겨 가지 않는다.
/// `rows`의 합이 `values.len()`보다 작으면 나머지는 마지막 줄 하나로 넣는다.
pub fn layout_rows(values: &[f64], rows: &[usize], rect: Rect, out: &mut Vec<Rect>) {
    run(values, rect, out, Some(rows), None);
}

/// 사각형들 중 가장 나쁜 가로세로 비율 (1 = 정사각형)
pub fn worst_aspect(rects: &[Rect]) -> f32 {
    rects
        .iter()
        .filter(|r| r.w > 0.5 && r.h > 0.5)
        .map(|r| (r.w / r.h).max(r.h / r.w))
        .fold(1.0, f32::max)
}

fn run(values: &[f64], rect: Rect, out: &mut Vec<Rect>, fixed: Option<&[usize]>, mut rows_out: Option<&mut Vec<usize>>) {
    let n = values.len();
    if n == 0 {
        return;
    }
    let total: f64 = values.iter().sum();
    if total <= 0.0 || rect.w <= 0.0 || rect.h <= 0.0 {
        out.extend(std::iter::repeat_n(Rect::new(rect.x, rect.y, 0.0, 0.0), n));
        return;
    }

    let scale = (rect.w as f64 * rect.h as f64) / total;
    let (mut x, mut y, mut w, mut h) = (rect.x as f64, rect.y as f64, rect.w as f64, rect.h as f64);
    let mut i = 0;
    let mut row_no = 0;
    while i < n {
        let j = match fixed {
            Some(rows) => match rows.get(row_no) {
                Some(&len) if len > 0 => (i + len).min(n),
                _ => n,
            },
            None => {
                // 가장 나쁜 가로세로 비율이 나빠지기 직전까지 한 줄에 넣는다
                let short = w.min(h).max(1e-9);
                let short2 = short * short;
                let mut j = i;
                let (mut sum, mut lo, mut hi) = (0.0f64, f64::INFINITY, 0.0f64);
                let mut best = f64::INFINITY;
                while j < n {
                    let a = values[j] * scale;
                    let s = sum + a;
                    let (l, hh) = (lo.min(a), hi.max(a));
                    let worst = (short2 * hh / (s * s)).max((s * s) / (short2 * l));
                    if j > i && worst > best {
                        break;
                    }
                    best = worst;
                    sum = s;
                    lo = l;
                    hi = hh;
                    j += 1;
                }
                j
            }
        };
        if let Some(r) = rows_out.as_deref_mut() {
            r.push(j - i);
        }
        row_no += 1;
        let sum: f64 = values[i..j].iter().map(|v| v * scale).sum();

        let last = j == n;
        if w >= h {
            // 왼쪽에 세로 열 하나
            let cw = if last { w } else { (sum / h).min(w) };
            let mut yy = y;
            for (offset, value) in values[i..j].iter().enumerate() {
                let hk = if i + offset + 1 == j { y + h - yy } else { value * scale / cw };
                out.push(Rect::new(x as f32, yy as f32, cw as f32, hk as f32));
                yy += hk;
            }
            x += cw;
            w -= cw;
        } else {
            // 위쪽에 가로 행 하나
            let rh = if last { h } else { (sum / w).min(h) };
            let mut xx = x;
            for (offset, value) in values[i..j].iter().enumerate() {
                let wk = if i + offset + 1 == j { x + w - xx } else { value * scale / rh };
                out.push(Rect::new(xx as f32, y as f32, wk as f32, rh as f32));
                xx += wk;
            }
            y += rh;
            h -= rh;
        }
        i = j;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn covers_area_without_overlap() {
        let vals = [6.0, 6.0, 4.0, 3.0, 2.0, 2.0, 1.0];
        let r = Rect::new(0.0, 0.0, 6.0, 4.0);
        let mut out = Vec::new();
        squarify(&vals, r, &mut out);
        assert_eq!(out.len(), vals.len());
        let total: f32 = out.iter().map(|r| r.area()).sum();
        assert!((total - 24.0).abs() < 1e-3);
        for (v, rc) in vals.iter().zip(&out) {
            assert!((rc.area() as f64 - v).abs() < 1e-3, "{v} vs {rc:?}");
            assert!(rc.x >= -1e-4 && rc.y >= -1e-4 && rc.x + rc.w <= 6.0001 && rc.y + rc.h <= 4.0001);
        }
        for a in 0..out.len() {
            for b in a + 1..out.len() {
                let (p, q) = (out[a], out[b]);
                let ox = (p.x + p.w).min(q.x + q.w) - p.x.max(q.x);
                let oy = (p.y + p.h).min(q.y + q.h) - p.y.max(q.y);
                assert!(ox <= 1e-3 || oy <= 1e-3, "overlap {p:?} {q:?}");
            }
        }
    }

    #[test]
    fn fixed_rows_keep_structure() {
        let r = Rect::new(0.0, 0.0, 10.0, 10.0);
        let (mut out, mut rows) = (Vec::new(), Vec::new());
        squarify_with_rows(&[5.0, 3.0, 1.0, 1.0], r, &mut out, &mut rows);
        assert_eq!(rows.iter().sum::<usize>(), 4);
        // 값이 바뀌어도 같은 줄 구성을 쓰면 면적은 맞고 개수도 같다
        let mut out2 = Vec::new();
        layout_rows(&[4.0, 4.0, 1.0, 1.0], &rows, r, &mut out2);
        assert_eq!(out2.len(), 4);
        let total: f32 = out2.iter().map(|r| r.area()).sum();
        assert!((total - 100.0).abs() < 1e-2);
        assert!(worst_aspect(&out2) >= 1.0);
    }

    #[test]
    fn single_value_fills_rect() {
        let mut out = Vec::new();
        squarify(&[5.0], Rect::new(1.0, 2.0, 10.0, 3.0), &mut out);
        assert_eq!(out, vec![Rect::new(1.0, 2.0, 10.0, 3.0)]);
    }
}
