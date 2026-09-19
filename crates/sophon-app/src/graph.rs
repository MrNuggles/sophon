//! Turns time series into SVG path commands for Slint `Path` elements, and
//! picks colors. Coordinates are integers in a 1000x1000 viewbox.

use std::fmt::Write;

use slint::Color;
use sophon_core::history::Ring;

use crate::Series;

const EMPTY: &str = "M 0 1000";

static STYLE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Graph style from settings: 3 draws bars instead of a line and area.
pub fn set_style(style: usize) {
    STYLE.store(style, std::sync::atomic::Ordering::Relaxed);
}

fn bars() -> bool {
    STYLE.load(std::sync::atomic::Ordering::Relaxed) == 3
}

/// One bar per point, as rectangles in the `area` path.
fn bar_path(points: &[(i32, i32)], gap: i32) -> Series {
    let mut area = String::with_capacity(points.len() * 36);
    for (i, &(x, y)) in points.iter().enumerate() {
        let next = points.get(i + 1).map_or(x + (x - points.get(i.wrapping_sub(1)).map_or(x - 10, |p| p.0)), |p| p.0);
        let w = (next - x - gap).max(1);
        let _ = write!(area, "M {x} 1000 L {x} {y} L {} {y} L {} 1000 Z ", x + w, x + w);
    }
    Series { line: EMPTY.into(), area: area.into() }
}

/// Series with the newest sample at the right edge; missing history leaves a gap on the left.
pub fn series(values: impl ExactSizeIterator<Item = f32>, capacity: usize, max: f32) -> Series {
    let n = values.len();
    if n == 0 || capacity < 2 {
        return Series { line: EMPTY.into(), area: EMPTY.into() };
    }
    let step = 1000.0 / (capacity - 1) as f32;
    let max = if max > 0.0 { max } else { 1.0 };
    if bars() {
        let pts: Vec<(i32, i32)> = values
            .enumerate()
            .map(|(i, v)| (((capacity - n + i) as f32 * step) as i32, (998.0 - (v / max).clamp(0.0, 1.0) * 990.0) as i32))
            .collect();
        return bar_path(&pts, (step * 0.25) as i32);
    }
    let mut line = String::with_capacity(n * 11 + 16);
    let first_x = ((capacity - n) as f32 * step) as i32;
    let mut last_x = first_x;
    for (i, v) in values.enumerate() {
        let x = ((capacity - n + i) as f32 * step) as i32;
        let y = (998.0 - (v / max).clamp(0.0, 1.0) * 990.0) as i32;
        let _ = write!(line, "{}{x} {y}", if i == 0 { "M " } else { " L " });
        last_x = x;
    }
    let area = format!("{line} L {last_x} 1000 L {first_x} 1000 Z");
    Series { line: line.into(), area: area.into() }
}

pub fn ring<const N: usize>(r: &Ring<N>, max: f32) -> Series {
    series(r.iter(), N, max)
}

/// The newest `count` samples, right-aligned (for small live graphs).
pub fn recent<const N: usize>(r: &Ring<N>, count: usize, max: f32) -> Series {
    let n = r.len().min(count);
    series(r.iter().skip(r.len() - n), count, max)
}

/// A visible time range over a sample timeline (the 1 s history, or a coarser tier).
#[derive(Clone, Copy)]
pub struct Timeline<'a, const N: usize = { sophon_core::history::HISTORY }> {
    pub times: &'a Ring<N>,
    pub start: f32,
    pub end: f32,
}

/// Most points drawn per series; longer ranges are max-pooled so spikes survive.
const MAX_POINTS: usize = 300;

impl<const N: usize> Timeline<'_, N> {
    /// (time, value) pairs inside the range (plus one on each side, so lines reach the edges).
    fn points(&self, values: &Ring<N>) -> Vec<(f32, f32)> {
        let n = values.len().min(self.times.len());
        let mut pts = Vec::with_capacity(n.min(MAX_POINTS * 2));
        let mut past_end = false;
        for k in 0..n {
            let (Some(t), Some(v)) = (self.times.back(k), values.back(k)) else { break };
            if t > self.end {
                if past_end {
                    continue;
                }
                past_end = true;
            }
            pts.push((t, v));
            if t < self.start {
                break;
            }
        }
        pts.reverse();
        if pts.len() <= MAX_POINTS {
            return pts;
        }
        // Max-pool into buckets of equal time width.
        let span = (self.end - self.start).max(1e-3);
        let mut out: Vec<(f32, f32)> = Vec::with_capacity(MAX_POINTS + 2);
        let mut bucket = usize::MAX;
        for (t, v) in pts {
            let b = (((t - self.start) / span) * MAX_POINTS as f32).floor().max(-1.0) as isize as usize;
            match out.last_mut() {
                Some(last) if b == bucket => {
                    if v > last.1 {
                        *last = (t, v);
                    }
                }
                _ => {
                    out.push((t, v));
                    bucket = b;
                }
            }
        }
        out
    }

    /// Every sample inside the range, oldest first (not pooled).
    pub fn samples(&self, values: &Ring<N>) -> Vec<(f32, f32)> {
        let n = values.len().min(self.times.len());
        let mut pts = Vec::new();
        for k in 0..n {
            let (Some(t), Some(v)) = (self.times.back(k), values.back(k)) else { break };
            if t < self.start {
                break;
            }
            if t <= self.end {
                pts.push((t, v));
            }
        }
        pts.reverse();
        pts
    }

    pub fn series(&self, values: &Ring<N>, max: f32) -> Series {
        let pts = self.points(values);
        if pts.is_empty() {
            return Series { line: EMPTY.into(), area: EMPTY.into() };
        }
        let span = (self.end - self.start).max(1e-3);
        let max = if max > 0.0 { max } else { 1.0 };
        let x_of = |t: f32| (((t - self.start) / span) * 1000.0).clamp(-20.0, 1020.0) as i32;
        if bars() {
            let xy: Vec<(i32, i32)> = pts.iter().map(|&(t, v)| (x_of(t), (998.0 - (v / max).clamp(0.0, 1.0) * 990.0) as i32)).collect();
            let gap = if xy.len() > 1 { ((xy[1].0 - xy[0].0) / 4).max(0) } else { 0 };
            return bar_path(&xy, gap);
        }
        let mut line = String::with_capacity(pts.len() * 11 + 16);
        for (i, &(t, v)) in pts.iter().enumerate() {
            let y = (998.0 - (v / max).clamp(0.0, 1.0) * 990.0) as i32;
            let _ = write!(line, "{}{} {y}", if i == 0 { "M " } else { " L " }, x_of(t));
        }
        let area = format!("{line} L {} 1000 L {} 1000 Z", x_of(pts[pts.len() - 1].0), x_of(pts[0].0));
        Series { line: line.into(), area: area.into() }
    }

    /// Time at a fraction of the range.
    pub fn time_at(&self, frac: f32) -> f32 {
        self.start + frac.clamp(0.0, 1.0) * (self.end - self.start)
    }

    /// The sample nearest to time `t`: (its time, value).
    pub fn value_at(&self, values: &Ring<N>, t: f32) -> Option<(f32, f32)> {
        let n = values.len().min(self.times.len());
        let mut best: Option<(f32, f32)> = None;
        for k in 0..n {
            let (Some(st), Some(v)) = (self.times.back(k), values.back(k)) else { break };
            if best.is_none_or(|(bt, _)| (st - t).abs() < (bt - t).abs()) {
                best = Some((st, v));
            }
            if st < t - (self.end - self.start) / 20.0 - 30.0 {
                break;
            }
        }
        best
    }
}

pub fn slice(v: &[f32], capacity: usize, max: f32) -> Series {
    series(v.iter().copied(), capacity, max)
}

/// A "nice" axis maximum (1, 2 or 5 x 10^k) at or above `v`, never below `floor`.
pub fn nice_max(v: f32, floor: f32) -> f32 {
    let v = v.max(floor).max(f32::MIN_POSITIVE);
    let mag = 10f32.powf(v.log10().floor());
    for m in [1.0, 2.0, 5.0, 10.0] {
        if m * mag >= v * 0.999 {
            return m * mag;
        }
    }
    10.0 * mag
}

/// A stable, pleasant color for a name (same app -> same color).
pub fn tint(name: &str) -> Color {
    let mut h: u32 = 0x811c9dc5;
    for b in name.bytes() {
        h = (h ^ b as u32).wrapping_mul(0x01000193);
    }
    hsv((h % 360) as f32, 0.55, 0.95)
}

fn hsv(h: f32, s: f32, v: f32) -> Color {
    let c = v * s;
    let x = c * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
    let (r, g, b) = match (h / 60.0) as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = v - c;
    let u = |f: f32| ((f + m) * 255.0) as u8;
    Color::from_rgb_u8(u(r), u(g), u(b))
}

pub fn rgb(hex: u32) -> Color {
    Color::from_rgb_u8((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nice() {
        assert_eq!(nice_max(0.3, 1.0), 1.0);
        assert_eq!(nice_max(3.0, 1.0), 5.0);
        assert_eq!(nice_max(120.0, 1.0), 200.0);
        assert_eq!(nice_max(1000.0, 1.0), 1000.0);
    }

    #[test]
    fn right_aligned() {
        let s = series([0.0, 100.0].into_iter(), 3, 100.0);
        assert_eq!(s.line.as_str(), "M 500 998 L 1000 8");
    }
}
