//! Sliding-window median filter for distance readings.
//!
//! Ultrasonic sensors produce occasional wild outliers (echoes off the tank
//! wall, a ripple, condensation). A median over the last `N` samples rejects
//! up to `N/2` of them without lag on genuine level changes.

/// Median of the last `N` samples (`N` should be odd).
#[derive(Debug, Clone)]
pub struct Median<const N: usize> {
    ring: [u16; N],
    len: usize,
    next: usize,
}

impl<const N: usize> Default for Median<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> Median<N> {
    pub const fn new() -> Self {
        Self {
            ring: [0; N],
            len: 0,
            next: 0,
        }
    }

    pub fn clear(&mut self) {
        self.len = 0;
        self.next = 0;
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Window is full enough to trust the median.
    pub fn is_ready(&self) -> bool {
        self.len >= N.div_ceil(2)
    }

    /// Add a sample and return the current median (`None` until at least one
    /// sample is present).
    pub fn push(&mut self, v: u16) -> Option<u16> {
        self.ring[self.next] = v;
        self.next = (self.next + 1) % N;
        if self.len < N {
            self.len += 1;
        }
        self.median()
    }

    pub fn median(&self) -> Option<u16> {
        if self.len == 0 {
            return None;
        }
        let mut sorted = [0u16; N];
        sorted[..self.len].copy_from_slice(&self.ring[..self.len]);
        sorted[..self.len].sort_unstable();
        Some(sorted[self.len / 2])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_outliers() {
        let mut m = Median::<5>::new();
        for v in [500, 502, 4000, 501, 499] {
            m.push(v);
        }
        assert_eq!(m.median(), Some(501));
    }

    #[test]
    fn ready_at_half_window() {
        let mut m = Median::<7>::new();
        assert!(!m.is_ready());
        for v in 0..4 {
            m.push(v);
        }
        assert!(m.is_ready());
    }
}
