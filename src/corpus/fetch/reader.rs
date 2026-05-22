//! M6.H resumable HTTP reader + the per-call / per-attempt state it shares
//! with the ingest closure.
//!
//! `ResumableHttpReader<R, F>` is the load-bearing primitive: a `Read` adapter
//! that sits *below* the decompressor and transparently re-establishes the
//! byte stream (HTTP `Range` resume) on a transient failure or a stall, keeping
//! the decoder alive (zero re-decompression). It is generic over the inner
//! reader `R` and a reconnect closure `F: FnMut(consumed) -> Result<R, _>`, so
//! the whole state machine is unit-testable with an in-memory `R` and a
//! scripted `F` — no `ureq`, no socket. The production opener (`http_opener`)
//! constructs `F` from a `ureq::Agent`. See `docs/plans/m6.h.md` §5.

use std::cell::{Cell, RefCell};
use std::io::{self, Read};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

/// Why a `read` returned `Ok(0)`: an intentional early-stop (target reached /
/// SIGINT) vs. a genuine end of the byte stream. Drives the `Eos` vs
/// `EarlyTarget` classification (so a true-EOS-at-target reports drained).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum EofReason {
    /// Stopped because `positions_ingested >= target`.
    Target,
    /// Stopped because the inner stream genuinely ended.
    InnerEof,
}

/// Why an attempt failed (read out of `AttemptState` after `stream_pgn`
/// returns, since the reason propagates through the decoder as an opaque
/// `io::Error`).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum AttemptFailure {
    /// Watchdog: no game parsed within `stall_timeout`, escalation exhausted.
    Stall,
    /// A transient `inner`/reconnect error that backoff should retry.
    Transient(String),
    /// A permanent failure (4xx) — abort, do not infinite-loop.
    Permanent(String),
    /// A resume request returned `200` (server ignored `Range`) — can't feed
    /// byte-0 into a mid-stream decoder; force a byte-0 restart.
    RangeIgnored,
}

/// Result of a reconnect attempt by the opener `F`.
#[derive(Debug)]
pub enum ReconnectError {
    /// Permanent HTTP error (4xx) — bubble to abort.
    Permanent(String),
    /// Transient (timeout / 5xx / connect) — bubble to backoff.
    Transient(String),
    /// Server answered `200` to a `Range` request — byte-0 restart.
    RangeIgnored,
}

/// Per-CALL state — persists across all (byte-0) attempts of one
/// `stream_to_ingest`. The accounting that makes byte-0 restart idempotent.
pub struct CallState {
    /// Target = NEW positions to APPEND this call ("give me N more").
    pub target: u64,
    /// Bumped ONLY on a real append; never on skipped re-seen games; never
    /// reset across attempts.
    pub positions_ingested: Cell<u64>,
    /// Bumped ONLY on a real append.
    pub games_emitted: Cell<u64>,
    /// Highest game_id appended this call; a byte-0 restart skips `<=` this.
    pub max_appended_game_id: Cell<Option<u64>>,
    /// SIGINT / operator stop.
    pub stop: Arc<AtomicBool>,
    /// A FATAL `commit_game` failure (disk full / I/O append error). Set by the
    /// ingest closure; halts the parse like `stop`, but is classified as a
    /// permanent abort so `stream_to_ingest` returns `Err` instead of silently
    /// under-counting. `None` ⇒ no abort.
    pub commit_abort: RefCell<Option<String>>,
}

impl CallState {
    /// New per-call state for a `target`-position request.
    pub fn new(target: u64, stop: Arc<AtomicBool>) -> Rc<CallState> {
        Rc::new(CallState {
            target,
            positions_ingested: Cell::new(0),
            games_emitted: Cell::new(0),
            max_appended_game_id: Cell::new(None),
            stop,
            commit_abort: RefCell::new(None),
        })
    }

    /// True once `positions_ingested >= target` (early-stop trigger).
    pub fn target_reached(&self) -> bool {
        self.target != 0 && self.positions_ingested.get() >= self.target
    }

    /// Record a fatal `commit_game` failure (first one wins) so the parse halts
    /// and the attempt classifies as a permanent abort.
    pub fn set_commit_abort(&self, msg: String) {
        let mut slot = self.commit_abort.borrow_mut();
        if slot.is_none() {
            *slot = Some(msg);
        }
    }

    /// `true` iff a fatal `commit_game` failure was recorded.
    pub fn commit_aborted(&self) -> bool {
        self.commit_abort.borrow().is_some()
    }
}

/// Per-ATTEMPT state — fresh on every byte-0 attempt.
pub struct AttemptState {
    /// Compressed-body byte offset within THIS attempt (the `Range` base).
    pub consumed: Cell<u64>,
    /// Timestamp of the most recent liveness event (game parsed, or byte read
    /// when `bytes_are_progress`). Drives the stall watchdog.
    pub last_game_at: Cell<Instant>,
    /// Games (or progress events) since the last reconnect — gates escalation.
    pub games_since_resume: Cell<u64>,
    /// Set by the reader before returning `Ok(0)`.
    pub eof_reason: Cell<Option<EofReason>>,
    /// Set by the reader before returning a non-EOF `Err`.
    pub failure: Cell<Option<AttemptFailure>>,
}

impl AttemptState {
    /// Fresh attempt state; `last_game_at` seeded to now so stream warm-up gets
    /// the full stall window.
    pub fn new() -> Rc<AttemptState> {
        Rc::new(AttemptState {
            consumed: Cell::new(0),
            last_game_at: Cell::new(Instant::now()),
            games_since_resume: Cell::new(0),
            eof_reason: Cell::new(None),
            failure: Cell::new(None),
        })
    }

    /// Liveness bump — called for EVERY yielded game (appended or skipped) so
    /// the watchdog sees a re-seen game as proof the stream is healthy.
    pub fn note_progress(&self) {
        self.last_game_at.set(Instant::now());
        self.games_since_resume
            .set(self.games_since_resume.get() + 1);
    }
}

/// A `Read` adapter that transparently `Range`-resumes its inner byte source.
pub struct ResumableHttpReader<R: Read, F: FnMut(u64) -> Result<R, ReconnectError>> {
    inner: R,
    reconnect: F,
    call: Rc<CallState>,
    att: Rc<AttemptState>,
    stall_timeout: Duration,
    max_noprogress_resumes: u32,
    noprogress_resumes: u32,
    /// CCRL temp-download phase: raw bytes count as progress (no games yet).
    bytes_are_progress: bool,
}

impl<R: Read, F: FnMut(u64) -> Result<R, ReconnectError>> ResumableHttpReader<R, F> {
    /// Construct the reader over an already-opened `inner` (offset 0) with a
    /// reconnect closure for resumes.
    pub fn new(
        inner: R,
        reconnect: F,
        call: Rc<CallState>,
        att: Rc<AttemptState>,
        stall_timeout: Duration,
        max_noprogress_resumes: u32,
        bytes_are_progress: bool,
    ) -> Self {
        ResumableHttpReader {
            inner,
            reconnect,
            call,
            att,
            stall_timeout,
            max_noprogress_resumes,
            noprogress_resumes: 0,
            bytes_are_progress,
        }
    }
}

impl<R: Read, F: FnMut(u64) -> Result<R, ReconnectError>> ResumableHttpReader<R, F> {
    /// Re-establish the byte stream from `consumed` (in-attempt `Range` resume).
    /// Counts toward the no-progress escalation bound; on escalation or a
    /// permanent/range-ignored reconnect, records the failure and returns the
    /// `Err` that bubbles up (→ outer byte-0 restart or abort).
    fn do_reconnect(&mut self) -> io::Result<()> {
        // A resume that yielded no game since the previous resume is "no
        // progress"; consecutive no-progress resumes escalate (defeats a
        // byte-alive-but-semantically-dead stream).
        if self.att.games_since_resume.get() == 0 {
            self.noprogress_resumes += 1;
        } else {
            self.noprogress_resumes = 0;
        }
        self.att.games_since_resume.set(0);
        if self.noprogress_resumes > self.max_noprogress_resumes {
            self.att.failure.set(Some(AttemptFailure::Stall));
            return Err(io::Error::other("stall: escalate to byte-0 restart"));
        }
        match (self.reconnect)(self.att.consumed.get()) {
            Ok(r) => {
                self.inner = r;
                // Give the freshly-reconnected stream a full stall window to
                // yield a game before the watchdog re-fires — otherwise a stall
                // would instantly re-stall (clock still stale) and escalate to a
                // byte-0 restart, throwing away every `consumed` byte. Escalation
                // is driven by `noprogress_resumes` (a reconnect that yields no
                // game), not by this clock, so resetting it here does not weaken
                // the anti-livelock bound.
                self.att.last_game_at.set(Instant::now());
                Ok(())
            }
            Err(ReconnectError::Permanent(m)) => {
                self.att
                    .failure
                    .set(Some(AttemptFailure::Permanent(m.clone())));
                Err(io::Error::other(m))
            }
            Err(ReconnectError::RangeIgnored) => {
                self.att.failure.set(Some(AttemptFailure::RangeIgnored));
                Err(io::Error::other("server ignored Range (200 on resume)"))
            }
            Err(ReconnectError::Transient(m)) => {
                self.att
                    .failure
                    .set(Some(AttemptFailure::Transient(m.clone())));
                Err(io::Error::other(m))
            }
        }
    }
}

impl<R: Read, F: FnMut(u64) -> Result<R, ReconnectError>> Read for ResumableHttpReader<R, F> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            // 0. Fatal commit-abort (disk full / I/O append error). Halt the
            //    parse immediately; the classifier surfaces it as a permanent
            //    abort (checked before `stop`, so the error isn't masked).
            if self.call.commit_aborted() {
                return Ok(0);
            }
            // 1. Clean stop (SIGINT / operator). Classifier checks `stop` first,
            //    so the EOF reason is irrelevant here.
            if self.call.stop.load(std::sync::atomic::Ordering::Relaxed) {
                return Ok(0);
            }
            // 2. Early-termination with a disambiguating peek: once the target
            //    is met, one inner read decides drained-at-target (InnerEof →
            //    Eos) vs more-available (Target → EarlyTarget). Peeked bytes are
            //    discarded (we are stopping either way).
            if self.call.target_reached() {
                match self.inner.read(buf) {
                    Ok(0) => self.att.eof_reason.set(Some(EofReason::InnerEof)),
                    _ => self.att.eof_reason.set(Some(EofReason::Target)),
                }
                return Ok(0);
            }
            // 3. Stall watchdog — keyed on games parsed (last_game_at is bumped
            //    only by a yielded game, or by bytes when bytes_are_progress).
            if self.att.last_game_at.get().elapsed() > self.stall_timeout {
                self.do_reconnect()?;
                continue;
            }
            // 4. Read; any inner error is network-resumable → reconnect.
            match self.inner.read(buf) {
                Ok(0) => {
                    self.att.eof_reason.set(Some(EofReason::InnerEof));
                    return Ok(0);
                }
                Ok(n) => {
                    self.att.consumed.set(self.att.consumed.get() + n as u64);
                    if self.bytes_are_progress {
                        self.att.note_progress();
                    }
                    return Ok(n);
                }
                Err(_) => {
                    self.do_reconnect()?;
                    continue;
                }
            }
        }
    }
}

// Design note: ALL `inner.read` errors are treated as network-resumable (a body
// read can only fail for transport reasons — timeout, reset, premature EOF —
// all of which a `Range` reconnect recovers). Permanence is HTTP-status-driven
// and surfaces only from the reconnect closure (`ReconnectError::Permanent` for
// a 4xx). So there is no `is_timeout_or_transient` predicate: the reader
// reconnects on any inner error; the give-up paths are the reconnect closure
// returning `Permanent`/`RangeIgnored`, or the no-progress escalation bound.

// --- Production ureq wiring (the only ureq-touching code) -------------------

/// The production inner-reader type: a boxed `Read` (the ureq body reader, or
/// an empty reader for a 416 "range past EOF" → clean EOS).
#[cfg(feature = "corpus-fetch")]
pub type HttpBody = Box<dyn Read>;

/// Build the configured `ureq::Agent`: connect timeout, recv-body deadline =
/// `stall_timeout` (the watchdog heartbeat — a hung socket hands control back
/// within this window), redirect cap, and OS-trust-store platform verification
/// (macOS Security framework) for channel identity (gate 1).
#[cfg(feature = "corpus-fetch")]
pub fn build_agent(
    connect_timeout: Duration,
    stall_timeout: Duration,
    max_redirects: u32,
) -> ureq::Agent {
    use ureq::tls::{RootCerts, TlsConfig};
    ureq::Agent::config_builder()
        .timeout_connect(Some(connect_timeout))
        .timeout_recv_body(Some(stall_timeout))
        .max_redirects(max_redirects)
        .tls_config(
            TlsConfig::builder()
                .root_certs(RootCerts::PlatformVerifier)
                .build(),
        )
        .build()
        .new_agent()
}

/// Build the production reconnect opener: `offset -> [Range] GET -> body
/// reader`. Maps HTTP status → `ReconnectError` (gate 3). On the first open
/// (offset 0) a `200` is success; on a resume (offset > 0) a `200` means the
/// server ignored `Range` → `RangeIgnored` (force a byte-0 restart). A `416`
/// (range past EOF) is a clean EOS → an empty reader.
#[cfg(feature = "corpus-fetch")]
pub fn http_opener(
    agent: ureq::Agent,
    url: String,
) -> impl FnMut(u64) -> Result<HttpBody, ReconnectError> {
    use crate::corpus::fetch::gates::{StatusClass, classify_http_status};
    move |offset| {
        let mut req = agent.get(&url);
        if offset > 0 {
            req = req.header("Range", &format!("bytes={offset}-"));
        }
        match req.call() {
            Ok(resp) => {
                let code = resp.status().as_u16();
                match classify_http_status(code) {
                    // 200: full body. On a resume this means Range was ignored.
                    StatusClass::Ok2xx => {
                        if offset > 0 {
                            Err(ReconnectError::RangeIgnored)
                        } else {
                            Ok(Box::new(resp.into_body().into_reader()))
                        }
                    }
                    // 206: partial content — the resume was honored.
                    StatusClass::Partial206 => Ok(Box::new(resp.into_body().into_reader())),
                    // 416: nothing past `offset` — clean EOS.
                    StatusClass::RangeNotSatisfiable => Ok(Box::new(std::io::empty())),
                    StatusClass::TransientHttp(c) => {
                        Err(ReconnectError::Transient(format!("http {c}")))
                    }
                    StatusClass::PermanentHttp(c) => {
                        Err(ReconnectError::Permanent(format!("http {c}")))
                    }
                }
            }
            // ureq returns Err(StatusCode) for non-2xx by default.
            Err(ureq::Error::StatusCode(code)) => match classify_http_status(code) {
                StatusClass::RangeNotSatisfiable => Ok(Box::new(std::io::empty())),
                StatusClass::TransientHttp(c) => {
                    Err(ReconnectError::Transient(format!("http {c}")))
                }
                StatusClass::PermanentHttp(c) => {
                    Err(ReconnectError::Permanent(format!("http {c}")))
                }
                // 2xx-as-error shouldn't happen; treat as transient.
                _ => Err(ReconnectError::Transient(format!("http {code}"))),
            },
            // Timeouts + transport errors are network-transient.
            Err(e) => Err(ReconnectError::Transient(e.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    /// In-memory `Read` over a shared byte vector, optionally erroring once when
    /// the read offset reaches `err_at`. Models the HTTP body for the reader's
    /// state-machine tests (no socket, no ureq).
    struct ScriptedReader {
        data: std::rc::Rc<Vec<u8>>,
        pos: usize,
        err_at: Option<usize>,
        err_kind: io::ErrorKind,
    }

    impl ScriptedReader {
        fn from(data: std::rc::Rc<Vec<u8>>, start: usize) -> Self {
            ScriptedReader {
                data,
                pos: start,
                err_at: None,
                err_kind: io::ErrorKind::TimedOut,
            }
        }
        fn erroring(
            data: std::rc::Rc<Vec<u8>>,
            start: usize,
            at: usize,
            kind: io::ErrorKind,
        ) -> Self {
            ScriptedReader {
                data,
                pos: start,
                err_at: Some(at),
                err_kind: kind,
            }
        }
    }

    impl Read for ScriptedReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            // Cap each read at the scheduled error offset so `consumed` lands
            // EXACTLY on `at` when the error fires (deterministic resume offset).
            if let Some(at) = self.err_at {
                if self.pos >= at {
                    self.err_at = None; // fire once
                    return Err(io::Error::new(self.err_kind, "scripted"));
                }
                let stop = at.min(self.data.len());
                let avail = &self.data[self.pos..stop];
                let n = avail.len().min(buf.len());
                buf[..n].copy_from_slice(&avail[..n]);
                self.pos += n;
                return Ok(n);
            }
            let avail = &self.data[self.pos.min(self.data.len())..];
            let n = avail.len().min(buf.len());
            buf[..n].copy_from_slice(&avail[..n]);
            self.pos += n;
            Ok(n)
        }
    }

    fn call_state(target: u64) -> Rc<CallState> {
        CallState::new(target, Arc::new(AtomicBool::new(false)))
    }

    fn drain<R: Read, F: FnMut(u64) -> Result<R, ReconnectError>>(
        r: &mut ResumableHttpReader<R, F>,
    ) -> io::Result<Vec<u8>> {
        let mut out = Vec::new();
        let mut buf = [0u8; 7]; // small buf to force many reads
        loop {
            match r.read(&mut buf)? {
                0 => return Ok(out),
                n => out.extend_from_slice(&buf[..n]),
            }
        }
    }

    #[test]
    fn resume_continues_from_consumed_offset() {
        let data = std::rc::Rc::new((0u8..50).collect::<Vec<u8>>());
        let call = call_state(0); // target 0 = no early-stop
        let att = AttemptState::new();
        let d2 = data.clone();
        let recon_offsets = std::rc::Rc::new(std::cell::RefCell::new(Vec::<u64>::new()));
        let ro = recon_offsets.clone();
        let reconnect = move |off: u64| {
            ro.borrow_mut().push(off);
            Ok(ScriptedReader::from(d2.clone(), off as usize))
        };
        // First reader errors once after 20 bytes.
        let inner = ScriptedReader::erroring(data.clone(), 0, 20, io::ErrorKind::ConnectionReset);
        let mut r = ResumableHttpReader::new(
            inner,
            reconnect,
            call,
            att.clone(),
            Duration::from_secs(3600),
            3,
            false,
        );
        let got = drain(&mut r).expect("drain");
        assert_eq!(
            got,
            (0u8..50).collect::<Vec<u8>>(),
            "stream is the unbroken concatenation"
        );
        assert_eq!(
            recon_offsets.borrow().as_slice(),
            &[20],
            "reconnect at exact consumed offset"
        );
        assert!(att.failure.take().is_none());
    }

    #[test]
    fn target_reached_returns_eof_with_reason_target() {
        let data = std::rc::Rc::new(vec![1u8; 100]);
        let call = call_state(10);
        call.positions_ingested.set(10); // target met
        let att = AttemptState::new();
        let d2 = data.clone();
        let reconnect = move |off: u64| Ok(ScriptedReader::from(d2.clone(), off as usize));
        let mut r = ResumableHttpReader::new(
            ScriptedReader::from(data, 0),
            reconnect,
            call,
            att.clone(),
            Duration::from_secs(3600),
            3,
            false,
        );
        let mut buf = [0u8; 8];
        assert_eq!(r.read(&mut buf).unwrap(), 0);
        assert_eq!(att.eof_reason.get(), Some(EofReason::Target));
    }

    #[test]
    fn inner_eof_returns_eof_with_reason_inner() {
        let data = std::rc::Rc::new(vec![1u8, 2, 3]);
        let call = call_state(0);
        let att = AttemptState::new();
        let d2 = data.clone();
        let reconnect = move |off: u64| Ok(ScriptedReader::from(d2.clone(), off as usize));
        let mut r = ResumableHttpReader::new(
            ScriptedReader::from(data, 0),
            reconnect,
            call,
            att.clone(),
            Duration::from_secs(3600),
            3,
            false,
        );
        assert_eq!(drain(&mut r).unwrap(), vec![1u8, 2, 3]);
        assert_eq!(att.eof_reason.get(), Some(EofReason::InnerEof));
    }

    #[test]
    fn recv_body_timeout_triggers_reconnect_not_failure() {
        let data = std::rc::Rc::new((0u8..30).collect::<Vec<u8>>());
        let call = call_state(0);
        let att = AttemptState::new();
        let d2 = data.clone();
        let n_recon = std::rc::Rc::new(std::cell::Cell::new(0u32));
        let nc = n_recon.clone();
        let reconnect = move |off: u64| {
            nc.set(nc.get() + 1);
            Ok(ScriptedReader::from(d2.clone(), off as usize))
        };
        // Timeout once at offset 10; last_game_at recent so it is NOT a stall.
        let inner = ScriptedReader::erroring(data.clone(), 0, 10, io::ErrorKind::TimedOut);
        let mut r = ResumableHttpReader::new(
            inner,
            reconnect,
            call,
            att.clone(),
            Duration::from_secs(3600),
            3,
            false,
        );
        assert_eq!(drain(&mut r).unwrap(), (0u8..30).collect::<Vec<u8>>());
        assert_eq!(n_recon.get(), 1, "one reconnect, not a failure");
        assert!(att.failure.take().is_none());
    }

    #[test]
    fn repeated_noprogress_resume_escalates_to_failure() {
        // Each reconnect yields a reader that errors immediately (no bytes, no
        // game) ⇒ every resume is a no-progress resume ⇒ `noprogress_resumes`
        // climbs and escalates at the bound. Driven by repeated inner errors
        // (deterministic), independent of wall-clock — the escalation bound is
        // the anti-livelock guarantee.
        let data = std::rc::Rc::new(vec![9u8; 100]);
        let call = call_state(0);
        let att = AttemptState::new();
        let d2 = data.clone();
        let n_recon = std::rc::Rc::new(std::cell::Cell::new(0u32));
        let nc = n_recon.clone();
        // Reconnected readers also error on their first read.
        let reconnect = move |off: u64| {
            nc.set(nc.get() + 1);
            Ok(ScriptedReader::erroring(
                d2.clone(),
                off as usize,
                off as usize,
                io::ErrorKind::ConnectionReset,
            ))
        };
        let mut r = ResumableHttpReader::new(
            ScriptedReader::erroring(data, 0, 0, io::ErrorKind::ConnectionReset),
            reconnect,
            call,
            att.clone(),
            Duration::from_secs(3600),
            3,
            false,
        );
        let mut buf = [0u8; 8];
        let res = r.read(&mut buf);
        assert!(res.is_err(), "escalates to a hard error");
        assert_eq!(att.failure.take(), Some(AttemptFailure::Stall));
        // Escalates AT the bound (≥3), not in a livelock (≤ bound+1).
        let n = n_recon.get();
        assert!(
            (3..=4).contains(&n),
            "escalate exactly at the bound, got {n} reconnects"
        );
    }

    #[test]
    fn stall_reconnects_then_reads_not_instant_escalate() {
        // A games-watchdog stall (clock stale) must reconnect AND give the
        // resumed stream a fresh window to read — NOT instantly escalate to a
        // byte-0 restart (which would discard the consumed bytes). Pins the
        // last_game_at-reset-on-reconnect fix.
        let data = std::rc::Rc::new((0u8..40).collect::<Vec<u8>>());
        let call = call_state(0);
        let att = AttemptState::new();
        att.last_game_at
            .set(Instant::now() - Duration::from_secs(1));
        let d2 = data.clone();
        let n_recon = std::rc::Rc::new(std::cell::Cell::new(0u32));
        let nc = n_recon.clone();
        let reconnect = move |off: u64| {
            nc.set(nc.get() + 1);
            Ok(ScriptedReader::from(d2.clone(), off as usize)) // healthy: has bytes
        };
        let mut r = ResumableHttpReader::new(
            ScriptedReader::from(data, 0),
            reconnect,
            call,
            att.clone(),
            Duration::from_millis(10),
            3,
            false,
        );
        let mut buf = [0u8; 8];
        let n = r
            .read(&mut buf)
            .expect("stall reconnects then reads, no hard error");
        assert!(
            n > 0,
            "the resumed stream is actually read, not instant-escalated"
        );
        assert_eq!(
            n_recon.get(),
            1,
            "exactly one reconnect for the single stall"
        );
        assert!(att.failure.take().is_none());
    }

    #[test]
    fn note_progress_bumps_liveness() {
        let att = AttemptState::new();
        att.last_game_at
            .set(Instant::now() - Duration::from_secs(100));
        let before = att.last_game_at.get();
        let g0 = att.games_since_resume.get();
        att.note_progress();
        assert!(
            att.last_game_at.get() > before,
            "last_game_at advances on a game"
        );
        assert_eq!(att.games_since_resume.get(), g0 + 1);
    }

    #[test]
    fn progress_resets_noprogress_so_no_false_escalation() {
        // A reconnect followed by progress (a parsed game) must reset the
        // escalation counter. Model: reconnect closure calls note_progress, so
        // each resume "made progress" ⇒ never escalates; the stream then ends.
        let data = std::rc::Rc::new((0u8..20).collect::<Vec<u8>>());
        let call = call_state(0);
        let att = AttemptState::new();
        let stale = Instant::now() - Duration::from_secs(1000);
        att.last_game_at.set(stale);
        let d2 = data.clone();
        let att2 = att.clone();
        let reconnect = move |off: u64| {
            att2.note_progress(); // a game arrived right after resume
            Ok(ScriptedReader::from(d2.clone(), off as usize))
        };
        let mut r = ResumableHttpReader::new(
            // Large stall_timeout: the initial stale `last_game_at` still
            // triggers the first stall (1000s ≫ 5s), but after note_progress
            // the post-resume reads can't false-stall even on a slow CI host.
            ScriptedReader::from(data, 0),
            reconnect,
            call,
            att.clone(),
            Duration::from_secs(5),
            3,
            false,
        );
        // Must not escalate: it reconnects (progress resets), then reads to EOF.
        let got = drain(&mut r).expect("no escalation");
        assert_eq!(got, (0u8..20).collect::<Vec<u8>>());
        assert!(att.failure.take().is_none());
        assert!(
            att.last_game_at.get() > stale,
            "progress refreshed the watchdog clock"
        );
    }

    #[test]
    fn range_ignored_200_sets_failure() {
        let data = std::rc::Rc::new((0u8..30).collect::<Vec<u8>>());
        let call = call_state(0);
        let att = AttemptState::new();
        let reconnect = move |_off: u64| -> Result<ScriptedReader, ReconnectError> {
            Err(ReconnectError::RangeIgnored)
        };
        let inner = ScriptedReader::erroring(data, 0, 10, io::ErrorKind::ConnectionReset);
        let mut r = ResumableHttpReader::new(
            inner,
            reconnect,
            call,
            att.clone(),
            Duration::from_secs(3600),
            3,
            false,
        );
        let mut buf = [0u8; 8];
        // First read returns ≤10 bytes (before the scripted error), the second
        // read hits the error → reconnect → RangeIgnored → Err.
        let mut saw_err = false;
        for _ in 0..10 {
            match r.read(&mut buf) {
                Ok(0) => break,
                Ok(_) => {}
                Err(_) => {
                    saw_err = true;
                    break;
                }
            }
        }
        assert!(saw_err);
        assert_eq!(att.failure.take(), Some(AttemptFailure::RangeIgnored));
    }

    #[test]
    fn permanent_reconnect_error_sets_failure() {
        let data = std::rc::Rc::new((0u8..30).collect::<Vec<u8>>());
        let call = call_state(0);
        let att = AttemptState::new();
        let reconnect = move |_off: u64| -> Result<ScriptedReader, ReconnectError> {
            Err(ReconnectError::Permanent("404".into()))
        };
        let inner = ScriptedReader::erroring(data, 0, 5, io::ErrorKind::ConnectionReset);
        let mut r = ResumableHttpReader::new(
            inner,
            reconnect,
            call,
            att.clone(),
            Duration::from_secs(3600),
            3,
            false,
        );
        let mut buf = [0u8; 8];
        let mut saw_err = false;
        for _ in 0..10 {
            match r.read(&mut buf) {
                Ok(0) => break,
                Ok(_) => {}
                Err(_) => {
                    saw_err = true;
                    break;
                }
            }
        }
        assert!(saw_err);
        assert!(matches!(
            att.failure.take(),
            Some(AttemptFailure::Permanent(_))
        ));
    }

    #[test]
    fn stop_flag_returns_eof_clean() {
        let data = std::rc::Rc::new(vec![1u8; 100]);
        let stop = Arc::new(AtomicBool::new(false));
        let call = CallState::new(0, stop.clone());
        let att = AttemptState::new();
        stop.store(true, Ordering::Relaxed);
        let d2 = data.clone();
        let reconnect = move |off: u64| Ok(ScriptedReader::from(d2.clone(), off as usize));
        let mut r = ResumableHttpReader::new(
            ScriptedReader::from(data, 0),
            reconnect,
            call,
            att,
            Duration::from_secs(3600),
            3,
            false,
        );
        let mut buf = [0u8; 8];
        assert_eq!(r.read(&mut buf).unwrap(), 0, "stop ⇒ clean EOF");
    }

    #[test]
    fn bytes_are_progress_read_bumps_liveness() {
        // CCRL temp-download mode: a successful raw read counts as progress, so
        // a steadily-downloading stream never false-stalls even with zero games.
        let data = std::rc::Rc::new((0u8..40).collect::<Vec<u8>>());
        let call = call_state(0);
        let att = AttemptState::new();
        let set = Instant::now() - Duration::from_millis(500);
        att.last_game_at.set(set);
        let g0 = att.games_since_resume.get();
        let d2 = data.clone();
        let reconnect = move |off: u64| Ok(ScriptedReader::from(d2.clone(), off as usize));
        let mut r = ResumableHttpReader::new(
            // Large stall_timeout so the 500ms-stale clock doesn't pre-stall.
            ScriptedReader::from(data, 0),
            reconnect,
            call,
            att.clone(),
            Duration::from_secs(3600),
            3,
            /* bytes_are_progress */ true,
        );
        let mut buf = [0u8; 8];
        assert!(r.read(&mut buf).unwrap() > 0);
        assert!(
            att.last_game_at.get() > set,
            "a raw read refreshes the watchdog clock"
        );
        assert_eq!(att.games_since_resume.get(), g0 + 1);
    }

    #[test]
    fn games_mode_read_does_not_bump_liveness() {
        // Lichess streaming mode (bytes_are_progress=false): a raw read does NOT
        // count as progress — only a parsed game (via note_progress) does. So
        // the bytes-but-no-games stall path stays reachable.
        let data = std::rc::Rc::new((0u8..40).collect::<Vec<u8>>());
        let call = call_state(0);
        let att = AttemptState::new();
        let set = Instant::now() - Duration::from_millis(500);
        att.last_game_at.set(set);
        let d2 = data.clone();
        let reconnect = move |off: u64| Ok(ScriptedReader::from(d2.clone(), off as usize));
        let mut r = ResumableHttpReader::new(
            ScriptedReader::from(data, 0),
            reconnect,
            call,
            att.clone(),
            Duration::from_secs(3600),
            3,
            /* bytes_are_progress */ false,
        );
        let mut buf = [0u8; 8];
        assert!(r.read(&mut buf).unwrap() > 0);
        assert_eq!(
            att.last_game_at.get(),
            set,
            "a raw read must NOT refresh the games-watchdog"
        );
    }
}
