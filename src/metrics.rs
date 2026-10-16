//! Cheap process-wide counters, exposed by `GET /api/metrics`.
//!
//! Deliberately plain atomics: the numbers are for the UI and for a quick
//! "is this thing actually moving bytes?" check, not for accounting.

use std::sync::atomic::{AtomicU64, Ordering};

/// Totals since this process started.
#[derive(Debug, Default)]
pub struct Metrics {
    transfers_started: AtomicU64,
    transfers_completed: AtomicU64,
    transfers_failed: AtomicU64,
    bytes_sent: AtomicU64,
    bytes_received: AtomicU64,
}

impl Metrics {
    pub fn new() -> Self {
        Self::default()
    }

    /// A transfer entered the manager (offered by us, or accepted by us).
    pub fn transfer_started(&self) {
        self.transfers_started.fetch_add(1, Ordering::Relaxed);
    }

    /// A transfer reached `completed`/`cancelled`/`rejected`.
    pub fn transfer_finished(&self) {
        self.transfers_completed.fetch_add(1, Ordering::Relaxed);
    }

    /// A transfer failed (network error, checksum mismatch, disk guard, …).
    pub fn transfer_failed(&self) {
        self.transfers_failed.fetch_add(1, Ordering::Relaxed);
    }

    pub fn add_sent(&self, bytes: u64) {
        self.bytes_sent.fetch_add(bytes, Ordering::Relaxed);
    }

    pub fn add_received(&self, bytes: u64) {
        self.bytes_received.fetch_add(bytes, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> serde_json::Value {
        serde_json::json!({
            "transfers_started": self.transfers_started.load(Ordering::Relaxed),
            "transfers_finished": self.transfers_completed.load(Ordering::Relaxed),
            "transfers_failed": self.transfers_failed.load(Ordering::Relaxed),
            "bytes_sent": self.bytes_sent.load(Ordering::Relaxed),
            "bytes_received": self.bytes_received.load(Ordering::Relaxed),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::Metrics;

    #[test]
    fn counters_add_up() {
        let m = Metrics::new();
        m.transfer_started();
        m.transfer_started();
        m.transfer_finished();
        m.transfer_failed();
        m.add_sent(1_500);
        m.add_sent(500);
        m.add_received(2_048);

        let s = m.snapshot();
        assert_eq!(s["transfers_started"], 2);
        assert_eq!(s["transfers_finished"], 1);
        assert_eq!(s["transfers_failed"], 1);
        assert_eq!(s["bytes_sent"], 2_000);
        assert_eq!(s["bytes_received"], 2_048);
    }
}
