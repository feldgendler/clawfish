//! M6.H URL catalog + per-URL fetch-state bookkeeping.
//!
//! Lichess monthly dumps follow a predictable URL pattern, so the fetcher can
//! auto-construct the default Lichess URL. CCRL distributes as `.7z` (supported
//! since the 2026-05-22 amendment — `sevenz_rust2`) but its filenames embed a
//! game count (`CCRL-4040.[N].pgn.7z`), so there is no stable auto-constructible
//! URL — **CCRL has no auto-default**; the operator passes `--url` to a current
//! CCRL archive (`.7z`/`.zip`/`.pgn.zst`; see `docs/data-catalog.md`). The
//! per-URL state file
//! (`fetch-state.json`) records how much each URL has contributed so the M6.I
//! driver can decide whether a URL has more to give. JSON reuses
//! `corpus::manifest`'s tested parser (no `serde` dep). See
//! `docs/plans/m6.h.md` §7.

use std::path::Path;

use crate::corpus::manifest::{JsonVal, json_escape, parse_json};
use crate::corpus::store::atomic_write;
use crate::corpus::{CorpusError, Source};

/// Default Lichess month when `--lichess-month` is omitted. A pinned,
/// known-present standard-rated dump (the catalog's auto-default). Kept
/// deliberately old/small so the no-arg path is cheap; the operator overrides
/// with `--lichess-month YYYY-MM` for a larger, more recent month.
pub const DEFAULT_LICHESS_MONTH: (u32, u32) = (2014, 7);

/// Construct the Lichess standard-rated monthly dump URL.
///
/// Pattern: `https://database.lichess.org/standard/lichess_db_standard_rated_YYYY-MM.pgn.zst`
/// (`MM` zero-padded). `month` is 1..=12 (the CLI parse guards the range).
pub fn lichess_url(year: u32, month: u32) -> String {
    format!(
        "https://database.lichess.org/standard/lichess_db_standard_rated_{year:04}-{month:02}.pgn.zst"
    )
}

/// Resolve the default URL for `source` when `--url` is omitted.
///
/// - `LichessOpen` → `Some(lichess_url(month | DEFAULT_LICHESS_MONTH))`.
/// - `Ccrl` → `None` (the count-bearing `.7z` filename isn't auto-constructible;
///   operator must pass `--url`).
/// - Self-play sources → `None` (not network-fetched).
pub fn default_url(source: Source, lichess_month: Option<(u32, u32)>) -> Option<String> {
    match source {
        Source::LichessOpen => {
            let (y, m) = lichess_month.unwrap_or(DEFAULT_LICHESS_MONTH);
            Some(lichess_url(y, m))
        }
        Source::Ccrl | Source::SelfPlayOnBook | Source::SelfPlayOffBook => None,
    }
}

/// How a URL's fetch attempt terminated (recorded in `fetch-state.json`).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum TermKind {
    /// Reached the requested position target (URL likely has more to give).
    EarlyTarget,
    /// Stream reached end-of-file (URL drained).
    Eos,
    /// SIGINT / operator stop.
    Stopped,
}

impl TermKind {
    fn as_str(self) -> &'static str {
        match self {
            TermKind::EarlyTarget => "early_target",
            TermKind::Eos => "eos",
            TermKind::Stopped => "stopped",
        }
    }
    fn from_str(s: &str) -> Option<TermKind> {
        match s {
            "early_target" => Some(TermKind::EarlyTarget),
            "eos" => Some(TermKind::Eos),
            "stopped" => Some(TermKind::Stopped),
            _ => None,
        }
    }
}

/// One per-URL record in `fetch-state.json`.
#[derive(Clone, PartialEq, Debug)]
pub struct FetchStateEntry {
    /// The fetched URL.
    pub url: String,
    /// Total positions this URL has contributed across all fetches.
    pub positions_contributed: u64,
    /// Total compressed bytes received across all fetches.
    pub bytes_received: u64,
    /// How the most recent fetch of this URL terminated.
    pub last_termination: TermKind,
}

/// The whole `fetch-state.json` document.
#[derive(Clone, PartialEq, Debug, Default)]
pub struct FetchState {
    /// One entry per touched URL.
    pub entries: Vec<FetchStateEntry>,
}

impl FetchState {
    /// Load from `path`. A missing or malformed file yields an empty state
    /// (never an error / panic — the file is best-effort bookkeeping; the shard
    /// is the source of truth).
    pub fn load(path: &Path) -> FetchState {
        let bytes = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(_) => return FetchState::default(),
        };
        parse_state(&bytes).unwrap_or_default()
    }

    /// Merge one fetch's result into the entry for `url` (creating it if new):
    /// adds to `positions_contributed`/`bytes_received`, overwrites
    /// `last_termination`.
    pub fn record(&mut self, url: &str, positions: u64, bytes: u64, term: TermKind) {
        if let Some(e) = self.entries.iter_mut().find(|e| e.url == url) {
            e.positions_contributed += positions;
            e.bytes_received += bytes;
            e.last_termination = term;
        } else {
            self.entries.push(FetchStateEntry {
                url: url.to_string(),
                positions_contributed: positions,
                bytes_received: bytes,
                last_termination: term,
            });
        }
    }

    /// Atomically write to `path` (hand-serialized JSON; `store::atomic_write`).
    pub fn save(&self, path: &Path) -> Result<(), CorpusError> {
        let mut s = String::from("{\n  \"entries\": [\n");
        for (i, e) in self.entries.iter().enumerate() {
            s.push_str("    {\"url\": \"");
            s.push_str(&json_escape(&e.url));
            s.push_str(&format!(
                "\", \"positions_contributed\": {}, \"bytes_received\": {}, \"last_termination\": \"{}\"}}",
                e.positions_contributed,
                e.bytes_received,
                e.last_termination.as_str()
            ));
            if i + 1 < self.entries.len() {
                s.push(',');
            }
            s.push('\n');
        }
        s.push_str("  ]\n}\n");
        atomic_write(path, s.as_bytes())
    }
}

/// Parse the `fetch-state.json` shape via the shared minimal JSON parser.
/// Returns `None` on any structural mismatch (caller falls back to empty).
fn parse_state(src: &str) -> Option<FetchState> {
    let top = parse_json(src).ok()?;
    let obj = match top {
        JsonVal::Obj(o) => o,
        _ => return None,
    };
    let entries_val = obj.iter().find(|(k, _)| k == "entries").map(|(_, v)| v)?;
    let arr = match entries_val {
        JsonVal::Arr(a) => a,
        _ => return None,
    };
    let mut entries = Vec::with_capacity(arr.len());
    for item in arr {
        let fields = match item {
            JsonVal::Obj(o) => o,
            _ => return None,
        };
        let get = |k: &str| fields.iter().find(|(fk, _)| fk == k).map(|(_, v)| v);
        let url = match get("url")? {
            JsonVal::Str(s) => s.clone(),
            _ => return None,
        };
        let positions_contributed = num_u64(get("positions_contributed")?)?;
        let bytes_received = num_u64(get("bytes_received")?)?;
        let last_termination = match get("last_termination")? {
            JsonVal::Str(s) => TermKind::from_str(s)?,
            _ => return None,
        };
        entries.push(FetchStateEntry {
            url,
            positions_contributed,
            bytes_received,
            last_termination,
        });
    }
    Some(FetchState { entries })
}

fn num_u64(v: &JsonVal) -> Option<u64> {
    match v {
        JsonVal::Num(n) if *n >= 0.0 && (*n as u64 as f64 - n).abs() < 0.5 => Some(*n as u64),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lichess_url_pattern_golden() {
        assert_eq!(
            lichess_url(2024, 1),
            "https://database.lichess.org/standard/lichess_db_standard_rated_2024-01.pgn.zst",
            "month must be zero-padded"
        );
        assert_eq!(
            lichess_url(2023, 12),
            "https://database.lichess.org/standard/lichess_db_standard_rated_2023-12.pgn.zst"
        );
    }

    #[test]
    fn default_url_lichess_auto_ccrl_requires_url() {
        // Lichess auto-constructs (with an override and a pinned default).
        assert_eq!(
            default_url(Source::LichessOpen, Some((2022, 6))).as_deref(),
            Some("https://database.lichess.org/standard/lichess_db_standard_rated_2022-06.pgn.zst")
        );
        let def = default_url(Source::LichessOpen, None).unwrap();
        assert!(def.starts_with("https://database.lichess.org/standard/"));
        assert!(def.ends_with(".pgn.zst"));
        // CCRL (and self-play) have NO auto-default — operator must pass --url.
        assert_eq!(default_url(Source::Ccrl, None), None);
        assert_eq!(default_url(Source::SelfPlayOnBook, None), None);
        assert_eq!(default_url(Source::SelfPlayOffBook, None), None);
    }

    #[test]
    fn term_kind_str_roundtrip_all_variants() {
        for t in [TermKind::EarlyTarget, TermKind::Eos, TermKind::Stopped] {
            assert_eq!(TermKind::from_str(t.as_str()), Some(t));
        }
        assert_eq!(TermKind::from_str("nonsense"), None);
    }

    #[test]
    fn fetch_state_rejects_non_integer_count() {
        // A fractional / negative count must be rejected (→ empty state), not
        // silently truncated — pins `num_u64`'s integer + non-negative guard.
        let dir = std::env::temp_dir().join(format!("clawfish-fsbad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let frac = dir.join("frac.json");
        std::fs::write(
            &frac,
            br#"{"entries":[{"url":"x","positions_contributed":5.5,"bytes_received":0,"last_termination":"eos"}]}"#,
        )
        .unwrap();
        assert!(
            FetchState::load(&frac).entries.is_empty(),
            "fractional count must be rejected"
        );
        let neg = dir.join("neg.json");
        std::fs::write(
            &neg,
            br#"{"entries":[{"url":"x","positions_contributed":-5,"bytes_received":0,"last_termination":"eos"}]}"#,
        )
        .unwrap();
        assert!(
            FetchState::load(&neg).entries.is_empty(),
            "negative count must be rejected"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fetch_state_roundtrip_preserves_early_target() {
        // A solo EarlyTarget-terminated entry exercises the `early_target`
        // from_str arm on load (the merge test ends in Eos).
        let dir = std::env::temp_dir().join(format!("clawfish-fset-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("fs.json");
        let mut st = FetchState::default();
        st.record("https://z/q.zst", 9, 99, TermKind::EarlyTarget);
        st.save(&path).unwrap();
        let back = FetchState::load(&path);
        assert_eq!(back.entries.len(), 1);
        assert_eq!(back.entries[0].last_termination, TermKind::EarlyTarget);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fetch_state_json_roundtrip() {
        let dir = std::env::temp_dir().join(format!("clawfish-fetchstate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("fetch-state.json");

        let mut st = FetchState::default();
        st.record("https://a/x.zst", 100, 5000, TermKind::EarlyTarget);
        st.record("https://a/x.zst", 50, 2000, TermKind::Eos); // merges into the same url
        st.record("https://b/y.zip", 7, 900, TermKind::Stopped);
        st.save(&path).unwrap();

        let back = FetchState::load(&path);
        // url "a" merged: positions 150, bytes 7000, last termination Eos.
        let a = back
            .entries
            .iter()
            .find(|e| e.url == "https://a/x.zst")
            .unwrap();
        assert_eq!(a.positions_contributed, 150);
        assert_eq!(a.bytes_received, 7000);
        assert_eq!(a.last_termination, TermKind::Eos);
        let b = back
            .entries
            .iter()
            .find(|e| e.url == "https://b/y.zip")
            .unwrap();
        assert_eq!(b.positions_contributed, 7);
        assert_eq!(b.last_termination, TermKind::Stopped);

        // A missing file ⇒ empty state, never an error.
        let empty = FetchState::load(&dir.join("does-not-exist.json"));
        assert!(empty.entries.is_empty());
        // A malformed file ⇒ empty state, never a panic.
        std::fs::write(dir.join("bad.json"), b"{not json").unwrap();
        let bad = FetchState::load(&dir.join("bad.json"));
        assert!(bad.entries.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
