//! The interval a fetch actually proves, per T12a decision 13.
//!
//! A batch carries the interval it reached plus `complete` or
//! `truncated(reason)`, proven rather than inferred. The walk sets
//! `orderBy=startTime` under T11 decision 6's page, byte and deadline caps, so
//! a *complete* page orders start alone: every event not yet returned starts at
//! or after that page's maximum start. The proven interval is therefore
//! half-open:
//!
//! `[window start, min(window end, last complete page's maximum start))`
//!
//! Never the maximum *end*: an event running past the bound is known, but the
//! days after it are not. Zero complete pages prove nothing at all.

use serde::{Deserialize, Serialize};

/// Why a walk stopped short of the requested window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TruncationReason {
    /// The walk hit T11 decision 6's page cap.
    PageCap,
    /// The walk hit T11 decision 6's byte cap.
    ByteCap,
    /// The walk hit T11 decision 6's event-count cap.
    EventCap,
    /// The walk hit T11 decision 6's 30-second deadline.
    Deadline,
    /// The walk stopped on a transport or HTTP failure.
    Transport,
    /// Rows the interval covered were evicted from the render cache.
    Evicted,
}

/// Whether a proven interval covers the whole requested window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "coverage", content = "reason")]
pub enum Coverage {
    /// The walk returned every event in the window.
    Complete,
    /// The walk stopped early; only the proven interval is authoritative.
    Truncated(TruncationReason),
}

/// The half-open instant range a fetch requested, in milliseconds since the
/// Unix epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Window {
    /// Inclusive start.
    pub start_ms: i64,
    /// Exclusive end.
    pub end_ms: i64,
}

impl Window {
    /// A window from `start_ms` (inclusive) to `end_ms` (exclusive).
    ///
    /// An inverted or empty input collapses to an empty window at `start_ms`
    /// rather than producing a range that reads as covering everything.
    pub fn new(start_ms: i64, end_ms: i64) -> Self {
        Self {
            start_ms,
            end_ms: end_ms.max(start_ms),
        }
    }
}

/// What a batch proves: a half-open interval plus its coverage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProvenInterval {
    /// Inclusive start; always the requested window's start.
    pub start_ms: i64,
    /// Exclusive end of the proven interval.
    pub end_ms: i64,
    /// Whether the whole window was proven.
    pub coverage: Coverage,
}

impl ProvenInterval {
    /// The interval a completed walk proves: the whole window.
    pub fn complete(window: Window) -> Self {
        Self {
            start_ms: window.start_ms,
            end_ms: window.end_ms,
            coverage: Coverage::Complete,
        }
    }

    /// The interval a truncated walk proves.
    ///
    /// `last_complete_page_max_start` is the maximum event start of the last
    /// page the walk received *whole*; a page cut mid-stream is discarded
    /// entirely and must not be passed here. `None` means no page completed,
    /// which proves nothing: the interval is empty at the window start.
    pub fn truncated(
        window: Window,
        last_complete_page_max_start: Option<i64>,
        reason: TruncationReason,
    ) -> Self {
        let end_ms = match last_complete_page_max_start {
            // Half-open at the page's maximum start: an event at exactly that
            // instant may have siblings on the next page, so the instant
            // itself is not proven.
            Some(max_start) => max_start.clamp(window.start_ms, window.end_ms),
            None => window.start_ms,
        };
        Self {
            start_ms: window.start_ms,
            end_ms,
            coverage: Coverage::Truncated(reason),
        }
    }

    /// Whether this interval proves anything at all.
    pub fn is_empty(&self) -> bool {
        self.end_ms <= self.start_ms
    }

    /// Whether `instant_ms` falls inside the proven interval.
    pub fn proves_instant(&self, instant_ms: i64) -> bool {
        instant_ms >= self.start_ms && instant_ms < self.end_ms
    }

    /// Whether a whole day is authoritative.
    ///
    /// T12a decision 13 makes only a date *wholly* inside the interval
    /// authoritative; the date holding the bound and every date past it render
    /// "unknown, more…", never empty. Day boundaries are computed in the
    /// display zone by the caller (T12a decision 2 keeps them date arithmetic,
    /// not instant arithmetic), so they arrive here as instants.
    pub fn proves_day(&self, day_start_ms: i64, day_end_ms: i64) -> bool {
        day_start_ms >= self.start_ms && day_end_ms <= self.end_ms
    }

    /// Downgrade to `truncated(evicted)`.
    ///
    /// T12a decision 13 requires eviction to downgrade every interval it
    /// touches in the dropping transaction, so `complete` holds only while its
    /// rows survive. The proven end is unchanged: eviction removes rows, it
    /// does not extend knowledge.
    pub fn downgrade_evicted(&mut self) {
        self.coverage = Coverage::Truncated(TruncationReason::Evicted);
    }
}
