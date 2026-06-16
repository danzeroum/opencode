//! In-process **metrics** — lightweight, dependency-free runner/coordinator counters plus a bounded
//! turn-latency sample buffer (percentiles computed on read). Shared via [`AppContext::metrics`] and
//! incremented at the server boundary (the runner invocation + the prompt handlers).
//!
//! This is the "prove it internally first" step (mirroring the bus + coordinator). The Prometheus text
//! exporter ([`MetricsSnapshot::render_prometheus`]) reuses these names; an OTLP bridge can follow.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// Recent turn latencies retained for percentile snapshots (bounded, drop-oldest).
const LATENCY_CAPACITY: usize = 1024;

/// Process-wide runner metrics — atomic counters + a bounded latency buffer. Cheap to share behind an
/// `Arc`; every method takes `&self`.
#[derive(Default)]
pub struct AppMetrics {
    prompts: AtomicU64,
    turns: AtomicU64,
    steps: AtomicU64,
    errors: AtomicU64,
    cancellations: AtomicU64,
    latencies_ms: Mutex<VecDeque<u64>>,
}

impl AppMetrics {
    /// A prompt was admitted (the `/prompt` admission path).
    pub fn record_prompt(&self) {
        self.prompts.fetch_add(1, Ordering::Relaxed);
    }

    /// A turn-loop run completed: `steps` taken over `latency_ms` wall-clock.
    pub fn record_turn(&self, steps: u64, latency_ms: u64) {
        self.turns.fetch_add(1, Ordering::Relaxed);
        self.steps.fetch_add(steps, Ordering::Relaxed);
        let mut l = self.latencies_ms.lock().expect("metrics mutex poisoned");
        if l.len() >= LATENCY_CAPACITY {
            l.pop_front();
        }
        l.push_back(latency_ms);
    }

    /// A turn-loop run errored.
    pub fn record_error(&self) {
        self.errors.fetch_add(1, Ordering::Relaxed);
    }

    /// A run ended in cancellation.
    pub fn record_cancellation(&self) {
        self.cancellations.fetch_add(1, Ordering::Relaxed);
    }

    /// A consistent point-in-time snapshot: the counters plus turn-latency percentiles (nearest-rank).
    pub fn snapshot(&self) -> MetricsSnapshot {
        let mut samples: Vec<u64> = self
            .latencies_ms
            .lock()
            .expect("metrics mutex poisoned")
            .iter()
            .copied()
            .collect();
        samples.sort_unstable();
        let pct = |p: u64| -> u64 {
            if samples.is_empty() {
                return 0;
            }
            // nearest-rank: ceil(p/100 * n), 1-based → 0-based index.
            let rank = ((p as f64 / 100.0) * samples.len() as f64).ceil() as usize;
            samples[rank.clamp(1, samples.len()) - 1]
        };
        MetricsSnapshot {
            prompts: self.prompts.load(Ordering::Relaxed),
            turns: self.turns.load(Ordering::Relaxed),
            steps: self.steps.load(Ordering::Relaxed),
            errors: self.errors.load(Ordering::Relaxed),
            cancellations: self.cancellations.load(Ordering::Relaxed),
            latency_count: samples.len() as u64,
            latency_p50_ms: pct(50),
            latency_p95_ms: pct(95),
            latency_p99_ms: pct(99),
            latency_max_ms: samples.last().copied().unwrap_or(0),
        }
    }
}

/// A point-in-time view of [`AppMetrics`] (see [`AppMetrics::snapshot`]).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct MetricsSnapshot {
    pub prompts: u64,
    pub turns: u64,
    pub steps: u64,
    pub errors: u64,
    pub cancellations: u64,
    pub latency_count: u64,
    pub latency_p50_ms: u64,
    pub latency_p95_ms: u64,
    pub latency_p99_ms: u64,
    pub latency_max_ms: u64,
}

impl MetricsSnapshot {
    /// Render this snapshot in the [Prometheus text exposition format][fmt] (v0.0.4). `events_total` is
    /// the bus's published-event count (tracked outside the runner snapshot). Counters carry the
    /// `_total` suffix; turn latency is a `summary` over the bounded recent-sample window — the
    /// percentiles (with `max` as `quantile="1"`) plus `_count`, but no `_sum` (no running total is kept
    /// by design).
    ///
    /// [fmt]: https://prometheus.io/docs/instrumenting/exposition_formats/
    pub fn render_prometheus(&self, events_total: u64) -> String {
        use std::fmt::Write as _;

        fn counter(out: &mut String, name: &str, help: &str, value: u64) {
            let _ = writeln!(out, "# HELP {name} {help}");
            let _ = writeln!(out, "# TYPE {name} counter");
            let _ = writeln!(out, "{name} {value}");
        }
        fn quantile(out: &mut String, q: &str, value: u64) {
            let _ = writeln!(out, "opencode_turn_latency_ms{{quantile=\"{q}\"}} {value}");
        }

        let mut out = String::with_capacity(1024);
        counter(
            &mut out,
            "opencode_prompts_total",
            "Prompts admitted.",
            self.prompts,
        );
        counter(
            &mut out,
            "opencode_turns_total",
            "Turn-loop runs completed.",
            self.turns,
        );
        counter(
            &mut out,
            "opencode_steps_total",
            "Steps taken across all turns.",
            self.steps,
        );
        counter(
            &mut out,
            "opencode_errors_total",
            "Turn-loop runs that errored.",
            self.errors,
        );
        counter(
            &mut out,
            "opencode_cancellations_total",
            "Runs cancelled.",
            self.cancellations,
        );
        counter(
            &mut out,
            "opencode_events_published_total",
            "Events published.",
            events_total,
        );

        let _ = writeln!(
            out,
            "# HELP opencode_turn_latency_ms Turn-loop wall-clock latency (ms) over a recent-sample window."
        );
        let _ = writeln!(out, "# TYPE opencode_turn_latency_ms summary");
        quantile(&mut out, "0.5", self.latency_p50_ms);
        quantile(&mut out, "0.95", self.latency_p95_ms);
        quantile(&mut out, "0.99", self.latency_p99_ms);
        quantile(&mut out, "1", self.latency_max_ms);
        let _ = writeln!(out, "opencode_turn_latency_ms_count {}", self.latency_count);

        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_and_latency_percentiles() {
        let m = AppMetrics::default();
        m.record_prompt();
        m.record_prompt();
        m.record_error();
        m.record_cancellation();
        // 100 turns with latencies 1..=100 ms (each one step).
        for ms in 1..=100u64 {
            m.record_turn(1, ms);
        }

        let s = m.snapshot();
        assert_eq!(s.prompts, 2);
        assert_eq!(s.errors, 1);
        assert_eq!(s.cancellations, 1);
        assert_eq!(s.turns, 100);
        assert_eq!(s.steps, 100);
        assert_eq!(s.latency_count, 100);
        // nearest-rank over 1..=100: p50=50, p95=95, p99=99, max=100.
        assert_eq!(s.latency_p50_ms, 50);
        assert_eq!(s.latency_p95_ms, 95);
        assert_eq!(s.latency_p99_ms, 99);
        assert_eq!(s.latency_max_ms, 100);
    }

    #[test]
    fn latency_buffer_is_bounded() {
        let m = AppMetrics::default();
        for ms in 0..(LATENCY_CAPACITY as u64 + 500) {
            m.record_turn(1, ms);
        }
        let s = m.snapshot();
        assert_eq!(s.turns, LATENCY_CAPACITY as u64 + 500);
        // Only the most recent LATENCY_CAPACITY samples are retained.
        assert_eq!(s.latency_count, LATENCY_CAPACITY as u64);
        assert_eq!(s.latency_max_ms, LATENCY_CAPACITY as u64 + 499);
    }

    #[test]
    fn empty_snapshot_is_zero() {
        let s = AppMetrics::default().snapshot();
        assert_eq!(s.latency_count, 0);
        assert_eq!(s.latency_p95_ms, 0);
        assert_eq!(s.latency_max_ms, 0);
    }

    #[test]
    fn renders_prometheus_text_format() {
        let m = AppMetrics::default();
        m.record_prompt();
        for ms in 1..=100u64 {
            m.record_turn(1, ms);
        }
        let text = m.snapshot().render_prometheus(7);

        // Counter families: HELP + TYPE + value line.
        assert!(text.contains("# TYPE opencode_turns_total counter"));
        assert!(text.contains("\nopencode_turns_total 100\n"));
        assert!(text.contains("\nopencode_prompts_total 1\n"));
        assert!(text.contains("\nopencode_steps_total 100\n"));
        assert!(text.contains("\nopencode_events_published_total 7\n"));
        // Summary family: quantiles (max as quantile="1") + count, deliberately no `_sum`.
        assert!(text.contains("# TYPE opencode_turn_latency_ms summary"));
        assert!(text.contains("opencode_turn_latency_ms{quantile=\"0.5\"} 50\n"));
        assert!(text.contains("opencode_turn_latency_ms{quantile=\"0.95\"} 95\n"));
        assert!(text.contains("opencode_turn_latency_ms{quantile=\"0.99\"} 99\n"));
        assert!(text.contains("opencode_turn_latency_ms{quantile=\"1\"} 100\n"));
        assert!(text.contains("opencode_turn_latency_ms_count 100\n"));
        assert!(!text.contains("_sum"));
        assert!(text.ends_with('\n'));
    }

    #[test]
    fn renders_zero_when_empty() {
        let text = AppMetrics::default().snapshot().render_prometheus(0);
        assert!(text.contains("\nopencode_turns_total 0\n"));
        assert!(text.contains("opencode_turn_latency_ms_count 0\n"));
    }
}
