//! The proxy's resource bounds (memo decision 4).
//!
//! Every field bounds a quantity that actually costs: bytes buffered, requests
//! in flight, sockets open, wall time waited. Tests construct a
//! [`ProxyLimits`] with small values so a timeout is provable in a second, and
//! a separate test asserts the production defaults, so shrinking a bound for a
//! test can never quietly become the shipped bound.

use std::time::Duration;

/// Bounds applied to one proxy process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProxyLimits {
    /// TCP + TLS connect budget.
    pub connect_timeout: Duration,
    /// Whole-request budget, connect included.
    pub request_timeout: Duration,
    /// Largest accepted response body.
    pub max_response_bytes: usize,
    /// Largest accepted outgoing request body.
    pub max_request_bytes: usize,
    /// Largest accepted inbound stdio frame.
    pub max_inbound_frame_bytes: usize,
    /// Requests allowed in flight at once.
    pub max_in_flight: usize,
    /// Idle upstream connections kept open.
    pub max_connections: usize,
    /// Total bytes the proxy may have buffered across all in-flight requests.
    ///
    /// Each in-flight request reserves its own frame plus the response
    /// allowance, so with a 4 MiB response cap this ceiling binds before
    /// [`ProxyLimits::max_in_flight`] does. That is the intent: the byte
    /// ceiling is the quantity that costs, and reads pause on it rather than
    /// queueing behind it.
    pub max_buffered_bytes: usize,
}

impl Default for ProxyLimits {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(10),
            request_timeout: Duration::from_secs(60),
            max_response_bytes: 4 * 1024 * 1024,
            max_request_bytes: 1024 * 1024,
            max_inbound_frame_bytes: 1024 * 1024,
            max_in_flight: 16,
            max_connections: 8,
            max_buffered_bytes: 32 * 1024 * 1024,
        }
    }
}

impl ProxyLimits {
    /// Bytes one in-flight request may reserve: its own frame plus the response
    /// allowance it may receive.
    pub fn reservation_for(&self, frame_len: usize) -> usize {
        frame_len.saturating_add(self.max_response_bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shipped_bounds_are_the_memo_values() {
        let limits = ProxyLimits::default();
        assert_eq!(limits.connect_timeout, Duration::from_secs(10));
        assert_eq!(limits.request_timeout, Duration::from_secs(60));
        assert_eq!(limits.max_response_bytes, 4 * 1024 * 1024);
        assert_eq!(limits.max_request_bytes, 1024 * 1024);
        assert_eq!(limits.max_inbound_frame_bytes, 1024 * 1024);
        assert_eq!(limits.max_in_flight, 16);
        assert_eq!(limits.max_connections, 8);
        assert_eq!(limits.max_buffered_bytes, 32 * 1024 * 1024);
    }

    #[test]
    fn one_request_always_fits_inside_the_buffer_ceiling() {
        // Without this the reservation could exceed the whole budget and every
        // forward would deadlock on a semaphore that can never be satisfied.
        let limits = ProxyLimits::default();
        assert!(limits.reservation_for(limits.max_request_bytes) <= limits.max_buffered_bytes);
    }
}
