//! M6.I — Texel tuning harness (deferred eval-weight calibration).
//!
//! Standalone consumer of the frozen corpus lanes (ADR-0037). The engine
//! crate's `evaluate` / search / `eval::data` const *values* are NOT modified
//! by this harness ⇒ `evaluate` byte-identical to `M6.F`, bench `1213649` /
//! d4 `90591` byte-for-byte. The harness tunes the ~193 linear deferred-term
//! weights against the corpus, gated (by the operator) on a mixed-TC SPRT vs
//! `M6.F` per the ADR-0037 §9 verdict ladder.
//!
//! Module map (the §Files table of `docs/plans/m6.i.md`):
//! - [`layout`]   — the canonical flat index map for the linear-core weights.
//! - [`params`]   — the full runtime eval weight set + json + apply.
//! - [`features`] — the `reference_score_white` oracle + the fast cached model.
//! - [`dataset`]  — lane load + on-disk feature cache + game-level split + mix.
//! - [`loss`]     — sigmoid/MSE objective + closed-form gradient + L2/mono reg.
//! - [`optimizer`]— Adam + early-stop + checkpoint/resume + coord-descent check.
//! - [`mixture`]  — optional bi-level simplex meta-optimization.
//!
//! All modules are fully implemented: feature extraction, cache build + stream,
//! Adam optimizer with checkpoint/resume, bi-level simplex mixture, and the
//! `apply_to_data_rs` codegen rewriter.

pub mod dataset;
pub mod features;
pub mod layout;
pub mod loss;
pub mod mixture;
pub mod optimizer;
pub mod params;

use std::fmt;

/// Crate-wide Texel-harness error. Mirrors `corpus::CorpusError` style.
#[derive(Debug)]
pub enum TexelError {
    /// Underlying I/O failure (lane read, cache write, checkpoint, json file).
    Io(std::io::Error),
    /// `bench/m6-params.json` (or checkpoint) JSON malformed.
    Json(String),
    /// Parse / schema failure (FEN, schema-version mismatch, marker region).
    Parse(String),
    /// Feature-cache header / framing / layout-hash mismatch.
    Cache(String),
}

impl fmt::Display for TexelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TexelError::Io(e) => write!(f, "io: {e}"),
            TexelError::Json(s) => write!(f, "json: {s}"),
            TexelError::Parse(s) => write!(f, "parse: {s}"),
            TexelError::Cache(s) => write!(f, "cache: {s}"),
        }
    }
}

impl std::error::Error for TexelError {}

impl From<std::io::Error> for TexelError {
    fn from(e: std::io::Error) -> Self {
        TexelError::Io(e)
    }
}
