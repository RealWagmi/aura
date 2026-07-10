//! Small full-width replay window shared by the UDP and iroh datagram paths.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReplayStatus {
    Fresh,
    Duplicate,
    TooOld,
}

pub(crate) struct ReplayWindow {
    highest: Option<u64>,
    bitmap: u128,
}

impl ReplayWindow {
    pub(crate) fn new() -> Self {
        Self {
            highest: None,
            bitmap: 0,
        }
    }

    pub(crate) fn observe(&mut self, nonce: u64) -> ReplayStatus {
        let Some(highest) = self.highest else {
            self.highest = Some(nonce);
            self.bitmap = 1;
            return ReplayStatus::Fresh;
        };
        if nonce > highest {
            let shift = nonce - highest;
            self.bitmap = if shift >= 128 {
                1
            } else {
                (self.bitmap << shift) | 1
            };
            self.highest = Some(nonce);
            return ReplayStatus::Fresh;
        }
        let age = highest - nonce;
        if age >= 128 {
            return ReplayStatus::TooOld;
        }
        let bit = 1_u128 << age;
        if self.bitmap & bit != 0 {
            ReplayStatus::Duplicate
        } else {
            self.bitmap |= bit;
            ReplayStatus::Fresh
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_duplicates_and_old_packets_but_accepts_reordering() {
        let mut window = ReplayWindow::new();
        assert_eq!(window.observe(10), ReplayStatus::Fresh);
        assert_eq!(window.observe(12), ReplayStatus::Fresh);
        assert_eq!(window.observe(11), ReplayStatus::Fresh);
        assert_eq!(window.observe(11), ReplayStatus::Duplicate);
        assert_eq!(window.observe(140), ReplayStatus::Fresh);
        assert_eq!(window.observe(10), ReplayStatus::TooOld);
    }
}
