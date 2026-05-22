//! M6.H robustness gates — pure, dependency-light decision functions.
//!
//! These implement the content-/transport-validity gates from the roadmap
//! "Sub-protocol — robustness gates" table (gates 3, 5, 6, 7). The networking
//! glue lives in `super::reader` (gates 1, 2, 4 are ureq-config / reader-state
//! machine concerns). Keeping these pure makes them unit-testable without any
//! network or decompression. See `docs/plans/m6.h.md` §6.

use std::io;
use std::path::Path;

use crate::corpus::CorpusError;
use crate::corpus::pgn::PgnStats;

/// Classification of an HTTP status code for the retry policy (gate 3).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum StatusClass {
    /// 2xx — proceed.
    Ok2xx,
    /// 206 — partial content (a resume succeeded).
    Partial206,
    /// 416 — range not satisfiable (past EOF) → treat as clean EOS.
    RangeNotSatisfiable,
    /// 408 / 429 / 5xx — back off and retry.
    TransientHttp(u16),
    /// Other 4xx (404 / 410 / 403 / …) — permanent; do not infinite-loop.
    PermanentHttp(u16),
}

/// Gate 3: split an HTTP status by retry-permanence.
///
/// - 200 → `Ok2xx`; 206 → `Partial206`; 416 → `RangeNotSatisfiable`.
/// - 408 (Request Timeout), 429 (Too Many Requests), any 5xx → `TransientHttp`.
/// - any other 4xx (and any other non-2xx) → `PermanentHttp`.
pub fn classify_http_status(code: u16) -> StatusClass {
    match code {
        200 => StatusClass::Ok2xx,
        206 => StatusClass::Partial206,
        416 => StatusClass::RangeNotSatisfiable,
        // 2xx other than 206 — treat as OK (full body).
        201..=299 => StatusClass::Ok2xx,
        // Explicitly-transient client codes + all 5xx.
        408 | 429 => StatusClass::TransientHttp(code),
        500..=599 => StatusClass::TransientHttp(code),
        // Any other 4xx (404/410/403/400/…) — permanent.
        _ => StatusClass::PermanentHttp(code),
    }
}

/// Gate 5: magic-byte sniff — does `prefix` *begin* with a PGN tag pair?
///
/// **Prefix-anchored, not `contains`**: the first non-whitespace bytes must be
/// `[` immediately followed by a known Seven-Tag-Roster token
/// (`Event`/`Site`/`Date`/`Round`/`White`/`Black`/`Result`) and then a space
/// or quote. This rejects an HTML body that merely embeds a `[Event ` substring
/// mid-page (the adversarial soft-404 / CDN-interstitial case).
pub fn sniff_is_pgn(prefix: &[u8]) -> bool {
    // Skip leading ASCII whitespace (some dumps open with a blank line).
    let mut i = 0;
    while i < prefix.len() && prefix[i].is_ascii_whitespace() {
        i += 1;
    }
    // First non-whitespace byte must open a tag.
    if prefix.get(i) != Some(&b'[') {
        return false;
    }
    let rest = &prefix[i + 1..];
    const TOKENS: [&[u8]; 7] = [
        b"Event", b"Site", b"Date", b"Round", b"White", b"Black", b"Result",
    ];
    for t in TOKENS {
        // A known Seven-Tag-Roster token immediately after `[`, delimited by a
        // space or the value-opening quote, is a genuine PGN tag pair.
        if rest.starts_with(t) && matches!(rest.get(t.len()), Some(b' ') | Some(b'"')) {
            return true;
        }
    }
    false
}

/// Gate 6: parse-sanity over a decompressed prefix's `PgnStats`.
///
/// `true` iff `games_seen > 0` and `parse_failures / games_seen <=
/// max_fail_ratio`. A zero-`games_seen` prefix fails (an empty / non-PGN body
/// that nevertheless passed the byte sniff).
pub fn parse_sanity_ok(stats: &PgnStats, max_fail_ratio: f64) -> bool {
    if stats.games_seen == 0 {
        return false;
    }
    (stats.parse_failures as f64) / (stats.games_seen as f64) <= max_fail_ratio
}

/// Free space (bytes) available to a non-privileged process on the filesystem
/// containing `dir`, via `libc::statvfs` (`f_bavail * f_frsize` — `f_frsize`,
/// the fundamental block size, NOT `f_bsize`, the preferred I/O size).
pub fn available_disk_bytes(dir: &Path) -> io::Result<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let cpath = CString::new(dir.as_os_str().as_bytes())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    // SAFETY: `statvfs` reads `*cpath` (a valid NUL-terminated C string) and
    // writes the all-zeroed `stat` out-param; both pointers are valid for the
    // call. Non-zero return ⇒ failure, handled below.
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(cpath.as_ptr(), &mut stat) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    // `f_frsize` (fundamental block size), NOT `f_bsize` (preferred I/O size,
    // often 1 MiB on macOS — would over-report by a large factor). `f_bavail`
    // is blocks available to a non-privileged process.
    Ok((stat.f_bavail as u64).saturating_mul(stat.f_frsize as u64))
}

/// Gate 7: refuse to start a fetch if free space on `dir`'s filesystem is below
/// `need_bytes`. Returns `CorpusError::Io` on a `statvfs` failure (treated as a
/// hard error — better to refuse than risk a mid-crunch ENOSPC).
pub fn disk_precheck(dir: &Path, need_bytes: u64) -> Result<(), CorpusError> {
    // A `statvfs` syscall failure is an `Io` error; a successful query that
    // reports too little free space is a policy rejection (`QualityGate`).
    let avail = available_disk_bytes(dir)?;
    if avail < need_bytes {
        return Err(CorpusError::QualityGate(format!(
            "insufficient free disk on {}: {avail} bytes free, need {need_bytes}",
            dir.display()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_http_status_split() {
        assert_eq!(classify_http_status(200), StatusClass::Ok2xx);
        assert_eq!(classify_http_status(206), StatusClass::Partial206);
        assert_eq!(classify_http_status(416), StatusClass::RangeNotSatisfiable);
        // Transient.
        assert_eq!(classify_http_status(408), StatusClass::TransientHttp(408));
        assert_eq!(classify_http_status(429), StatusClass::TransientHttp(429));
        assert_eq!(classify_http_status(500), StatusClass::TransientHttp(500));
        assert_eq!(classify_http_status(503), StatusClass::TransientHttp(503));
        // Permanent (other 4xx).
        assert_eq!(classify_http_status(404), StatusClass::PermanentHttp(404));
        assert_eq!(classify_http_status(410), StatusClass::PermanentHttp(410));
        assert_eq!(classify_http_status(403), StatusClass::PermanentHttp(403));
        assert_eq!(classify_http_status(400), StatusClass::PermanentHttp(400));
        // A 2xx other than 200/206 is still OK (pins the 201..=299 arm).
        assert_eq!(classify_http_status(201), StatusClass::Ok2xx);
        assert_eq!(classify_http_status(204), StatusClass::Ok2xx);
    }

    #[test]
    fn sniff_is_pgn_prefix_anchored() {
        // Every Seven-Tag-Roster token is accepted as a valid prefix.
        assert!(sniff_is_pgn(b"[Event \"Rated Blitz game\"]\n[Site \"x\"]"));
        assert!(sniff_is_pgn(b"[Site \"https://lichess.org\"]"));
        assert!(sniff_is_pgn(b"[Date \"2024.01.01\"]"));
        assert!(sniff_is_pgn(b"[Round \"-\"]"));
        assert!(sniff_is_pgn(b"[White \"player\"]"));
        assert!(sniff_is_pgn(b"[Black \"player\"]"));
        assert!(sniff_is_pgn(b"[Result \"1-0\"]"));
        // Leading whitespace is allowed (some dumps start with a blank line).
        assert!(sniff_is_pgn(b"\n\r\n  [Event \"x\"]"));
        // Plain HTML — first non-ws byte is `<`, rejected.
        assert!(!sniff_is_pgn(b"<!DOCTYPE html><html><head>"));
        assert!(!sniff_is_pgn(b"<html>not pgn</html>"));
        // ADVERSARIAL: HTML body that merely *embeds* a PGN-tag-looking
        // substring mid-page. `contains` would pass it; the prefix anchor must
        // reject it (first non-ws byte is `<`, not `[`).
        assert!(!sniff_is_pgn(
            b"<html><body>[Event \"fake\"] inside html</body></html>"
        ));
        // `[` followed by a NON-tag token is not a PGN tag.
        assert!(!sniff_is_pgn(b"[garbage data here"));
        // A known token as a PREFIX of a longer word, with no delimiter, is NOT
        // a tag — pins the `&&` (both prefix AND delimiter), not `||`.
        assert!(!sniff_is_pgn(b"[Eventually \"x\"]"));
        assert!(!sniff_is_pgn(b"[Whitewash \"x\"]"));
        // Empty / tiny.
        assert!(!sniff_is_pgn(b""));
        assert!(!sniff_is_pgn(b"[Ev"));
    }

    fn stats(seen: u64, fails: u64) -> PgnStats {
        PgnStats {
            games_seen: seen,
            parse_failures: fails,
            games_emitted: 0,
            no_result_dropped: 0,
        }
    }

    #[test]
    fn parse_sanity_threshold() {
        // 10% exactly passes (≤).
        assert!(parse_sanity_ok(&stats(100, 10), 0.10));
        // 5% passes.
        assert!(parse_sanity_ok(&stats(100, 5), 0.10));
        // 11% fails.
        assert!(!parse_sanity_ok(&stats(100, 11), 0.10));
        // Zero games seen ⇒ fail (an empty / non-PGN body that passed the byte
        // sniff must not be accepted as "0/0 = clean").
        assert!(!parse_sanity_ok(&stats(0, 0), 0.10));
    }

    #[test]
    fn available_disk_bytes_plausible_for_tmp() {
        let n = available_disk_bytes(&std::env::temp_dir()).expect("statvfs ok");
        // > 1 MiB: any real filesystem has this much free, and it rejects a
        // degenerate `Ok(1)` stub (a real `statvfs` reports the true free space,
        // not a constant).
        assert!(n > 1_000_000, "temp dir free space implausibly small: {n}");
    }

    #[test]
    fn disk_precheck_rejects_when_below_floor() {
        // A policy rejection (statvfs SUCCEEDS, but free < need) is a
        // `QualityGate` error — distinct from a `statvfs` syscall failure
        // (`Io`). Needing an impossible amount forces the policy path without a
        // real full disk.
        let r = disk_precheck(&std::env::temp_dir(), u64::MAX);
        assert!(
            matches!(r, Err(CorpusError::QualityGate(_))),
            "below-floor must be a QualityGate rejection, not Io: {r:?}"
        );
        // A trivial need passes.
        assert!(disk_precheck(&std::env::temp_dir(), 1).is_ok());
    }
}
