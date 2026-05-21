//! Vendored opening-book reader (CC0 `2moves_v1.epd`, `bench/data/openings.epd`).
//!
//! Provides random sampling of opening positions for self-play. The book
//! is consumed by an `OpeningMode::Book` self-play campaign (every record
//! tagged `Source::SelfPlayOnBook`); the complementary `OpeningMode::
//! Random` campaign starts every game from `startpos + opening_random_
//! plies` random plies and tags records `Source::SelfPlayOffBook`. The
//! book / off-book proportion is a training-time per-source reweighting
//! axis at M6.I (ADR-0035 §10), no longer a corpus-generation knob.
//!
//! Format: one 6-field FEN per line. Whole-file SHA-256 is recorded in
//! the manifest (`opening_book_sha256`) so the reproducibility re-run
//! match check detects upstream drift.

use std::fs;
use std::path::Path;

use crate::Position;

use super::CorpusError;
use super::manifest::{hex_digest, sha256_bytes};
use super::prng::Prng;

/// In-memory opening book: validated positions + a hex SHA-256 of the
/// source file (pinned in `Manifest.opening_book_sha256`).
#[derive(Clone, Debug)]
pub struct Book {
    positions: Vec<Position>,
    sha256: String,
    path: String,
}

impl Book {
    /// Load + validate an EPD file (one FEN per line). Comment lines
    /// (starting with `#`) and blank lines are skipped; every other line
    /// must parse as a `Position` or `load_epd` returns
    /// `CorpusError::Pgn` with the offending line number.
    pub fn load_epd(path: &Path) -> Result<Book, CorpusError> {
        let bytes = fs::read(path).map_err(CorpusError::Io)?;
        let sha256 = hex_digest(&sha256_bytes(&bytes));
        let text =
            std::str::from_utf8(&bytes).map_err(|e| CorpusError::Pgn(format!("utf8: {e}")))?;
        let mut positions = Vec::new();
        for (i, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let pos = Position::from_fen(line).map_err(|e| {
                CorpusError::Pgn(format!(
                    "{}:{}: bad FEN in opening book: {e:?}",
                    path.display(),
                    i + 1
                ))
            })?;
            positions.push(pos);
        }
        if positions.is_empty() {
            return Err(CorpusError::Pgn(format!(
                "{}: opening book has zero positions",
                path.display()
            )));
        }
        Ok(Book {
            positions,
            sha256,
            path: path.to_string_lossy().into_owned(),
        })
    }

    /// Uniformly sample one opening position (seeded). The book is
    /// non-empty by construction (`load_epd` rejects empty files).
    pub fn sample(&self, rng: &mut Prng) -> &Position {
        let n = self.positions.len() as u64;
        let idx = rng.below(n) as usize;
        &self.positions[idx]
    }

    /// Hex SHA-256 of the source EPD bytes (pinned in
    /// `Manifest.opening_book_sha256`).
    pub fn sha256(&self) -> &str {
        &self.sha256
    }

    /// Source-path string, recorded in `Manifest.opening_book_path`.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Number of positions in the book.
    pub fn len(&self) -> usize {
        self.positions.len()
    }

    /// `true` iff the book is empty (always `false` for a `load_epd`-
    /// constructed instance).
    pub fn is_empty(&self) -> bool {
        self.positions.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::corpus::prng::substream_seed;
    use std::io::Write;

    fn write_temp_epd(name: &str, contents: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("clawfish-openings-{name}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("book.epd");
        let mut f = fs::File::create(&p).unwrap();
        f.write_all(contents.as_bytes()).unwrap();
        p
    }

    #[test]
    fn load_epd_parses_valid_fens() {
        let p = write_temp_epd(
            "valid",
            "# comment\n\
             rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1\n\
             rnbqkbnr/ppp1pppp/8/3p4/3P4/8/PPP1PPPP/RNBQKBNR w KQkq - 0 2\n\
             \n\
             rnbqkbnr/pppp1ppp/8/4p3/4P3/8/PPPP1PPP/RNBQKBNR w KQkq - 0 2\n",
        );
        let b = Book::load_epd(&p).unwrap();
        assert_eq!(b.len(), 3);
        assert!(!b.is_empty());
        assert_eq!(b.sha256().len(), 64);
    }

    #[test]
    fn load_epd_rejects_bad_fen_with_line_number() {
        let p = write_temp_epd(
            "bad",
            "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1\n\
             not-a-fen\n",
        );
        match Book::load_epd(&p) {
            Err(CorpusError::Pgn(msg)) => assert!(msg.contains(":2:"), "msg: {msg}"),
            other => panic!("expected line-2 Pgn error, got {other:?}"),
        }
    }

    #[test]
    fn load_epd_rejects_empty_file() {
        let p = write_temp_epd("empty", "# only a comment\n\n");
        match Book::load_epd(&p) {
            Err(CorpusError::Pgn(msg)) => assert!(msg.contains("zero positions")),
            other => panic!("expected zero-positions error, got {other:?}"),
        }
    }

    #[test]
    fn sample_is_seed_deterministic() {
        let p = write_temp_epd(
            "sample",
            "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1\n\
             rnbqkbnr/ppp1pppp/8/3p4/3P4/8/PPP1PPPP/RNBQKBNR w KQkq - 0 2\n\
             rnbqkbnr/pppp1ppp/8/4p3/4P3/8/PPPP1PPP/RNBQKBNR w KQkq - 0 2\n",
        );
        let b = Book::load_epd(&p).unwrap();
        let mut a = Prng::new(substream_seed(0xC0FFEE, 1));
        let mut c = Prng::new(substream_seed(0xC0FFEE, 1));
        // 10 samples — both streams should pick the same indices.
        for _ in 0..10 {
            let sa = b.sample(&mut a);
            let sc = b.sample(&mut c);
            assert_eq!(sa.to_fen(), sc.to_fen());
        }
    }

    #[test]
    fn sample_visits_multiple_positions_over_many_draws() {
        let p = write_temp_epd(
            "spread",
            "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1\n\
             rnbqkbnr/ppp1pppp/8/3p4/3P4/8/PPP1PPPP/RNBQKBNR w KQkq - 0 2\n\
             rnbqkbnr/pppp1ppp/8/4p3/4P3/8/PPPP1PPP/RNBQKBNR w KQkq - 0 2\n",
        );
        let b = Book::load_epd(&p).unwrap();
        let mut rng = Prng::new(0x12345);
        let mut seen = std::collections::HashSet::new();
        for _ in 0..200 {
            seen.insert(b.sample(&mut rng).to_fen());
        }
        // With 200 draws over 3 positions the chance of missing one is
        // ~(2/3)^200 ≈ 1e-35 — effectively zero, so this is deterministic
        // under the fixed seed.
        assert_eq!(seen.len(), 3, "sample should visit all positions");
    }

    #[test]
    fn vendored_book_loads_and_matches_recorded_sha256() {
        // The vendored book at bench/data/openings.epd is committed; its
        // SHA-256 is pinned in bench/openings.md. Load it and verify both
        // the parse succeeds and the digest matches the documented value.
        let p = std::path::Path::new("bench/data/openings.epd");
        if !p.exists() {
            eprintln!("vendored opening book not present — skipping test");
            return;
        }
        let b = Book::load_epd(p).expect("vendored book must parse");
        assert!(
            b.len() >= 1000,
            "vendored book should have >=1000 positions"
        );
        assert_eq!(
            b.sha256(),
            "dc91f225bc93e7ec091095bf8264595da33d36b9d3ac97ddd2dd54bc3a094fa4",
            "vendored book SHA must match bench/openings.md — upstream drift would break reproducibility"
        );
    }
}
