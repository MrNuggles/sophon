//! Fixed-capacity ring buffers for time series. No allocation after creation.

/// Samples kept for graphs: one per tick, so 10 minutes at the default rate
/// (longer while the window is hidden and sampling slows down).
pub const HISTORY: usize = 600;

#[derive(Clone, Copy)]
pub struct Ring<const N: usize = HISTORY> {
    buf: [f32; N],
    head: usize,
    len: usize,
}

impl<const N: usize> Default for Ring<N> {
    fn default() -> Self {
        Self { buf: [0.0; N], head: 0, len: 0 }
    }
}

impl<const N: usize> Ring<N> {
    pub fn push(&mut self, v: f32) {
        self.buf[self.head] = v;
        self.head = (self.head + 1) % N;
        self.len = (self.len + 1).min(N);
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub const fn capacity(&self) -> usize {
        N
    }

    /// Oldest to newest.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = f32> + '_ {
        let start = (self.head + N - self.len) % N;
        (0..self.len).map(move |i| self.buf[(start + i) % N])
    }

    /// The `k`-th newest sample (0 = newest).
    pub fn back(&self, k: usize) -> Option<f32> {
        (k < self.len).then(|| self.buf[(self.head + N - 1 - k) % N])
    }

    pub fn last(&self) -> f32 {
        if self.len == 0 { 0.0 } else { self.buf[(self.head + N - 1) % N] }
    }

    pub fn max(&self) -> f32 {
        self.iter().fold(0.0, f32::max)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_in_order() {
        let mut r = Ring::<3>::default();
        for v in 1..=5 {
            r.push(v as f32);
        }
        assert_eq!(r.iter().collect::<Vec<_>>(), vec![3.0, 4.0, 5.0]);
        assert_eq!(r.last(), 5.0);
        assert_eq!(r.max(), 5.0);
        assert_eq!(r.back(0), Some(5.0));
        assert_eq!(r.back(2), Some(3.0));
        assert_eq!(r.back(3), None);
    }
}
