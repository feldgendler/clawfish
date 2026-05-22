# M6.H prior art — network/decompression layer (ureq / zstd / zip / Range-resume)

Prior-art research for the `corpus fetch` network fetcher. **General-purpose
Rust infra, not chess-domain** — ADR-0003 source-reading restriction does not
bite (no chess engine repos browsed). Feeds `docs/plans/m6.h.md`.

Primary platform: Apple Silicon macOS. Edition 2024. License allowlist:
crates.io only; permissive only (the `deny.toml` set + the Unicode-3.0/Zlib
additions this milestone makes — see plan §Dependencies).

## 1. `ureq` — sync HTTP client → **target 3.3.0** (MIT OR Apache-2.0)

2.x → 3.x is a ground-up rewrite; APIs incompatible. 3.3.0 is edition-2024,
MSRV 1.85.

**Agent + granular timeouts** — `Agent::config_builder()` → `ConfigBuilder`;
all timeout setters take `Option<Duration>`:

```rust
let agent = ureq::Agent::config_builder()
    .timeout_connect(Some(Duration::from_secs(10)))      // TCP handshake
    .timeout_recv_response(Some(Duration::from_secs(30))) // headers
    .timeout_recv_body(Some(Duration::from_secs(10)))     // PER-READ body deadline
    .max_redirects(5)
    .build()
    .new_agent();
```

Timeout method set: `timeout_global`, `timeout_per_call`, `timeout_resolve`,
`timeout_connect`, `timeout_send_request`, `timeout_send_body`,
`timeout_recv_response`, `timeout_recv_body`, `timeout_await_100`.

**GET + Range header** — `.header("Range", "bytes=12345678-")` before `.call()`.

**Streaming body as `impl Read`** — `response.into_body().into_reader()` →
`BodyReader<'static>: Read` (sendable; no whole-body buffering, streams from
socket). Borrowed variant: `body.as_reader()`.

**Non-2xx** — by default 4xx/5xx → `Err(ureq::Error::StatusCode(u16))`
(`http_status_as_error(true)` default). Body not accessible when this fires
(flip to `false` + check `response.status()` to read an error body). Other
error variants: `Error::Timeout(Timeout::RecvBody)`, `Error::Io(io::Error)`.

**TLS / OS trust store** — default backend rustls + `ring`, default roots the
static Mozilla `webpki-roots` bundle. **For the OS trust store use the
`platform-verifier` feature**:

```rust
use ureq::tls::{TlsConfig, RootCerts};
let agent = ureq::Agent::config_builder()
    .tls_config(TlsConfig::builder().root_certs(RootCerts::PlatformVerifier).build())
    .build().new_agent();
```

`RootCerts::PlatformVerifier` delegates cert-chain + hostname verification to
the OS (macOS Security framework) — strictly better than loading a static
snapshot via `rustls-native-certs`, and keeps clawfish working on networks
with a legitimately-installed corporate root CA. **Do NOT add
`rustls-native-certs` directly** — `platform-verifier` subsumes it. (This is a
clean realization of the roadmap's "validate against the OS trust store"
intent; the literal "`rustls-native-certs`" wording in the roadmap is the
mechanism, `platform-verifier` is the better mechanism for the same goal.)

**Redirects** — `.max_redirects(5)` (u32, default 10);
`.max_redirects_will_error(true)` (default) → `Error::TooManyRedirects`.

**No auto-retry in 3.x** (2.x retried idempotent methods automatically). The
`ResumableHttpReader` must implement all retry/reconnect itself.

## 2. `rustls-native-certs` — NOT a direct dep

Subsumed by ureq's `platform-verifier` feature (§1). Recorded for completeness
only.

## 3. zstd — **`zstd` 0.13.3 (C bindings), MIT**

`zstd::stream::read::Decoder::new(reader)` (`reader: Read`, auto-`BufReader`)
or `Decoder::with_buffer(bufread)`. Decoder: `Read`. Concatenates multi-frame
streams to inner EOF by default (leave `single_frame()` off).

**Resumable inner reader is sound**: the decoder's window/block state is
independent of the byte source. As long as `ResumableHttpReader::read()`
returns the exact continuation bytes and **never lets a transient error
escape** (absorb internally → reconnect → present continuation), the decoder
is oblivious to the reconnect. Do not use `get_mut()` to swap the inner reader
from the decoder's view — wrap reconnect *inside* the `Read` impl.

`zstd` vendors libzstd in `zstd-sys` and statically links it via a build-time
C compiler (always present on macOS dev). **No runtime system dep** — "no
system-tool deps" satisfied. ~1.4–3.5× faster decode than pure-Rust `ruzstd`;
for overnight multi-GB ingest the gap matters → pick `zstd`. (`zstd-sys`
license is `BSD-3-Clause OR GPL-2.0` → cargo-deny resolves to BSD-3-Clause.)

## 4. `zip` — CCRL: **download-to-temp + `ZipArchive`** (MIT)

ZIP central directory is at EOF → `ZipArchive::new` needs `Read + Seek`; an
HTTP body is `Read`-only. Two options:

- `zip::read::read_zipfile_from_stream(&mut r) -> ZipResult<Option<ZipFile<R>>>`
  (`R: Read`, no Seek) — reads local-file-header-prefixed entries
  sequentially; `Ok(None)` at the central-directory marker. Each `ZipFile`
  must be fully consumed before the next call. **But Range-resume is hard**:
  resume needs the byte offset of the *current entry's* start, not just total
  bytes — non-trivial mid-entry.
- **Recommended for CCRL: download to a temp file (`Range`-resumable
  naturally), then `ZipArchive::new(File)` once complete, iterate entries,
  pick the `.pgn`.** CCRL snapshots are tens of MB, so the temp file is cheap
  and avoids the offset-tracking footgun. Disk pre-check (§7) covers the temp
  file. Temp file deleted after ingest.

`zip` pulls `flate2`/`miniz_oxide` for deflate (`miniz_oxide` is
`MIT OR Zlib OR Apache-2.0` → MIT resolves). **Verify the exact `zip` version
with `cargo add zip` before pinning** (search snippets disagreed: lib.rs
showed 2.x, a docs.rs page title showed 8.6.0).

## 5. HTTP Range-resume pattern

Track `consumed: u64` (bytes delivered to the caller). On transient failure →
reopen `GET` with `Range: bytes={consumed}-`; expect **`206 Partial
Content`** + `Content-Range: bytes {consumed}-{total-1}/{total}`. Detect:

| Resumed-request status | Meaning | Action |
|---|---|---|
| `206` | range honored | continue feeding at `consumed` |
| `200` | server ignored Range, restarting at 0 | discard decoder, restart-from-zero (escalation) |
| `416` | Range Not Satisfiable (past EOF) | treat as clean EOS |

`Accept-Ranges: bytes` advertises support (advisory). **Lichess
(`database.lichess.org`, nginx static files) supports byte ranges** —
`Accept-Ranges: bytes` present. CCRL uses temp-file download (§4) so resume is
file-offset based, not body-stream based.

## 6. macOS socket timeouts via ureq

ureq sets `SO_RCVTIMEO` from `timeout_recv_body`; a fired read timeout →
kernel `EAGAIN`/`EWOULDBLOCK` (macOS) / `ETIMEDOUT` (Linux), which **ureq
converts to `Error::Timeout(Timeout::RecvBody)`** (not a raw `io::Error`). So
the reader's `read()` sees `Err(Timeout)` on a stall-window expiry. **It is a
per-read-syscall deadline (≈`SO_RCVTIMEO`), not a total-body deadline** — a
slow trickle won't trip it (which is exactly why the M6.H stall watchdog keys
on *games parsed*, not byte-liveness — see plan). For a hard total cap use
`timeout_global` (M6.H deliberately does NOT cap total time).

## 7. Disk-space pre-check — `libc::statvfs` (no new dep)

```rust
fn available_bytes(path: &Path) -> io::Result<u64> {
    let c = CString::new(path.as_os_str().as_bytes())?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((s.f_bavail as u64) * (s.f_frsize as u64))   // f_frsize, NOT f_bsize
}
```

macOS gotcha: `f_bsize` is the preferred I/O size (often 1 MiB) — use
`f_frsize` (fundamental block size) × `f_bavail` (blocks available to
non-privileged). `libc` already a dep → no new crate (rejected `fs2`
unmaintained / `fs4` adds a dep).

## Dependency table

| Crate | Version | License | Action |
|---|---|---|---|
| `ureq` | `3.3` (features `["platform-verifier"]`) | MIT/Apache-2.0 | add |
| `zstd` | `0.13` | MIT | add |
| `zip` | verify w/ `cargo add` | MIT | add |
| `libc` | `0.2` (present) | MIT/Apache-2.0 | existing |
| `rustls-native-certs` | — | — | do NOT add (subsumed) |
| `fs2`/`fs4` | — | — | do NOT add |

**Transitive-license note for the plan**: `platform-verifier` pulls
rustls + `ring` (ring declares `license-file`, no SPDX `license` field →
cargo-deny flags it "unlicensed" → needs a `[[licenses.clarify]]`); proc-macro
deps may pull `unicode-ident` (`Unicode-3.0`); `miniz_oxide`/others may surface
`Zlib`. Plan must (a) add `Unicode-3.0` + `Zlib` to the `deny.toml` allowlist
(consistent with CLAUDE.md's stated permissive set), (b) add a ring clarify if
`cargo deny check` flags it, determined empirically.

## Key gotchas (carry into implementation)

- ureq 3.x has **no auto-retry** — `ResumableHttpReader` owns all reconnect.
- Stall fires as `Error::Timeout(Timeout::RecvBody)`, not `Io(WouldBlock)`.
- `timeout_recv_body` is per-read, not total; trickle won't trip it → key the
  watchdog on **games parsed**.
- `Error::StatusCode(u16)` on 4xx/5xx; no body unless `http_status_as_error(false)`.
- Never let a transient error escape `ResumableHttpReader` into the zstd decoder.
- `statvfs`: `f_frsize`, never `f_bsize`.
- `read_zipfile_from_stream` needs each entry fully consumed before advancing;
  Range-resume mid-entry is hard → temp-file approach for CCRL.
