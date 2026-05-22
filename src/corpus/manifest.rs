//! Corpus manifest: provenance record, SHA-256 checksums, and filter-spec echo.
//!
//! ## Frozen-artifact contract
//!
//! The manifest is the bytes consumers freeze on.  It records every source
//! file's path, byte-count, and SHA-256 digest so a consumer (M6.I tuner)
//! can verify the corpus it reads is the one that was produced. The R-TC
//! reproducibility contract adds the campaign seeds + depth ladder + the
//! corpus-bytes digest so the quality gate can re-verify the on-disk
//! artifact byte-identically.
//!
//! ## JSON encoding
//!
//! `write_manifest`/`read_manifest` use a hand-rolled minimal JSON writer and
//! parser.  Only the value shapes this struct uses are supported: strings,
//! `u32`/`u64`, `f64`, and arrays of objects.  Malformed input returns
//! the canonical [`CorpusError::Json`] (see `super::CorpusError`).
//!
//! ## File I/O
//!
//! `write_manifest` writes the manifest file directly (not via atomic rename).
//! `read_manifest` accepts only valid UTF-8.

use std::fs;
use std::path::Path;

use super::{CorpusError, HIGH_SCORE_CP, OPENING_SKIP_PLIES, PER_GAME_CAP, QUIET_MARGIN_CP};

// ── SHA-256 (FIPS 180-4) ──────────────────────────────────────────────────────

/// The first 32 bits of the fractional parts of the cube roots of the first 64
/// primes — the FIPS 180-4 §4.2.2 round constants.
#[rustfmt::skip]
const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5,
    0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3,
    0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc,
    0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
    0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13,
    0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3,
    0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5,
    0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208,
    0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// Initial hash values — the first 32 bits of the fractional parts of the
/// square roots of the first 8 primes (FIPS 180-4 §5.3.3).
const H0: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
];

/// Compress one 512-bit (64-byte) block into the running hash state.
fn compress(state: &mut [u32; 8], block: &[u8; 64]) {
    // Prepare message schedule W[0..64].
    let mut w = [0u32; 64];
    for (i, chunk) in block.chunks_exact(4).enumerate() {
        w[i] = u32::from_be_bytes(chunk.try_into().unwrap());
    }
    for i in 16..64 {
        let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
        let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
        w[i] = w[i - 16]
            .wrapping_add(s0)
            .wrapping_add(w[i - 7])
            .wrapping_add(s1);
    }

    let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = *state;

    for i in 0..64 {
        let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
        let ch = (e & f) ^ (!e & g);
        let temp1 = h
            .wrapping_add(s1)
            .wrapping_add(ch)
            .wrapping_add(K[i])
            .wrapping_add(w[i]);
        let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
        let maj = (a & b) ^ (a & c) ^ (b & c);
        let temp2 = s0.wrapping_add(maj);

        h = g;
        g = f;
        f = e;
        e = d.wrapping_add(temp1);
        d = c;
        c = b;
        b = a;
        a = temp1.wrapping_add(temp2);
    }

    state[0] = state[0].wrapping_add(a);
    state[1] = state[1].wrapping_add(b);
    state[2] = state[2].wrapping_add(c);
    state[3] = state[3].wrapping_add(d);
    state[4] = state[4].wrapping_add(e);
    state[5] = state[5].wrapping_add(f);
    state[6] = state[6].wrapping_add(g);
    state[7] = state[7].wrapping_add(h);
}

/// Compute the SHA-256 digest of a byte slice.
///
/// Implements FIPS 180-4 §6.2.2 (SHA-256 hash computation).
pub fn sha256_bytes(data: &[u8]) -> [u8; 32] {
    let mut state = H0;

    // Process all complete 64-byte blocks.
    let full_blocks = data.len() / 64;
    for i in 0..full_blocks {
        let block: &[u8; 64] = data[i * 64..(i + 1) * 64].try_into().unwrap();
        compress(&mut state, block);
    }

    // Padding: append bit '1' then zeros then the 64-bit big-endian message
    // length in *bits* (FIPS 180-4 §5.1.1).
    let remainder = &data[full_blocks * 64..];
    let bit_len = (data.len() as u64).wrapping_mul(8);

    // The padded message always fits in 1 or 2 additional blocks.
    let mut pad = [0u8; 128];
    pad[..remainder.len()].copy_from_slice(remainder);
    pad[remainder.len()] = 0x80;
    // Length field goes in the last 8 bytes.
    let pad_blocks = if remainder.len() < 56 { 1usize } else { 2 };
    let len_offset = pad_blocks * 64 - 8;
    pad[len_offset..len_offset + 8].copy_from_slice(&bit_len.to_be_bytes());

    for i in 0..pad_blocks {
        let block: &[u8; 64] = pad[i * 64..(i + 1) * 64].try_into().unwrap();
        compress(&mut state, block);
    }

    // Produce the final digest (big-endian word serialization).
    let mut digest = [0u8; 32];
    for (i, word) in state.iter().enumerate() {
        digest[i * 4..(i + 1) * 4].copy_from_slice(&word.to_be_bytes());
    }
    digest
}

/// Compute the SHA-256 digest of a file on disk.
///
/// Reads the file entirely into memory; suitable for corpus source files
/// which are expected to be at most a few GiB.
pub fn sha256_file(path: &Path) -> Result<[u8; 32], CorpusError> {
    let data = fs::read(path)?;
    Ok(sha256_bytes(&data))
}

/// Format a 32-byte digest as a lowercase hex string (64 characters).
///
/// Use this to convert the output of [`sha256_bytes`] or [`sha256_file`] into
/// the string stored in [`SourceEntry::sha256`].
pub fn hex_digest(digest: &[u8; 32]) -> String {
    let mut out = String::with_capacity(64);
    for &b in digest {
        out.push(char::from_digit(u32::from(b >> 4), 16).unwrap());
        out.push(char::from_digit(u32::from(b & 0x0f), 16).unwrap());
    }
    out
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

// ── Manifest structs ──────────────────────────────────────────────────────────

/// Provenance record for one source file in the corpus.
///
/// The `sha256` field is stored as a lowercase hex string in JSON so the
/// manifest is human-readable with a standard `sha256sum` comparison.
#[derive(Debug, Clone, PartialEq)]
pub struct SourceEntry {
    /// Human-readable label (e.g., `"ccrl-2022"`, `"lichess-2023-01"`).
    pub label: String,
    /// Path to the source file as recorded at extraction time.
    pub path: String,
    /// Number of bytes in the source file.
    pub byte_count: u64,
    /// SHA-256 digest of the source file (lowercase hex, 64 chars).
    pub sha256: String,
    /// Number of positions extracted from this source.
    pub positions_extracted: u64,
}

/// Corpus manifest: the frozen-artifact provenance record.
///
/// Written alongside the corpus by the extraction pipeline and read by M6.I
/// to verify it is consuming the correct corpus snapshot. The R-TC
/// reproducibility contract pins `self_play_seed` + `split_seed` +
/// `depth_ladder` + `corpus_sha256`, so the data-quality gate can re-derive
/// the bytes and check them against the digest recorded here.
#[derive(Debug, Clone, PartialEq)]
pub struct Manifest {
    /// Corpus schema version — bump when the format changes incompatibly.
    pub schema_version: u32,
    /// ISO-8601 timestamp (UTC) of when the corpus was constructed.
    pub created_at: String,
    /// Git commit hash of the engine used for quiet-position extraction.
    pub engine_commit: String,
    /// Total number of positions in the corpus.
    pub total_positions: u64,
    /// Fraction of positions held out for validation (0.0–1.0).
    pub validation_fraction: f64,
    /// One entry per source file.
    pub sources: Vec<SourceEntry>,
    /// Self-play campaign base seed (R-TC: pairs with the depth ladder so
    /// `corpus selfplay --seed <self_play_seed>` re-derives identical bytes).
    pub self_play_seed: u64,
    /// Number of games the self-play campaign produced (re-run knob).
    pub games: u64,
    /// Max half-moves before adjudicating an over-long self-play game.
    pub max_plies: u32,
    /// Seeded-random opening plies from startpos in self-play (R-TC knob).
    pub opening_random_plies: u32,
    /// Worker count used at corpus-build time. Pinned so the re-run.sh
    /// reproduces the identical on-disk byte order (dedup is order-independent,
    /// but the build pass writes shards in `(game_id, ply)` order so worker
    /// count is byte-irrelevant — recorded here for full reproducibility audit).
    pub workers: u32,
    /// Train/val game-level split seed (R3 reproducible split).
    pub split_seed: u64,
    /// Held-out validation fraction passed to `corpus build` (mirrors
    /// `validation_fraction`; pinned separately so the re-run script reads
    /// the exact build-time knob from one place).
    pub val_fraction: f64,
    /// Calibrated R-TC depth-ladder rungs `(depth, weight)`, written here so
    /// `corpus rerun` reproduces the empirically-anchored sampling.
    pub depth_ladder: Vec<(u8, u32)>,
    /// Optional vendored opening-book path (relative to repo root) used
    /// at self-play time. `Some` only when `opening_mode = Some("book")`.
    pub opening_book_path: Option<String>,
    /// SHA-256 (lowercase hex) of the opening-book bytes — pinned so a
    /// re-run detects upstream-drift on the vendored book.
    pub opening_book_sha256: Option<String>,
    /// Opening regime the self-play campaign ran in (`"book"` or
    /// `"random"`). `None` on a not-yet-run / calibrate-ladder-only
    /// manifest stub. The on-book / off-book proportion is reweighted at
    /// M6.I via per-source loss; one corpus per regime (ADR-0035 §10).
    pub opening_mode: Option<String>,
    /// SHA-256 (lowercase hex) of the frozen corpus bytes themselves. The
    /// reproducibility check (quality gate) re-derives this from disk and
    /// fails if the digest drifts.
    pub corpus_sha256: String,
}

// ── Minimal JSON writer ───────────────────────────────────────────────────────

/// Escape a string for embedding in a JSON string literal.
pub(crate) fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out
}

fn write_json_string(out: &mut String, s: &str) {
    out.push('"');
    out.push_str(&json_escape(s));
    out.push('"');
}

fn write_source_entry(out: &mut String, e: &SourceEntry, indent: &str) {
    let inner = format!("{indent}  ");
    out.push_str(&format!("{indent}{{\n"));
    out.push_str(&format!("{inner}\"label\": "));
    write_json_string(out, &e.label);
    out.push_str(",\n");
    out.push_str(&format!("{inner}\"path\": "));
    write_json_string(out, &e.path);
    out.push_str(",\n");
    out.push_str(&format!("{inner}\"byte_count\": {},\n", e.byte_count));
    out.push_str(&format!("{inner}\"sha256\": "));
    write_json_string(out, &e.sha256);
    out.push_str(",\n");
    out.push_str(&format!(
        "{inner}\"positions_extracted\": {}\n",
        e.positions_extracted
    ));
    out.push_str(&format!("{indent}}}"));
}

fn write_manifest_json(m: &Manifest) -> String {
    let mut out = String::new();
    out.push_str("{\n");
    out.push_str(&format!("  \"schema_version\": {},\n", m.schema_version));
    out.push_str("  \"created_at\": ");
    write_json_string(&mut out, &m.created_at);
    out.push_str(",\n");
    out.push_str("  \"engine_commit\": ");
    write_json_string(&mut out, &m.engine_commit);
    out.push_str(",\n");
    out.push_str(&format!("  \"total_positions\": {},\n", m.total_positions));
    // f64 — write with enough precision for a fraction in [0, 1].
    out.push_str(&format!(
        "  \"validation_fraction\": {},\n",
        format_f64(m.validation_fraction)
    ));
    out.push_str("  \"sources\": [\n");
    for (i, entry) in m.sources.iter().enumerate() {
        write_source_entry(&mut out, entry, "    ");
        if i + 1 < m.sources.len() {
            out.push(',');
        }
        out.push('\n');
    }
    out.push_str("  ],\n");
    out.push_str(&format!("  \"self_play_seed\": {},\n", m.self_play_seed));
    out.push_str(&format!("  \"games\": {},\n", m.games));
    out.push_str(&format!("  \"max_plies\": {},\n", m.max_plies));
    out.push_str(&format!(
        "  \"opening_random_plies\": {},\n",
        m.opening_random_plies
    ));
    out.push_str(&format!("  \"workers\": {},\n", m.workers));
    out.push_str(&format!("  \"split_seed\": {},\n", m.split_seed));
    out.push_str(&format!(
        "  \"val_fraction\": {},\n",
        format_f64(m.val_fraction)
    ));
    out.push_str("  \"depth_ladder\": [\n");
    for (i, (depth, weight)) in m.depth_ladder.iter().enumerate() {
        out.push_str(&format!("    {{\"depth\": {depth}, \"weight\": {weight}}}"));
        if i + 1 < m.depth_ladder.len() {
            out.push(',');
        }
        out.push('\n');
    }
    out.push_str("  ],\n");
    // Opening-book provenance — fields are *omitted* (not `null`-valued)
    // when no book is loaded, because the JSON parser doesn't recognize a
    // bare `null` token (one-shot reader, only the value shapes we emit).
    if let Some(p) = &m.opening_book_path {
        out.push_str("  \"opening_book_path\": ");
        write_json_string(&mut out, p);
        out.push_str(",\n");
    }
    if let Some(s) = &m.opening_book_sha256 {
        out.push_str("  \"opening_book_sha256\": ");
        write_json_string(&mut out, s);
        out.push_str(",\n");
    }
    if let Some(mode) = &m.opening_mode {
        out.push_str("  \"opening_mode\": ");
        write_json_string(&mut out, mode);
        out.push_str(",\n");
    }
    out.push_str("  \"corpus_sha256\": ");
    write_json_string(&mut out, &m.corpus_sha256);
    out.push('\n');
    out.push('}');
    out
}

/// Format an f64 for JSON: use enough decimal digits for an exact round-trip
/// of the values we store (fractions in [0, 1] with up to ~15 sig figures).
fn format_f64(v: f64) -> String {
    // `{:.17}` gives enough precision for an exact IEEE-754 round-trip.  We
    // strip trailing zeros (but keep at least one decimal digit) so that
    // "0.10000000000000001" → "0.1" in the common case.
    let raw = format!("{v:.17}");
    // Strip trailing zeros after the decimal point.
    if let Some(dot) = raw.find('.') {
        let trimmed = raw.trim_end_matches('0');
        // Ensure at least one digit after the decimal point.
        if trimmed.ends_with('.') {
            format!("{trimmed}0")
        } else if trimmed.len() > dot {
            trimmed.to_owned()
        } else {
            raw
        }
    } else {
        // No decimal point — shouldn't happen for f64 formatted with .17
        format!("{v:.1}")
    }
}

// ── Minimal JSON parser ───────────────────────────────────────────────────────

/// Lightweight JSON value — only the shapes used by [`Manifest`] and (M6.H)
/// `fetch::catalog`'s `fetch-state.json`. `pub(crate)` so the fetcher reuses
/// this tested parser instead of duplicating a fragile second one.
#[derive(Debug)]
pub(crate) enum JsonVal {
    Str(String),
    Num(f64),
    Arr(Vec<JsonVal>),
    Obj(Vec<(String, JsonVal)>),
}

/// Parse a complete JSON document (object/array/scalar) and reject trailing
/// content. The single `pub(crate)` entry point for the minimal parser.
/// Only the M6.H `fetch::catalog` consumer uses this, so it is gated behind the
/// `corpus-fetch` feature (manifest's own parse uses `Parser` directly).
#[cfg(feature = "corpus-fetch")]
pub(crate) fn parse_json(src: &str) -> Result<JsonVal, CorpusError> {
    let mut p = Parser::new(src.as_bytes());
    p.skip_ws();
    let v = p.parse_value()?;
    p.skip_ws();
    if p.peek().is_some() {
        return Err(p.err("trailing content after JSON value"));
    }
    Ok(v)
}

struct Parser<'a> {
    src: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn new(src: &'a [u8]) -> Self {
        Parser { src, pos: 0 }
    }

    fn err(&self, msg: impl Into<String>) -> CorpusError {
        CorpusError::Json(format!("at byte {}: {}", self.pos, msg.into()))
    }

    fn peek(&self) -> Option<u8> {
        self.src.get(self.pos).copied()
    }

    fn advance(&mut self) -> Option<u8> {
        let b = self.src.get(self.pos).copied();
        if b.is_some() {
            self.pos += 1;
        }
        b
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    fn expect(&mut self, b: u8) -> Result<(), CorpusError> {
        self.skip_ws();
        match self.advance() {
            Some(got) if got == b => Ok(()),
            Some(got) => Err(self.err(format!("expected '{}' got '{}'", b as char, got as char))),
            None => Err(self.err(format!("expected '{}' but got EOF", b as char))),
        }
    }

    fn parse_string(&mut self) -> Result<String, CorpusError> {
        self.expect(b'"')?;
        let mut s = String::new();
        loop {
            match self.advance() {
                None => return Err(self.err("unterminated string")),
                Some(b'"') => break,
                Some(b'\\') => match self.advance() {
                    Some(b'"') => s.push('"'),
                    Some(b'\\') => s.push('\\'),
                    Some(b'/') => s.push('/'),
                    Some(b'n') => s.push('\n'),
                    Some(b'r') => s.push('\r'),
                    Some(b't') => s.push('\t'),
                    Some(b'u') => {
                        // 4 hex digits
                        let mut code: u32 = 0;
                        for _ in 0..4 {
                            let b = self.advance().ok_or_else(|| self.err("short \\uXXXX"))?;
                            let nibble = hex_nibble(b)
                                .ok_or_else(|| self.err(format!("bad hex '{}'", b as char)))?;
                            code = (code << 4) | u32::from(nibble);
                        }
                        s.push(char::from_u32(code).unwrap_or('\u{FFFD}'));
                    }
                    Some(c) => {
                        return Err(self.err(format!("unknown escape \\{}", c as char)));
                    }
                    None => return Err(self.err("EOF in escape")),
                },
                Some(b) => s.push(b as char),
            }
        }
        Ok(s)
    }

    fn parse_number(&mut self) -> Result<f64, CorpusError> {
        let start = self.pos;
        // Consume optional leading sign.
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        // Integer part.
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.pos += 1;
        }
        // Optional fractional part.
        if self.peek() == Some(b'.') {
            self.pos += 1;
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        // Optional exponent.
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        let s = std::str::from_utf8(&self.src[start..self.pos])
            .map_err(|_| self.err("non-UTF-8 in number"))?;
        s.parse::<f64>()
            .map_err(|_| self.err(format!("invalid number: {s:?}")))
    }

    fn parse_array(&mut self) -> Result<Vec<JsonVal>, CorpusError> {
        self.expect(b'[')?;
        let mut arr = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(arr);
        }
        loop {
            self.skip_ws();
            arr.push(self.parse_value()?);
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b']') => {
                    self.pos += 1;
                    break;
                }
                _ => return Err(self.err("expected ',' or ']' in array")),
            }
        }
        Ok(arr)
    }

    fn parse_object(&mut self) -> Result<Vec<(String, JsonVal)>, CorpusError> {
        self.expect(b'{')?;
        let mut pairs = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(pairs);
        }
        loop {
            self.skip_ws();
            let key = self.parse_string()?;
            self.skip_ws();
            self.expect(b':')?;
            self.skip_ws();
            let val = self.parse_value()?;
            pairs.push((key, val));
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b'}') => {
                    self.pos += 1;
                    break;
                }
                _ => return Err(self.err("expected ',' or '}' in object")),
            }
        }
        Ok(pairs)
    }

    fn parse_value(&mut self) -> Result<JsonVal, CorpusError> {
        self.skip_ws();
        match self.peek() {
            Some(b'"') => Ok(JsonVal::Str(self.parse_string()?)),
            Some(b'[') => Ok(JsonVal::Arr(self.parse_array()?)),
            Some(b'{') => Ok(JsonVal::Obj(self.parse_object()?)),
            Some(b'-' | b'0'..=b'9') => Ok(JsonVal::Num(self.parse_number()?)),
            Some(b) => Err(self.err(format!("unexpected byte '{}' starting value", b as char))),
            None => Err(self.err("unexpected EOF")),
        }
    }
}

// ── JSON → Manifest ───────────────────────────────────────────────────────────

fn require_str<'a>(val: &'a JsonVal, field: &str) -> Result<&'a str, CorpusError> {
    match val {
        JsonVal::Str(s) => Ok(s),
        _ => Err(CorpusError::Json(format!(
            "field {field:?} must be a string"
        ))),
    }
}

fn require_u64(val: &JsonVal, field: &str) -> Result<u64, CorpusError> {
    match val {
        JsonVal::Num(n) => {
            let v = *n as u64;
            // Round-trip check: the stored integer must survive f64 → u64.
            if (v as f64 - n).abs() > 0.5 {
                return Err(CorpusError::Json(format!(
                    "field {field:?} value {n} is not an integer"
                )));
            }
            Ok(v)
        }
        _ => Err(CorpusError::Json(format!(
            "field {field:?} must be a number"
        ))),
    }
}

fn require_u32(val: &JsonVal, field: &str) -> Result<u32, CorpusError> {
    let v = require_u64(val, field)?;
    u32::try_from(v)
        .map_err(|_| CorpusError::Json(format!("field {field:?} value {v} overflows u32")))
}

fn require_f64(val: &JsonVal, field: &str) -> Result<f64, CorpusError> {
    match val {
        JsonVal::Num(n) => Ok(*n),
        _ => Err(CorpusError::Json(format!(
            "field {field:?} must be a number"
        ))),
    }
}

fn find_field<'a>(obj: &'a [(String, JsonVal)], key: &str) -> Result<&'a JsonVal, CorpusError> {
    obj.iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v)
        .ok_or_else(|| CorpusError::Json(format!("missing field {key:?}")))
}

fn parse_source_entry(obj: &[(String, JsonVal)]) -> Result<SourceEntry, CorpusError> {
    Ok(SourceEntry {
        label: require_str(find_field(obj, "label")?, "label")?.to_owned(),
        path: require_str(find_field(obj, "path")?, "path")?.to_owned(),
        byte_count: require_u64(find_field(obj, "byte_count")?, "byte_count")?,
        sha256: require_str(find_field(obj, "sha256")?, "sha256")?.to_owned(),
        positions_extracted: require_u64(
            find_field(obj, "positions_extracted")?,
            "positions_extracted",
        )?,
    })
}

fn parse_manifest_json(src: &str) -> Result<Manifest, CorpusError> {
    let mut p = Parser::new(src.as_bytes());
    p.skip_ws();
    let top = p.parse_object()?;
    p.skip_ws();
    if p.peek().is_some() {
        return Err(p.err("trailing content after manifest JSON object"));
    }

    let schema_version = require_u32(find_field(&top, "schema_version")?, "schema_version")?;
    let created_at = require_str(find_field(&top, "created_at")?, "created_at")?.to_owned();
    let engine_commit =
        require_str(find_field(&top, "engine_commit")?, "engine_commit")?.to_owned();
    let total_positions = require_u64(find_field(&top, "total_positions")?, "total_positions")?;
    let validation_fraction = require_f64(
        find_field(&top, "validation_fraction")?,
        "validation_fraction",
    )?;

    let sources_val = find_field(&top, "sources")?;
    let sources = match sources_val {
        JsonVal::Arr(arr) => arr
            .iter()
            .map(|item| match item {
                JsonVal::Obj(obj) => parse_source_entry(obj),
                _ => Err(CorpusError::Json(
                    "sources array element must be an object".into(),
                )),
            })
            .collect::<Result<Vec<_>, _>>()?,
        _ => {
            return Err(CorpusError::Json(
                "field \"sources\" must be an array".into(),
            ));
        }
    };

    let self_play_seed = require_u64(find_field(&top, "self_play_seed")?, "self_play_seed")?;
    let split_seed = require_u64(find_field(&top, "split_seed")?, "split_seed")?;
    // Reproducibility-contract knobs added after the initial schema:
    // missing fields default to 0/`validation_fraction` so older manifests
    // continue to round-trip. New manifests written by `cmd_selfplay` /
    // `cmd_build` populate them with the actual run values.
    let games = optional_u64(&top, "games").unwrap_or(0);
    let max_plies = optional_u32(&top, "max_plies").unwrap_or(0);
    let opening_random_plies = optional_u32(&top, "opening_random_plies").unwrap_or(0);
    let workers = optional_u32(&top, "workers").unwrap_or(0);
    let val_fraction = optional_f64(&top, "val_fraction").unwrap_or(validation_fraction);
    let corpus_sha256 =
        require_str(find_field(&top, "corpus_sha256")?, "corpus_sha256")?.to_owned();

    let depth_ladder_val = find_field(&top, "depth_ladder")?;
    let depth_ladder = match depth_ladder_val {
        JsonVal::Arr(arr) => arr
            .iter()
            .map(|item| match item {
                JsonVal::Obj(obj) => {
                    let depth = require_u32(find_field(obj, "depth")?, "depth")?;
                    let weight = require_u32(find_field(obj, "weight")?, "weight")?;
                    let depth = u8::try_from(depth)
                        .map_err(|_| CorpusError::Json(format!("depth {depth} overflows u8")))?;
                    Ok((depth, weight))
                }
                _ => Err(CorpusError::Json(
                    "depth_ladder array element must be an object".into(),
                )),
            })
            .collect::<Result<Vec<_>, CorpusError>>()?,
        _ => {
            return Err(CorpusError::Json(
                "field \"depth_ladder\" must be an array".into(),
            ));
        }
    };

    // Opening-book + opening-mode fields. `opening_mode` is `None` on a
    // calibrate-ladder-only stub or any older manifest pre-dating the
    // four-source taxonomy; the rerun script defaults to `random` in
    // that case.
    let opening_book_path = optional_str(&top, "opening_book_path").map(str::to_owned);
    let opening_book_sha256 = optional_str(&top, "opening_book_sha256").map(str::to_owned);
    let opening_mode = optional_str(&top, "opening_mode").map(str::to_owned);

    Ok(Manifest {
        schema_version,
        created_at,
        engine_commit,
        total_positions,
        validation_fraction,
        sources,
        self_play_seed,
        games,
        max_plies,
        opening_random_plies,
        workers,
        split_seed,
        val_fraction,
        depth_ladder,
        opening_book_path,
        opening_book_sha256,
        opening_mode,
        corpus_sha256,
    })
}

fn optional_u64(top: &[(String, JsonVal)], key: &str) -> Option<u64> {
    let v = top.iter().find(|(k, _)| k == key).map(|(_, v)| v)?;
    require_u64(v, key).ok()
}

fn optional_u32(top: &[(String, JsonVal)], key: &str) -> Option<u32> {
    let v = top.iter().find(|(k, _)| k == key).map(|(_, v)| v)?;
    require_u32(v, key).ok()
}

fn optional_f64(top: &[(String, JsonVal)], key: &str) -> Option<f64> {
    let v = top.iter().find(|(k, _)| k == key).map(|(_, v)| v)?;
    require_f64(v, key).ok()
}

/// Optional string field — `null` or absent → `None`, otherwise the string.
fn optional_str<'a>(top: &'a [(String, JsonVal)], key: &str) -> Option<&'a str> {
    let v = top.iter().find(|(k, _)| k == key).map(|(_, v)| v)?;
    require_str(v, key).ok()
}

// ── Public I/O ────────────────────────────────────────────────────────────────

/// Write the corpus manifest to `dir/manifest.json`.
///
/// Uses direct file I/O rather than atomic rename.  The integration slice that
/// assembles the full pipeline can swap to `corpus::store::atomic_write` when
/// that module is available; atomic write is unnecessary for a manifest that is
/// always written once at the end of a successful extraction run.
pub fn write_manifest(dir: &Path, manifest: &Manifest) -> Result<(), CorpusError> {
    let json = write_manifest_json(manifest);
    let path = dir.join("manifest.json");
    fs::write(path, json.as_bytes())?;
    Ok(())
}

/// Read the corpus manifest from `dir/manifest.json`.
pub fn read_manifest(dir: &Path) -> Result<Manifest, CorpusError> {
    let path = dir.join("manifest.json");
    let bytes = fs::read(path)?;
    let s = std::str::from_utf8(&bytes)
        .map_err(|_| CorpusError::Json("manifest.json is not valid UTF-8".into()))?;
    parse_manifest_json(s)
}

/// Write `dir/filter_spec.txt` echoing the pinned filtering constants.
///
/// This is the M6.G↔M6.I contract surface: M6.I reads this file to confirm
/// it is consuming a corpus built with compatible filtering parameters.
/// The quiet-predicate description is included so the file is self-documenting.
pub fn write_filter_spec(dir: &Path) -> Result<(), CorpusError> {
    let content = format!(
        "# Corpus filter specification (M6.G↔M6.I contract surface)\n\
         # Do not edit — regenerate by re-running the corpus extraction pipeline.\n\
         #\n\
         # Quiet predicate: |static_eval - qsearch_score| <= QUIET_MARGIN_CP\n\
         # All scores in centipawns (side-to-move relative).\n\
         QUIET_MARGIN_CP={QUIET_MARGIN_CP}\n\
         OPENING_SKIP_PLIES={OPENING_SKIP_PLIES}\n\
         HIGH_SCORE_CP={HIGH_SCORE_CP}\n\
         PER_GAME_CAP={PER_GAME_CAP}\n"
    );
    let path = dir.join("filter_spec.txt");
    fs::write(path, content.as_bytes())?;
    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── SHA-256 known-answer tests ────────────────────────────────────────────

    /// FIPS 180-4 example vectors for SHA-256.
    #[test]
    fn sha256_known_vector() {
        // SHA-256("") = e3b0c442...
        assert_eq!(
            hex_digest(&sha256_bytes(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );

        // SHA-256("abc") = ba7816bf...
        assert_eq!(
            hex_digest(&sha256_bytes(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );

        // SHA-256 of a file containing the single byte 0x61 ('a') equals the
        // same as sha256_bytes(b"a").  We test sha256_file via a temp file.
        let dir = tempdir();
        let path = dir.path().join("a.bin");
        std::fs::write(&path, b"abc").unwrap();
        let file_digest = sha256_file(&path).unwrap();
        assert_eq!(
            hex_digest(&file_digest),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );

        // A longer NIST vector: SHA-256("abcdbcdecdefdefgefghfghighijhijkijkl
        // jklmklmnlmnomnopnopq") = 248d6a61...
        assert_eq!(
            hex_digest(&sha256_bytes(
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
            )),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    // ── JSON round-trip ───────────────────────────────────────────────────────

    /// Round-trip: construct a Manifest with ≥2 SourceEntry and non-default
    /// R-TC reproducibility fields, write+read, assert equal.
    #[test]
    fn manifest_json_roundtrip() {
        let manifest = Manifest {
            schema_version: 1,
            created_at: "2026-05-19T12:00:00Z".to_owned(),
            engine_commit: "1e8a410abc1234567890abcdef1234567890abcd".to_owned(),
            total_positions: 1_500_000,
            validation_fraction: 0.1,
            sources: vec![
                SourceEntry {
                    label: "ccrl-2022".to_owned(),
                    path: "/data/ccrl/ccrl-2022.pgn".to_owned(),
                    byte_count: 1_073_741_824,
                    sha256: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
                        .to_owned(),
                    positions_extracted: 900_000,
                },
                SourceEntry {
                    label: "lichess-2023-01".to_owned(),
                    path: "/data/lichess/2023-01.pgn.zst".to_owned(),
                    byte_count: 2_147_483_648,
                    sha256: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
                        .to_owned(),
                    positions_extracted: 600_000,
                },
            ],
            // f64-exact integers so the JSON number representation
            // roundtrips: under ~2^53. The pipeline uses small/structured
            // seeds (campaign defaults), not crypto-random u64s.
            self_play_seed: 12_345_678_901_234,
            games: 24,
            max_plies: 400,
            opening_random_plies: 8,
            workers: 1,
            split_seed: 98_765_432_109_876,
            val_fraction: 0.1,
            depth_ladder: vec![(4, 1), (6, 1), (8, 1), (10, 1)],
            opening_book_path: Some("bench/data/openings.epd".to_owned()),
            opening_book_sha256: Some(
                "dc91f225bc93e7ec091095bf8264595da33d36b9d3ac97ddd2dd54bc3a094fa4".to_owned(),
            ),
            opening_mode: Some("book".to_owned()),
            corpus_sha256: "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
                .to_owned(),
        };

        let dir = tempdir();
        write_manifest(dir.path(), &manifest).unwrap();
        let read_back = read_manifest(dir.path()).unwrap();
        assert_eq!(manifest, read_back);
    }

    // ── filter_spec echoes pinned constants ───────────────────────────────────

    /// `write_filter_spec` must embed the four pinned constants by name=value.
    #[test]
    fn filter_spec_echoes_pinned_constants() {
        let dir = tempdir();
        write_filter_spec(dir.path()).unwrap();
        let content = std::fs::read_to_string(dir.path().join("filter_spec.txt")).unwrap();

        // Each constant must appear as "NAME=VALUE" on its own line.
        assert!(
            content.contains(&format!("QUIET_MARGIN_CP={QUIET_MARGIN_CP}")),
            "missing QUIET_MARGIN_CP=30"
        );
        assert!(
            content.contains(&format!("OPENING_SKIP_PLIES={OPENING_SKIP_PLIES}")),
            "missing OPENING_SKIP_PLIES=8"
        );
        assert!(
            content.contains(&format!("HIGH_SCORE_CP={HIGH_SCORE_CP}")),
            "missing HIGH_SCORE_CP=600"
        );
        assert!(
            content.contains(&format!("PER_GAME_CAP={PER_GAME_CAP}")),
            "missing PER_GAME_CAP=10"
        );

        // Verify the literal values match what the plan specifies.
        assert_eq!(QUIET_MARGIN_CP, 30);
        assert_eq!(OPENING_SKIP_PLIES, 8);
        assert_eq!(HIGH_SCORE_CP, 600);
        assert_eq!(PER_GAME_CAP, 10);
    }

    // ── helpers ───────────────────────────────────────────────────────────────

    /// A minimal temp-directory RAII guard (no third-party deps).
    struct TempDir {
        path: std::path::PathBuf,
    }

    impl TempDir {
        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn tempdir() -> TempDir {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        // Use TMPDIR env var so sandbox-mode writes land in an allowed path.
        let base = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".to_owned());
        let path = std::path::Path::new(&base)
            .join(format!("clawfish-manifest-test-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&path).expect("create test tempdir");
        TempDir { path }
    }
}
