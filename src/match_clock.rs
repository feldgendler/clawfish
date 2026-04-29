//! Match time control types shared by the harness binary and any future
//! tooling that drives UCI engines.
//!
//! [`MatchTimeMode`] is the load-bearing seam between wallclock TC (ELOH.A)
//! and fixed-node-count TC (ELOH.C). The method [`MatchTimeMode::format_go_command`]
//! is the single place that knows how to emit the UCI `go` line given the
//! current mode and per-side clock state.

/// How the harness counts time for each move.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchTimeMode {
    /// Per-side wallclock tracked harness-side. ELOH.A.
    Wallclock,
    /// Send `go nodes N` to both engines; per-side clock unused. ELOH.C.
    Nodes(u64),
}

/// Per-side clock state for a single colour.
///
/// `remaining_ms` is signed so post-deduction values can go negative within
/// the grace window without saturating. The harness checks
/// `remaining_ms < -i64::from(harness_overhead_ms)` to detect a time forfeit.
#[derive(Debug, Clone, Copy)]
pub struct PerSideClock {
    /// Remaining time in milliseconds. May be negative post-deduction (within
    /// grace). Negative-on-`go` emission is total; the forfeit check fires
    /// before issuing `go` in normal flow but the formatter is unconditional.
    pub remaining_ms: i64,
    /// Per-move increment in milliseconds, credited after each move.
    pub increment_ms: u32,
}

impl MatchTimeMode {
    /// Format the `go` command per UCI's colour-absolute clock convention.
    ///
    /// `Wallclock`: `go wtime <W> btime <B> winc <Wi> binc <Bi>`.
    /// `Nodes(N)`:  `go nodes <N>` — `white` and `black` params ignored.
    ///
    /// UCI clocks are colour-absolute: the same string is sent to both
    /// engines in a given game; each engine extracts only its own side's
    /// fields. See plan §3.1 for the full rationale.
    pub fn format_go_command(self, white: PerSideClock, black: PerSideClock) -> String {
        match self {
            MatchTimeMode::Wallclock => format!(
                "go wtime {} btime {} winc {} binc {}",
                white.remaining_ms, black.remaining_ms, white.increment_ms, black.increment_ms,
            ),
            MatchTimeMode::Nodes(n) => {
                // colour clock params are irrelevant in nodes mode
                let _ = white;
                let _ = black;
                format!("go nodes {n}")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clock(remaining_ms: i64, increment_ms: u32) -> PerSideClock {
        PerSideClock {
            remaining_ms,
            increment_ms,
        }
    }

    #[test]
    fn wallclock_formats_colour_absolute() {
        let w = clock(10_000, 100);
        let b = clock(12_000, 100);
        assert_eq!(
            MatchTimeMode::Wallclock.format_go_command(w, b),
            "go wtime 10000 btime 12000 winc 100 binc 100"
        );
    }

    #[test]
    fn wallclock_asymmetric_increments() {
        // Pins that per-engine asymmetric TC (--opponent-tc-override) is
        // correctly propagated: each side carries its own engine's increment.
        let w = clock(5_000, 100);
        let b = clock(6_000, 200);
        assert_eq!(
            MatchTimeMode::Wallclock.format_go_command(w, b),
            "go wtime 5000 btime 6000 winc 100 binc 200"
        );
    }

    #[test]
    fn nodes_mode_emits_go_nodes_n() {
        // Clock params must be ignored regardless of values.
        let any = clock(99_999, 999);
        assert_eq!(
            MatchTimeMode::Nodes(50_000).format_go_command(any, any),
            "go nodes 50000"
        );
    }

    #[test]
    fn wallclock_zero_increment_emits_inc_fields_with_zero() {
        let w = clock(30_000, 0);
        let b = clock(30_000, 0);
        // Full-string match (consistent with `wallclock_formats_colour_absolute`).
        // An impl that emits `winc 00` or trailing whitespace would fail this.
        assert_eq!(
            MatchTimeMode::Wallclock.format_go_command(w, b),
            "go wtime 30000 btime 30000 winc 0 binc 0"
        );
    }

    #[test]
    fn wallclock_negative_remaining_emits_negative_field() {
        // Post-deduction, pre-forfeit-detection edge: the formatter is total.
        // The match loop checks forfeit BEFORE issuing go in normal flow, but
        // the formatter must not panic or clamp on negative inputs.
        let w = clock(-50, 0);
        let b = clock(5_000, 100);
        assert_eq!(
            MatchTimeMode::Wallclock.format_go_command(w, b),
            "go wtime -50 btime 5000 winc 0 binc 100"
        );
    }
}
