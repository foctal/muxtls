use std::sync::atomic::{AtomicU64, Ordering};

use muxtls_proto::VarInt;

use crate::{Error, Result};

/// Receive totals count payload bytes, including bytes held by a partial read.
pub(crate) struct ReceiveWindow {
    pub(crate) received: AtomicU64,
    consumed: AtomicU64,
    advertised: AtomicU64,
    window: u64,
}

impl ReceiveWindow {
    pub(crate) fn new(window: usize) -> Self {
        Self {
            received: AtomicU64::new(0),
            consumed: AtomicU64::new(0),
            advertised: AtomicU64::new(window as u64),
            window: window as u64,
        }
    }

    pub(crate) fn receive(&self, size: usize) -> Result<()> {
        self.received
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                value
                    .checked_add(size as u64)
                    .filter(|next| *next <= self.advertised.load(Ordering::Acquire))
            })
            .map(|_| ())
            .map_err(|_| Error::Protocol("peer exceeded advertised receive credit".to_owned()))
    }

    pub(crate) fn consume(&self, size: usize) -> bool {
        // Every byte has a unique owning receive chunk and is released once.
        if size == 0 {
            return false;
        }
        self.consumed.fetch_add(size as u64, Ordering::AcqRel);
        self.needs_update()
    }

    pub(crate) fn needs_update(&self) -> bool {
        let next = self
            .consumed
            .load(Ordering::Acquire)
            .saturating_add(self.window)
            .min(VarInt::MAX);
        let old = self.advertised.load(Ordering::Acquire);
        next > old
            && (next - old >= self.window.div_ceil(2)
                || self.received.load(Ordering::Acquire) == old
                || next == VarInt::MAX)
    }

    pub(crate) fn update(&self) -> Option<VarInt> {
        let next = self
            .consumed
            .load(Ordering::Acquire)
            .saturating_add(self.window)
            .min(VarInt::MAX);
        if self.needs_update() {
            self.advertised.store(next, Ordering::Release);
            Some(VarInt::from_u64(next).expect("bounded credit"))
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn updates_coalesce_and_overruns_do_not_mutate_received_total() {
        let window = ReceiveWindow::new(32);
        window.receive(31).unwrap();
        assert!(window.receive(2).is_err());
        assert_eq!(window.received.load(Ordering::Acquire), 31);
        for _ in 0..15 {
            window.consume(1);
            assert!(window.update().is_none());
        }
        window.consume(1);
        assert_eq!(window.update().unwrap().into_inner(), 48);
        assert!(window.update().is_none());
        window.receive(17).unwrap();
        assert!(window.receive(1).is_err());
        window.consume(1);
        assert_eq!(window.update().unwrap().into_inner(), 49);
        window.receive(1).unwrap();
    }
    #[test]
    fn lifetime_offsets_saturate_without_overflow_or_duplicate_credit() {
        let window = ReceiveWindow::new(32);
        window.consumed.store(VarInt::MAX - 2, Ordering::Release);
        window.received.store(VarInt::MAX - 2, Ordering::Release);
        assert_eq!(window.update().unwrap().into_inner(), VarInt::MAX);
        window.receive(2).unwrap();
        assert!(window.receive(1).is_err());
        window.consume(2);
        assert!(window.update().is_none());
    }
}
