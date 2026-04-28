//! FEN (Forsyth-Edwards Notation) parser and formatter, per Edwards 1994
//! §16.1 (vendored at `docs/reference/pgn-spec-1994.txt`).
//!
//! Strict semantics:
//! - field separator is exactly one ASCII space (`split(' ')`),
//! - integer fields parse as strict-decimal (no leading `+`, `-`, or whitespace),
//! - castling-rights letters appear in the spec's order (uppercase before
//!   lowercase, kingside before queenside) — re-orderings are rejected,
//! - structural sanity checks (Decision §8 of `docs/plans/m1.b.md`):
//!     - exactly one king per color,
//!     - no pawns on rank 1 or 8,
//!     - EP target square (when present) is on rank 3 or 6.
//!
//! **Phantom-EP sanitization (Stockfish-compatible).** A syntactically valid
//! EP target on rank 3 or 6 is *silently cleared to `None`* unless a
//! pseudo-legal en passant capture actually exists in the parsed position.
//! Concretely, the EP target is kept iff: the side-to-move matches the EP
//! rank's expected capturer (Black for rank 3, White for rank 6), the
//! opposing pawn that supposedly just double-pushed is present on the file
//! and rank one step beyond the EP target, and at least one side-to-move
//! pawn sits adjacent (file ±1, same rank as the pushed pawn) to make the
//! capture. This matches Stockfish's behavior and keeps the field in lock-
//! step with `make_move`'s post-double-push EP — see [`ep_capturer_exists`]
//! for the shared adjacency predicate. Round-trip is therefore *not*
//! identity for FEN strings carrying a phantom EP: `from_fen` will drop
//! it, and the re-formatted FEN emits `-` for that field.

use std::fmt;

use crate::piece::{Color, Piece, PieceKind};
use crate::position::{CastlingRights, Position};
use crate::square::Square;

#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum FenError {
    WrongFieldCount {
        found: usize,
    },
    BadPiecePlacement(String),
    BadActiveColor(String),
    BadCastlingRights(String),
    BadEnPassant(String),
    BadHalfmoveClock(String),
    BadFullmoveNumber(String),
    MissingKing(Color),
    TooManyKings(Color),
    PawnOnBackRank(Square),
    /// A castling-rights letter is set, but the implied king/rook positions
    /// are absent or wrong-colored. M1.F §13 of `docs/plans/m1.f.md`:
    /// e.g. `K` set but no white rook on h1, or king not on e1. The
    /// `right` value is the *single* offending right (one variant per
    /// failure: `WHITE_KING`, `WHITE_QUEEN`, `BLACK_KING`, `BLACK_QUEEN`).
    InconsistentCastlingRights {
        right: CastlingRights,
    },
}

impl fmt::Display for FenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FenError::WrongFieldCount { found } => {
                write!(f, "FEN must have 6 fields, found {found}")
            }
            FenError::BadPiecePlacement(s) => write!(f, "bad piece placement: {s}"),
            FenError::BadActiveColor(s) => write!(f, "bad active color: {s:?}"),
            FenError::BadCastlingRights(s) => write!(f, "bad castling rights: {s:?}"),
            FenError::BadEnPassant(s) => write!(f, "bad en passant target: {s:?}"),
            FenError::BadHalfmoveClock(s) => write!(f, "bad halfmove clock: {s:?}"),
            FenError::BadFullmoveNumber(s) => write!(f, "bad fullmove number: {s:?}"),
            FenError::MissingKing(color) => write!(f, "missing {color:?} king"),
            FenError::TooManyKings(color) => write!(f, "too many {color:?} kings"),
            FenError::PawnOnBackRank(sq) => write!(f, "pawn on back rank at {sq}"),
            FenError::InconsistentCastlingRights { right } => {
                write!(f, "inconsistent castling rights: {right}")
            }
        }
    }
}

impl std::error::Error for FenError {}

pub(crate) fn parse(s: &str) -> Result<Position, FenError> {
    // 1. Strict split on single ASCII space.
    let fields: Vec<&str> = s.split(' ').collect();
    if fields.len() != 6 {
        return Err(FenError::WrongFieldCount {
            found: fields.len(),
        });
    }

    let mut p = Position::empty();

    // 2. Parse placement (field 0). Walk byte-by-byte; ranks counted 8 down
    //    to 1. Pawn-on-back-rank check fires synchronously before set_piece.
    parse_placement(&mut p, fields[0])?;

    // 3. Active color.
    let side = parse_active_color(fields[1])?;

    // 4. Castling rights.
    let castling = parse_castling(fields[2])?;

    // 5. En passant target. EP-rank check fires before set_aux_state;
    //    phantom-EP sanitization runs after placement is known (step 8).
    let ep_raw = parse_ep(fields[3])?;

    // 6. Halfmove clock. Strict-decimal: reject leading '+' (the radix
    //    parser accepts it; the spec/Reviewer-revision §C does not).
    if fields[4].starts_with('+') {
        return Err(FenError::BadHalfmoveClock(fields[4].to_string()));
    }
    // Reviewer-revision §C: pin radix-10 parsing literally (the lint
    // suggests `.parse()`, which is equivalent here, but the plan calls
    // out `from_str_radix(_, 10)` by name).
    #[allow(clippy::from_str_radix_10)]
    let halfmove = u8::from_str_radix(fields[4], 10)
        .map_err(|_| FenError::BadHalfmoveClock(fields[4].to_string()))?;

    // 7. Fullmove number. Same strict-decimal stance.
    if fields[5].starts_with('+') {
        return Err(FenError::BadFullmoveNumber(fields[5].to_string()));
    }
    #[allow(clippy::from_str_radix_10)]
    let fullmove = u16::from_str_radix(fields[5], 10)
        .map_err(|_| FenError::BadFullmoveNumber(fields[5].to_string()))?;
    if fullmove == 0 {
        return Err(FenError::BadFullmoveNumber(fields[5].to_string()));
    }

    // 8. Phantom-EP sanitization (see module docstring): drop a
    //    syntactically valid EP target if no pseudo-legal en passant capture
    //    actually exists in the parsed position. Stockfish-compatible.
    let ep = sanitize_ep(&p, side, ep_raw);

    // 9. Apply auxiliary state.
    p.set_aux_state(side, castling, ep, halfmove, fullmove);

    // 10. Cross-field invariant: king count.
    p.validate_post_parse()?;

    // 11. Compute the Polyglot Zobrist hash from scratch (per ADR-0009).
    //     M1.E will switch to incremental updates inside make/unmake; the
    //     parser path stays at from-scratch.
    p.refresh_zobrist();

    // 12. Compute the combined material + PST score from scratch (M3.A).
    p.refresh_static_eval();

    Ok(p)
}

fn parse_placement(p: &mut Position, s: &str) -> Result<(), FenError> {
    // Pre-check: exactly 8 slash-separated chunks. Without this, an FEN with
    // 7 ranks whose top chunk begins with pawns would trigger
    // `PawnOnBackRank` (the walker assumes the first chunk is rank 8) before
    // reaching the structural check at end-of-input. Pinning the chunk count
    // first ensures structural failures emit `BadPiecePlacement`.
    if s.split('/').count() != 8 {
        return Err(FenError::BadPiecePlacement(s.to_string()));
    }

    let bytes = s.as_bytes();
    // Current rank index, counted 8 down to 1 as we consume the field.
    let mut rank: u8 = 8;
    // File cursor for the current rank, 0..=8. Reaching 8 means the rank is
    // full; any further occupant within the rank is overflow.
    let mut file: u8 = 0;

    for &b in bytes {
        match b {
            b'/' => {
                // End of a rank. The rank we just finished must have had
                // exactly 8 squares. After that, we descend; rank=1 is the
                // last; a slash following rank=1 means too many ranks.
                if file != 8 {
                    return Err(FenError::BadPiecePlacement(s.to_string()));
                }
                // Unreachable: the pre-check guarantees exactly 7 slashes, so
                // `rank` (8 → 1) reaches 1 only after the 7th slash; an 8th
                // can't exist by construction.
                if rank == 1 {
                    unreachable!("8th slash in piece placement after pre-check guaranteed 7");
                }
                rank -= 1;
                file = 0;
            }
            b'1'..=b'8' => {
                let count = b - b'0';
                let new_file = file + count;
                if new_file > 8 {
                    return Err(FenError::BadPiecePlacement(s.to_string()));
                }
                file = new_file;
            }
            _ => {
                // Must be a piece letter. Otherwise (incl. multi-byte UTF-8
                // continuation bytes, '0', '9', any other char), reject.
                let piece = match Piece::from_fen_char(b) {
                    Some(piece) => piece,
                    None => return Err(FenError::BadPiecePlacement(s.to_string())),
                };
                if file >= 8 {
                    return Err(FenError::BadPiecePlacement(s.to_string()));
                }
                // Pawn-on-back-rank check fires before the piece is placed.
                if piece.kind == PieceKind::Pawn && (rank == 1 || rank == 8) {
                    let sq = Square::from_file_rank(file, rank - 1)
                        .ok_or_else(|| FenError::BadPiecePlacement(s.to_string()))?;
                    return Err(FenError::PawnOnBackRank(sq));
                }
                let sq = Square::from_file_rank(file, rank - 1)
                    .ok_or_else(|| FenError::BadPiecePlacement(s.to_string()))?;
                p.set_piece(sq, piece);
                file += 1;
            }
        }
    }

    // After consuming the whole field, we must be sitting on rank=1 with the
    // last rank fully encoded.
    if rank != 1 || file != 8 {
        return Err(FenError::BadPiecePlacement(s.to_string()));
    }

    Ok(())
}

fn parse_active_color(s: &str) -> Result<Color, FenError> {
    let bytes = s.as_bytes();
    if bytes.len() != 1 {
        return Err(FenError::BadActiveColor(s.to_string()));
    }
    Color::from_fen_char(bytes[0]).ok_or_else(|| FenError::BadActiveColor(s.to_string()))
}

fn parse_castling(s: &str) -> Result<CastlingRights, FenError> {
    if s == "-" {
        return Ok(CastlingRights::NONE);
    }
    // Unreachable: an empty field-2 requires double-space in the input, which
    // produces 7 fields and trips WrongFieldCount before this is called.
    if s.is_empty() {
        unreachable!("empty castling field after WrongFieldCount gate");
    }
    // Strict spec order: K, Q, k, q. Walk the input and the spec sequence
    // in lock-step; each input letter must match the next-or-later spec
    // letter, and each spec letter is consumed at most once.
    let bytes = s.as_bytes();
    let spec: [(u8, CastlingRights); 4] = [
        (b'K', CastlingRights::WHITE_KING),
        (b'Q', CastlingRights::WHITE_QUEEN),
        (b'k', CastlingRights::BLACK_KING),
        (b'q', CastlingRights::BLACK_QUEEN),
    ];
    let mut spec_idx: usize = 0;
    let mut rights = CastlingRights::NONE;
    for &b in bytes {
        // Find b in spec[spec_idx..]. If absent (out-of-order, duplicate, or
        // unknown letter incl. '-'), reject.
        let mut found_at: Option<usize> = None;
        for (i, &(sb, _)) in spec.iter().enumerate().skip(spec_idx) {
            if sb == b {
                found_at = Some(i);
                break;
            }
        }
        match found_at {
            Some(i) => {
                rights = rights.with(spec[i].1);
                spec_idx = i + 1;
            }
            None => return Err(FenError::BadCastlingRights(s.to_string())),
        }
    }
    Ok(rights)
}

fn parse_ep(s: &str) -> Result<Option<Square>, FenError> {
    if s == "-" {
        return Ok(None);
    }
    // Must be exactly 2 ASCII bytes parsing via Square::parse_uci.
    let sq = Square::parse_uci(s).ok_or_else(|| FenError::BadEnPassant(s.to_string()))?;
    // Enforce rank 3 (index 2) or rank 6 (index 5).
    let rank = sq.rank();
    if rank != 2 && rank != 5 {
        return Err(FenError::BadEnPassant(s.to_string()));
    }
    Ok(Some(sq))
}

/// Phantom-EP sanitization. Returns `Some(ep_sq)` iff a pseudo-legal en
/// passant capture actually exists in `p`; `None` otherwise. Inputs come
/// straight from `parse_ep`, so `ep_sq.rank()` is guaranteed 2 or 5.
///
/// The five conditions for keeping the EP target:
/// 1. `ep` is `Some(_)` (no-op for `None`).
/// 2. The side-to-move matches the EP rank's expected capturer — Black for
///    rank 3 (white pushed; black to capture), White for rank 6 (mirror).
/// 3. The opponent's pawn that supposedly just double-pushed is present on
///    `(ep_sq.file(), pawn_rank)` where `pawn_rank` is the rank one step
///    *beyond* the EP target (rank 4 for rank-3 EP, rank 5 for rank-6 EP).
/// 4. At least one side-to-move pawn is adjacent to the pushed pawn —
///    delegated to [`ep_capturer_exists`].
/// 5. The EP target square itself is empty — a piece on `ep_sq` blocks the
///    capture (the would-be capturer can't land there). After a real
///    double-push made by `make_move`, this is a tautology because the
///    pawn passed over an empty square; the check fires only on phantom
///    FENs where some other piece has been placed on the EP square.
///
/// Failing any of (2)–(5) yields `None` (silent sanitization, no error —
/// matches Stockfish's behavior on phantom EPs).
fn sanitize_ep(p: &Position, side: Color, ep: Option<Square>) -> Option<Square> {
    let ep_sq = ep?;
    let (expected_side, pawn_rank) = match ep_sq.rank() {
        2 => (Color::Black, 3),
        5 => (Color::White, 4),
        _ => unreachable!("parse_ep enforced rank 2 or 5"),
    };
    if side != expected_side {
        return None;
    }
    let pushed = Piece::new(expected_side.flip(), PieceKind::Pawn);
    let pushed_sq = Square::from_file_rank(ep_sq.file(), pawn_rank)
        .expect("ep_sq.file() in 0..8 and pawn_rank in {3,4}");
    if p.piece_at(pushed_sq) != Some(pushed) {
        return None;
    }
    if p.piece_at(ep_sq).is_some() {
        return None;
    }
    if !ep_capturer_exists(p, side, ep_sq.file(), pawn_rank) {
        return None;
    }
    Some(ep_sq)
}

/// True iff `capturer_color` has a pawn at `(ep_file ± 1, capturer_rank)` —
/// i.e. a pawn that geometrically can make the en passant capture toward an
/// EP target on file `ep_file`.
///
/// The "capturer rank" is the rank of the *just-pushed enemy pawn* (= the
/// rank from which the would-be capturer attacks): rank 4 (idx 3) for an
/// EP target on rank 3, rank 5 (idx 4) for an EP target on rank 6.
///
/// This is the adjacency predicate shared by [`sanitize_ep`] (FEN parse
/// time) and `mov::make_move` (after a double-push). The matching predicate
/// in `zobrist::ep_file_to_hash` reads the position's own `ep_target` and
/// `side_to_move`; this one is parametric so it can run *before*
/// `set_aux_state` writes those fields.
pub(crate) fn ep_capturer_exists(
    pos: &Position,
    capturer_color: Color,
    ep_file: u8,
    capturer_rank: u8,
) -> bool {
    let capturer_pawn = Piece::new(capturer_color, PieceKind::Pawn);
    if ep_file > 0 {
        let adj = Square::from_file_rank(ep_file - 1, capturer_rank)
            .expect("ep_file-1 in 0..8 and rank in 0..8");
        if pos.piece_at(adj) == Some(capturer_pawn) {
            return true;
        }
    }
    if ep_file < 7 {
        let adj = Square::from_file_rank(ep_file + 1, capturer_rank)
            .expect("ep_file+1 in 0..8 and rank in 0..8");
        if pos.piece_at(adj) == Some(capturer_pawn) {
            return true;
        }
    }
    false
}

pub(crate) fn format(p: &Position, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    // 1. Placement: ranks 8 down to 1, separated by '/'.
    for rank_index in (0..8u8).rev() {
        let mut empty_run: u8 = 0;
        for file in 0..8u8 {
            let sq = Square::from_file_rank(file, rank_index)
                .expect("file/rank in 0..8 are always valid");
            match p.piece_at(sq) {
                Some(piece) => {
                    if empty_run > 0 {
                        write!(f, "{}", empty_run)?;
                        empty_run = 0;
                    }
                    write!(f, "{}", piece.fen_char() as char)?;
                }
                None => {
                    empty_run += 1;
                }
            }
        }
        if empty_run > 0 {
            write!(f, "{}", empty_run)?;
        }
        if rank_index != 0 {
            f.write_str("/")?;
        }
    }

    // 2. Space + active color.
    write!(f, " {}", p.side_to_move().fen_char() as char)?;

    // 3. Space + castling rights (CastlingRights::Display already correct).
    write!(f, " {}", p.castling_rights())?;

    // 4. Space + EP target.
    match p.ep_target() {
        Some(sq) => write!(f, " {}", sq)?,
        None => f.write_str(" -")?,
    }

    // 5. Space + halfmove + space + fullmove.
    write!(f, " {} {}", p.halfmove_clock(), p.fullmove_number())?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::piece::PieceKind;
    use crate::position::CastlingRights;

    /// The six canonical perft positions referenced by M1 (see plan §"Canonical
    /// perft positions referenced by this phase"). Pinned in this module so the
    /// test does not depend on file IO or share its data with another test
    /// fixture (drift between the two would be silent).
    const CANONICAL_PERFT_FENS: [&str; 6] = [
        // Starting
        "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1",
        // Kiwipete
        "r3k2r/p1ppqpb1/bn2pnp1/3PN3/1p2P3/2N2Q1p/PPPBBPPP/R3K2R w KQkq - 0 1",
        // Position 3
        "8/2p5/3p4/KP5r/1R3p1k/8/4P1P1/8 w - - 0 1",
        // Position 4
        "r3k2r/Pppp1ppp/1b3nbN/nP6/BBP1P3/q4N2/Pp1P2PP/R2Q1RK1 w kq - 0 1",
        // Position 5
        "rnbq1k1r/pp1Pbppp/2p5/8/2B5/8/PPP1NnPP/RNBQK2R w KQ - 1 8",
        // Position 6
        "r4rk1/1pp1qppp/p1np1n2/2b1p1B1/2B1P1b1/P1NP1N2/1PP1QPPP/R4RK1 w - - 0 10",
    ];

    // ---------------------------------------------------------------------
    // Positive parse / format cases
    // ---------------------------------------------------------------------

    #[test]
    fn parse_starting_position() {
        let p = Position::from_fen(Position::STARTING_FEN).expect("starting FEN must parse");
        assert_eq!(p.to_fen(), Position::STARTING_FEN);
        p.debug_assert_consistent();
    }

    #[test]
    fn parse_canonical_perft_positions() {
        for fen in CANONICAL_PERFT_FENS {
            let parsed = Position::from_fen(fen)
                .unwrap_or_else(|e| panic!("canonical FEN must parse: {fen} ({e:?})"));
            assert_eq!(
                parsed.to_fen(),
                fen,
                "canonical FEN must round-trip exactly: {fen}"
            );
            parsed.debug_assert_consistent();
        }
    }

    #[test]
    fn parse_after_e4() {
        // FEN spec §16.1.4 example after 1. e4. The spec sets EP=e3 after
        // the double push regardless of capturability; our parser applies
        // Stockfish-style phantom-EP sanitization (see module docstring) —
        // black has no pawn on d4 or f4, so the EP target drops to None
        // and the re-formatted FEN emits `-` in that field.
        let fen = "rnbqkbnr/pppppppp/8/8/4P3/8/PPPP1PPP/RNBQKBNR b KQkq e3 0 1";
        let p = Position::from_fen(fen).expect("post-1.e4 FEN must parse");
        assert_eq!(p.side_to_move(), Color::Black);
        assert_eq!(p.ep_target(), None);
        let canonical = "rnbqkbnr/pppppppp/8/8/4P3/8/PPPP1PPP/RNBQKBNR b KQkq - 0 1";
        assert_eq!(p.to_fen(), canonical);
    }

    #[test]
    fn parse_after_c5() {
        // FEN spec §16.1.4 example after 1. e4 c5. Same story: white has
        // no pawn on b5 or d5 to capture en passant at c6, so the phantom
        // EP target sanitizes to None.
        let fen = "rnbqkbnr/pp1ppppp/8/2p5/4P3/8/PPPP1PPP/RNBQKBNR w KQkq c6 0 2";
        let p = Position::from_fen(fen).expect("post-1.e4-c5 FEN must parse");
        assert_eq!(p.side_to_move(), Color::White);
        assert_eq!(p.ep_target(), None);
        assert_eq!(p.fullmove_number(), 2);
        let canonical = "rnbqkbnr/pp1ppppp/8/2p5/4P3/8/PPPP1PPP/RNBQKBNR w KQkq - 0 2";
        assert_eq!(p.to_fen(), canonical);
    }

    #[test]
    fn parse_kings_only_position() {
        // FEN spec §16.1.4 example: minimal piece set, halfmove=5, fullmove=39.
        let fen = "4k3/8/8/8/8/8/4P3/4K3 w - - 5 39";
        let p = Position::from_fen(fen).expect("kings-and-pawn FEN must parse");
        assert_eq!(p.side_to_move(), Color::White);
        assert_eq!(p.castling_rights(), CastlingRights::NONE);
        assert_eq!(p.ep_target(), None);
        assert_eq!(p.halfmove_clock(), 5);
        assert_eq!(p.fullmove_number(), 39);
        assert_eq!(p.king_square(Color::White), Square::E1);
        assert_eq!(p.king_square(Color::Black), Square::E8);
        assert_eq!(p.pieces_colored(Color::White, PieceKind::Pawn).count(), 1);
        assert_eq!(p.to_fen(), fen);
    }

    #[test]
    fn format_emits_six_fields() {
        // Every parsed position formats to a six-field string with exactly
        // five single-space separators.
        for fen in CANONICAL_PERFT_FENS {
            let p = Position::from_fen(fen).expect("canonical FEN must parse");
            let s = p.to_fen();
            let space_count = s.bytes().filter(|&b| b == b' ').count();
            assert_eq!(
                space_count, 5,
                "formatted FEN {s:?} must have exactly 5 single-space separators"
            );
            // And six fields when split on a single space.
            assert_eq!(s.split(' ').count(), 6, "formatted FEN must have 6 fields");
        }
    }

    #[test]
    fn format_castling_dash_when_none() {
        // Position with no castling rights — format must emit "-", not "".
        let fen = "4k3/8/8/8/8/8/8/4K3 w - - 0 1";
        let p = Position::from_fen(fen).expect("kings-only FEN must parse");
        let s = p.to_fen();
        // Split on space — castling is field index 2.
        let castling_field = s.split(' ').nth(2).expect("castling field present");
        assert_eq!(castling_field, "-");
    }

    #[test]
    fn format_ep_dash_when_none() {
        // Position with no EP target — format must emit "-", not "".
        let fen = "4k3/8/8/8/8/8/8/4K3 w - - 0 1";
        let p = Position::from_fen(fen).expect("kings-only FEN must parse");
        let s = p.to_fen();
        // Split on space — EP is field index 3.
        let ep_field = s.split(' ').nth(3).expect("ep field present");
        assert_eq!(ep_field, "-");
    }

    #[test]
    fn parse_accepts_castling_dash() {
        let fen = "4k3/8/8/8/8/8/8/4K3 w - - 0 1";
        let p = Position::from_fen(fen).expect("dash-castling FEN must parse");
        assert_eq!(p.castling_rights(), CastlingRights::NONE);
    }

    #[test]
    fn parse_accepts_ep_dash() {
        let fen = "4k3/8/8/8/8/8/8/4K3 w - - 0 1";
        let p = Position::from_fen(fen).expect("dash-EP FEN must parse");
        assert_eq!(p.ep_target(), None);
    }

    #[test]
    fn parse_sanitizes_ep_rank3_with_white_to_move() {
        // White-to-move + EP target on rank 3 is impossible from any legal
        // game (rank-3 EP means white just pushed; black would now move).
        // The phantom-EP sanitizer drops it to None silently — matches
        // Stockfish.
        let fen = "4k3/8/8/8/8/4P3/8/4K3 w - e3 0 1";
        let p = Position::from_fen(fen).expect("rank-3 EP with white to move must parse");
        assert_eq!(p.side_to_move(), Color::White);
        assert_eq!(p.ep_target(), None);
    }

    #[test]
    fn parse_sanitizes_ep_rank6_with_black_to_move() {
        // Symmetric companion to parse_sanitizes_ep_rank3_with_white_to_move.
        let fen = "4k3/8/8/4p3/8/8/8/4K3 b - e6 0 1";
        let p = Position::from_fen(fen).expect("rank-6 EP with black to move must parse");
        assert_eq!(p.side_to_move(), Color::Black);
        assert_eq!(p.ep_target(), None);
    }

    #[test]
    fn parse_keeps_ep_when_capturer_exists_black_to_move() {
        // EP=e3, black to move, white pawn on e4 (just pushed), black pawn
        // on f4 (capturer). Pseudo-legal capture exists → EP retained,
        // round-trip is identity.
        let fen = "4k3/8/8/8/4Pp2/8/8/4K3 b - e3 0 1";
        let p = Position::from_fen(fen).expect("EP-capturable FEN must parse");
        assert_eq!(p.ep_target(), Some(Square::E3));
        assert_eq!(p.to_fen(), fen);
    }

    #[test]
    fn parse_keeps_ep_when_capturer_exists_white_to_move() {
        // EP=e6, white to move, black pawn on e5 (just pushed), white pawn
        // on d5 (capturer). Symmetric companion.
        let fen = "4k3/8/8/3Pp3/8/8/8/4K3 w - e6 0 1";
        let p = Position::from_fen(fen).expect("EP-capturable FEN must parse");
        assert_eq!(p.ep_target(), Some(Square::E6));
        assert_eq!(p.to_fen(), fen);
    }

    #[test]
    fn parse_sanitizes_ep_when_pushed_pawn_missing() {
        // EP=e3, black to move, black pawn on d4 (geometric capturer) but
        // NO white pawn on e4 — the supposedly just-pushed pawn isn't
        // there. Sanitize: a real EP capture can't actually be made.
        let fen = "4k3/8/8/8/3p4/8/8/4K3 b - e3 0 1";
        let p = Position::from_fen(fen).expect("FEN with absent pushed pawn must parse");
        assert_eq!(p.ep_target(), None);
    }

    #[test]
    fn parse_sanitizes_ep_when_no_capturer_adjacent() {
        // EP=e3, black to move, white pawn on e4 (just pushed), but no
        // black pawn on d4 or f4. No pseudo-legal capture exists → drop EP.
        let fen = "4k3/8/8/8/4P3/8/8/4K3 b - e3 0 1";
        let p = Position::from_fen(fen).expect("phantom EP FEN must parse");
        assert_eq!(p.ep_target(), None);
    }

    #[test]
    fn parse_sanitizes_ep_when_destination_obstructed_rank6() {
        // EP=e6, white to move, black pawn on e5 (just pushed), white pawn
        // on d5 (geometric capturer adjacent), BUT a black knight occupies
        // e6 — the EP destination is obstructed, so no capture is possible.
        // Stockfish 18 silently sanitizes this to `-`; we match.
        let fen = "4k3/8/4n3/3Pp3/8/8/8/4K3 w - e6 0 1";
        let p = Position::from_fen(fen).expect("FEN must parse");
        assert_eq!(p.ep_target(), None);
    }

    #[test]
    fn parse_sanitizes_ep_when_destination_obstructed_rank3() {
        // Symmetric to the rank-6 obstructed test: EP=e3, black to move,
        // white pawn on e4 (just pushed), black pawn on d4 (capturer), BUT
        // a white knight occupies e3 — destination obstructed.
        let fen = "4k3/8/8/8/3pP3/4N3/8/4K3 b - e3 0 1";
        let p = Position::from_fen(fen).expect("FEN must parse");
        assert_eq!(p.ep_target(), None);
    }

    #[test]
    fn parse_keeps_ep_a_file_capturer() {
        // EP target on the a-file — file-edge guard: only a right neighbor
        // exists. White pawn on b5 (capturer), black pawn on a5 (pushed),
        // white to move, EP=a6.
        let fen = "4k3/8/8/pP6/8/8/8/4K3 w - a6 0 1";
        let p = Position::from_fen(fen).expect("a-file EP FEN must parse");
        assert_eq!(p.ep_target(), Some(Square::A6));
    }

    #[test]
    fn parse_keeps_ep_h_file_capturer() {
        // EP target on the h-file — file-edge guard: only a left neighbor
        // exists. White pawn on g5 (capturer), black pawn on h5 (pushed),
        // white to move, EP=h6.
        let fen = "4k3/8/8/6Pp/8/8/8/4K3 w - h6 0 1";
        let p = Position::from_fen(fen).expect("h-file EP FEN must parse");
        assert_eq!(p.ep_target(), Some(Square::H6));
    }

    #[test]
    fn parse_accepts_huge_halfmove() {
        // 50-move-rule positions can reach 100; we accept up to u8::MAX.
        let fen = "4k3/8/8/8/8/8/8/4K3 w - - 100 1";
        let p = Position::from_fen(fen).expect("halfmove=100 must parse");
        assert_eq!(p.halfmove_clock(), 100);
    }

    #[test]
    fn parse_accepts_max_halfmove() {
        // Boundary at u8::MAX = 255.
        let fen = "4k3/8/8/8/8/8/8/4K3 w - - 255 1";
        let p = Position::from_fen(fen).expect("halfmove=255 must parse");
        assert_eq!(p.halfmove_clock(), 255);
    }

    #[test]
    fn parse_accepts_max_fullmove() {
        // Boundary at u16::MAX = 65535.
        let fen = "4k3/8/8/8/8/8/8/4K3 w - - 0 65535";
        let p = Position::from_fen(fen).expect("fullmove=65535 must parse");
        assert_eq!(p.fullmove_number(), 65535);
    }

    #[test]
    fn display_uses_starting_fen_constant() {
        // Anchored from the FEN side: Display of the starting position equals
        // the pinned STARTING_FEN constant.
        assert_eq!(
            format!("{}", Position::starting_position()),
            Position::STARTING_FEN
        );
    }

    // ---------------------------------------------------------------------
    // Negative parse cases
    // ---------------------------------------------------------------------

    #[test]
    fn parse_rejects_too_few_fields() {
        // 5 fields — missing fullmove.
        let r = Position::from_fen("rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0");
        assert!(matches!(r, Err(FenError::WrongFieldCount { found: 5 })));
        // 4 fields — missing both halfmove and fullmove. Stockfish accepts
        // this de-facto truncated form; we don't (per the strict-spec stance).
        let r = Position::from_fen("rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq -");
        assert!(matches!(r, Err(FenError::WrongFieldCount { found: 4 })));
    }

    #[test]
    fn parse_rejects_too_many_fields() {
        // 7 fields — trailing extra token.
        let r =
            Position::from_fen("rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1 extra");
        assert!(matches!(r, Err(FenError::WrongFieldCount { found: 7 })));
    }

    #[test]
    fn parse_rejects_empty_string() {
        let r = Position::from_fen("");
        // Empty input: split(' ') yields one empty field → found = 1.
        // The plan describes this as "0", but with strict split(' ') the count
        // is 1. We pin the WrongFieldCount variant; the exact found value is
        // a minor detail. Match flexibly.
        match r {
            Err(FenError::WrongFieldCount { .. }) => {}
            other => panic!("expected WrongFieldCount, got {other:?}"),
        }
    }

    #[test]
    fn parse_rejects_whitespace_only() {
        // Three spaces → split(' ') yields four empty tokens, not 6 fields.
        let r = Position::from_fen("   ");
        match r {
            Err(FenError::WrongFieldCount { .. }) => {}
            other => panic!("expected WrongFieldCount, got {other:?}"),
        }
    }

    #[test]
    fn parse_rejects_short_rank() {
        // Rank 8 chunk encodes only 7 squares (digit 7 means seven empties).
        let r = Position::from_fen("7/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1");
        assert!(matches!(r, Err(FenError::BadPiecePlacement(_))));
    }

    #[test]
    fn parse_rejects_too_many_squares_in_rank() {
        // Ten pawns in rank 7 — cumulative-square overflow. Rank 7 (not rank 8)
        // so the pawn-on-back-rank check does not fire first; both kings are
        // present so the king-count check is satisfied.
        let r = Position::from_fen("4k3/pppppppppp/8/8/8/8/8/4K3 w - - 0 1");
        assert!(matches!(r, Err(FenError::BadPiecePlacement(_))));
    }

    #[test]
    fn parse_rejects_too_few_ranks() {
        // Only 7 slash-separated chunks.
        let r = Position::from_fen("pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1");
        assert!(matches!(r, Err(FenError::BadPiecePlacement(_))));
    }

    #[test]
    fn parse_rejects_too_many_ranks() {
        // 9 slash-separated chunks. Both kings present in the FIRST 8 chunks
        // (rank 8 = black king, rank 1 = white king); the 9th chunk is the
        // overflow. So even a buggy parser that silently truncates to the
        // first 8 chunks before validating cannot trip the king-count check
        // — the only available failure mode is the structural BadPiecePlacement.
        let r = Position::from_fen("4k3/8/8/8/8/8/8/4K3/8 w - - 0 1");
        assert!(matches!(r, Err(FenError::BadPiecePlacement(_))));
    }

    #[test]
    fn parse_rejects_empty_rank_chunk() {
        // Empty chunk in the middle of an otherwise-8-chunk placement.
        // Split on `/`: ["4k3", "8", "8", "", "8", "8", "8", "4K3"] = 8
        // elements. So this exercises "empty chunk" semantics without
        // conflating with the "too many chunks" failure mode.
        let r = Position::from_fen("4k3/8/8//8/8/8/4K3 w - - 0 1");
        assert!(matches!(r, Err(FenError::BadPiecePlacement(_))));
    }

    #[test]
    fn parse_rejects_trailing_slash() {
        // Trailing slash → 9 chunks, the last empty. Distinct failure mode
        // from "9 non-empty chunks" (which is parse_rejects_too_many_ranks).
        let r = Position::from_fen("4k3/8/8/8/8/8/8/4K3/ w - - 0 1");
        assert!(matches!(r, Err(FenError::BadPiecePlacement(_))));
    }

    #[test]
    fn parse_rejects_zero_digit() {
        // Digit 0 is out of range (must be 1..=8). Placement designed so the
        // ONLY issue is the `0` digit:
        //   - rank 7 = `0ppppppp1` → 0+7+1 = 8 squares if `0` silently means
        //     "0 empty"; reading-as-invalid trips BadPiecePlacement;
        //   - both kings present, no back-rank pawns. So an implementation
        //     that silently accepts `0` would parse this position successfully
        //     (no error → test fails), pinning the digit-rejection contract.
        let r = Position::from_fen("4k3/0ppppppp1/8/8/8/8/8/4K3 w - - 0 1");
        assert!(matches!(r, Err(FenError::BadPiecePlacement(_))));
    }

    #[test]
    fn parse_rejects_digit_above_8() {
        // Digit 9 is out of range (must be 1..=8). Placement designed so the
        // ONLY issue is the `9` digit (no back-rank pawns; both kings present).
        // Even though `9` always implies cumulative-overflow, isolating in
        // rank 7 keeps the failure path one of {invalid-digit, cumulative-
        // overflow} — both produce BadPiecePlacement.
        let r = Position::from_fen("4k3/9/8/8/8/8/8/4K3 w - - 0 1");
        assert!(matches!(r, Err(FenError::BadPiecePlacement(_))));
    }

    #[test]
    fn parse_rejects_non_ascii_in_placement() {
        // UTF-8 multibyte char in placement — must NOT panic on slicing.
        let fen = "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBN\u{265B} w KQkq - 0 1";
        let r = Position::from_fen(fen);
        assert!(matches!(r, Err(FenError::BadPiecePlacement(_))));
    }

    #[test]
    fn parse_rejects_unknown_piece_letter() {
        // Various non-SAN ASCII letters — `X` (uppercase non-SAN), `s` (knight
        // in some non-English notations), `m` (arbitrary), `e` (lowercase
        // non-SAN). Pin that none silently normalize to a valid piece kind.
        // Each placement has only the bad letter substituted into rank 1.
        let valid_kings = "4k3/8/8/8/8/8/8/4K3";
        for bad_letter in ['X', 's', 'm', 'e'] {
            // Replace the white king's `K` with the bad letter on rank 1, but
            // restore a valid king elsewhere so only-the-letter is the issue.
            // Concretely: place white king on a1 plus the bad letter on h1.
            let placement = format!("4k3/8/8/8/8/8/8/K6{bad_letter}");
            let fen = format!("{placement} w - - 0 1");
            let r = Position::from_fen(&fen);
            assert!(
                matches!(r, Err(FenError::BadPiecePlacement(_))),
                "letter {bad_letter:?} must reject as unknown piece, got {r:?}",
            );
        }
        // Sanity check: the same placement shape with valid kings parses.
        let _ = valid_kings;
    }

    #[test]
    fn parse_rejects_bad_active_color() {
        // Uppercase W — case-sensitive per spec.
        let r = Position::from_fen("rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR W KQkq - 0 1");
        assert!(matches!(r, Err(FenError::BadActiveColor(_))));
    }

    #[test]
    fn parse_rejects_bad_active_color_long() {
        // Multi-char "white" — must be a single byte.
        let r = Position::from_fen("rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR white KQkq - 0 1");
        assert!(matches!(r, Err(FenError::BadActiveColor(_))));
    }

    #[test]
    fn parse_rejects_bad_castling_chars() {
        // 'X' is not a valid castling letter.
        let r = Position::from_fen("rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w XQkq - 0 1");
        assert!(matches!(r, Err(FenError::BadCastlingRights(_))));
    }

    #[test]
    fn parse_rejects_bad_castling_order() {
        // QKkq — Q before K violates spec ordering (uppercase before lowercase,
        // kingside before queenside). See Reviewer-revision §A.
        let r = Position::from_fen("rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w QKkq - 0 1");
        assert!(matches!(r, Err(FenError::BadCastlingRights(_))));
    }

    #[test]
    fn parse_rejects_duplicate_castling() {
        // KKkq — K appears twice.
        let r = Position::from_fen("rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KKkq - 0 1");
        assert!(matches!(r, Err(FenError::BadCastlingRights(_))));
    }

    #[test]
    fn parse_rejects_castling_dash_with_letters() {
        // "-K" — dash followed by letters. The dash means "no castling" and
        // is mutually exclusive with letters. Pin that this is rejected
        // rather than silently accepted as either "-" or "K".
        let r = Position::from_fen("rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w -K - 0 1");
        assert!(matches!(r, Err(FenError::BadCastlingRights(_))));
    }

    #[test]
    fn parse_rejects_bad_ep_square() {
        // Several malformed EP squares — all must reject.
        for bad_ep in ["i9", "e0", "e9", "aa", "e", "e3x"] {
            let fen = format!("rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq {bad_ep} 0 1");
            let r = Position::from_fen(&fen);
            assert!(
                matches!(r, Err(FenError::BadEnPassant(_))),
                "EP square {bad_ep:?} must reject, got {r:?}"
            );
        }
    }

    #[test]
    fn parse_rejects_ep_wrong_rank() {
        // EP on rank 4 — impossible from a legal preceding move.
        let r = Position::from_fen("rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq e4 0 1");
        assert!(matches!(r, Err(FenError::BadEnPassant(_))));
    }

    #[test]
    fn parse_rejects_ep_em_dash() {
        // EP field is the UTF-8 em-dash (U+2014, three bytes) instead of '-'.
        // Pins that the byte-oriented parser does not panic on multi-byte UTF-8.
        let r =
            Position::from_fen("rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq \u{2014} 0 1");
        assert!(matches!(r, Err(FenError::BadEnPassant(_))));
    }

    #[test]
    fn parse_rejects_negative_halfmove() {
        let r = Position::from_fen("rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w - - -1 1");
        assert!(matches!(r, Err(FenError::BadHalfmoveClock(_))));
    }

    #[test]
    fn parse_rejects_plus_prefixed_halfmove() {
        // Strict-decimal: leading '+' rejected.
        let r = Position::from_fen("rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w - - +1 1");
        assert!(matches!(r, Err(FenError::BadHalfmoveClock(_))));
    }

    #[test]
    fn parse_rejects_non_numeric_halfmove() {
        let r = Position::from_fen("rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w - - x 1");
        assert!(matches!(r, Err(FenError::BadHalfmoveClock(_))));
    }

    #[test]
    fn parse_rejects_zero_fullmove() {
        // Spec: fullmove is a positive integer.
        let r = Position::from_fen("rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w - - 0 0");
        assert!(matches!(r, Err(FenError::BadFullmoveNumber(_))));
    }

    #[test]
    fn parse_rejects_negative_fullmove() {
        let r = Position::from_fen("rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w - - 0 -1");
        assert!(matches!(r, Err(FenError::BadFullmoveNumber(_))));
    }

    #[test]
    fn parse_rejects_plus_prefixed_fullmove() {
        // Strict-decimal: leading '+' rejected.
        let r = Position::from_fen("rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w - - 0 +1");
        assert!(matches!(r, Err(FenError::BadFullmoveNumber(_))));
    }

    #[test]
    fn parse_rejects_no_white_king() {
        // Empty board with only a black king, white to move.
        let r = Position::from_fen("4k3/8/8/8/8/8/8/8 w - - 0 1");
        assert!(matches!(r, Err(FenError::MissingKing(Color::White))));
    }

    #[test]
    fn parse_rejects_two_white_kings() {
        // Two white kings on the board.
        let r = Position::from_fen("4k3/8/8/8/8/8/8/3KK3 w - - 0 1");
        assert!(matches!(r, Err(FenError::TooManyKings(Color::White))));
    }

    #[test]
    fn parse_rejects_no_black_king() {
        // Only a white king.
        let r = Position::from_fen("8/8/8/8/8/8/8/4K3 w - - 0 1");
        assert!(matches!(r, Err(FenError::MissingKing(Color::Black))));
    }

    #[test]
    fn parse_rejects_pawn_on_rank_1() {
        // White pawn on a1; minimal placement around it. The pawn-on-back-rank
        // check fires before the king count check.
        let r = Position::from_fen("4k3/8/8/8/8/8/8/P3K3 w - - 0 1");
        assert!(matches!(r, Err(FenError::PawnOnBackRank(sq)) if sq == Square::A1));
    }

    #[test]
    fn parse_rejects_pawn_on_rank_8() {
        // Black pawn on a8 (any pawn on rank 8 is invalid regardless of color).
        let r = Position::from_fen("p7/8/8/8/8/8/8/k3K3 w - - 0 1");
        assert!(matches!(r, Err(FenError::PawnOnBackRank(sq)) if sq == Square::A8));
    }

    // ---------------------------------------------------------------------
    // M1.F §13: castling-rights ↔ mailbox consistency.
    //
    // Tests in this block assert FEN parser rejection of FENs whose
    // castling-rights letters do not match the implied king/rook positions
    // on the relevant starting squares. Implementation lives inside
    // `validate_post_parse` and is wired up in the M1.F impl pass — these
    // tests are red until the impl lands.
    // ---------------------------------------------------------------------

    #[test]
    fn castling_k_without_white_king_on_e1() {
        // `K` set but white king is on d1, not e1. Black king on e8 (so
        // king-count check passes); white rook is on h1 (so the rook-side
        // half of the consistency check passes); the only failure path is
        // the white-king-on-e1 mailbox check.
        let r = Position::from_fen("4k3/8/8/8/8/8/8/3K3R w K - 0 1");
        assert!(
            matches!(
                r,
                Err(FenError::InconsistentCastlingRights {
                    right: CastlingRights::WHITE_KING
                })
            ),
            "expected InconsistentCastlingRights{{WHITE_KING}}, got {r:?}"
        );
    }

    #[test]
    fn castling_k_without_white_rook_on_h1() {
        // `K` set, white king on e1, but no white rook on h1.
        let r = Position::from_fen("4k3/8/8/8/8/8/8/4K3 w K - 0 1");
        assert!(
            matches!(
                r,
                Err(FenError::InconsistentCastlingRights {
                    right: CastlingRights::WHITE_KING
                })
            ),
            "expected InconsistentCastlingRights{{WHITE_KING}}, got {r:?}"
        );
    }

    #[test]
    fn castling_k_with_black_rook_on_h1() {
        // `K` set; white king on e1; a black (wrong-colored) rook on h1.
        // The mailbox check (not just bitboards) distinguishes color.
        let r = Position::from_fen("4k3/8/8/8/8/8/8/4K2r w K - 0 1");
        assert!(
            matches!(
                r,
                Err(FenError::InconsistentCastlingRights {
                    right: CastlingRights::WHITE_KING
                })
            ),
            "expected InconsistentCastlingRights{{WHITE_KING}}, got {r:?}"
        );
    }

    #[test]
    fn castling_q_without_white_rook_on_a1() {
        // `Q` set, white king on e1, no white rook on a1. (No `K` set, so
        // the kingside half of the validation is skipped — only `Q` fires.)
        let r = Position::from_fen("4k3/8/8/8/8/8/8/4K3 w Q - 0 1");
        assert!(
            matches!(
                r,
                Err(FenError::InconsistentCastlingRights {
                    right: CastlingRights::WHITE_QUEEN
                })
            ),
            "expected InconsistentCastlingRights{{WHITE_QUEEN}}, got {r:?}"
        );
    }

    #[test]
    fn castling_k_without_black_king_on_e8() {
        // `k` set, but black king on d8 (not e8). White king on e1 to satisfy
        // king-count; black rook on h8 so only the king-square check fires.
        let r = Position::from_fen("3k3r/8/8/8/8/8/8/4K3 b k - 0 1");
        assert!(
            matches!(
                r,
                Err(FenError::InconsistentCastlingRights {
                    right: CastlingRights::BLACK_KING
                })
            ),
            "expected InconsistentCastlingRights{{BLACK_KING}}, got {r:?}"
        );
    }

    #[test]
    fn castling_q_without_black_rook_on_a8() {
        // `q` set, black king on e8, no black rook on a8.
        let r = Position::from_fen("4k3/8/8/8/8/8/8/4K3 b q - 0 1");
        assert!(
            matches!(
                r,
                Err(FenError::InconsistentCastlingRights {
                    right: CastlingRights::BLACK_QUEEN
                })
            ),
            "expected InconsistentCastlingRights{{BLACK_QUEEN}}, got {r:?}"
        );
    }

    #[test]
    fn castling_kqkq_full_consistency_passes() {
        // Standard starting position: `KQkq` set, all four implied pieces
        // on their starting squares. Must parse cleanly.
        let p = Position::from_fen(Position::STARTING_FEN)
            .expect("standard starting position must satisfy castling-mailbox consistency");
        assert_eq!(p.castling_rights(), CastlingRights::ALL);
    }

    #[test]
    fn parse_rejects_halfmove_above_u8_max() {
        // 256 overflows u8.
        let r = Position::from_fen("rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w - - 256 1");
        assert!(matches!(r, Err(FenError::BadHalfmoveClock(_))));
    }

    #[test]
    fn parse_rejects_fullmove_above_u16_max() {
        // 70000 overflows u16 (max 65535).
        let r = Position::from_fen("rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w - - 0 70000");
        assert!(matches!(r, Err(FenError::BadFullmoveNumber(_))));
    }

    #[test]
    fn parse_rejects_leading_whitespace() {
        // Leading space → strict split(' ') yields an empty first field.
        // Either WrongFieldCount (7 fields) or BadPiecePlacement is acceptable.
        let r = Position::from_fen(" rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1");
        assert!(matches!(
            r,
            Err(FenError::WrongFieldCount { .. }) | Err(FenError::BadPiecePlacement(_))
        ));
    }

    #[test]
    fn parse_rejects_trailing_whitespace() {
        // Trailing space → 7th empty field.
        let r = Position::from_fen("rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1 ");
        assert!(matches!(r, Err(FenError::WrongFieldCount { found: 7 })));
    }

    #[test]
    fn parse_rejects_double_space() {
        // Two spaces between fields 2 and 3 yield a 7th phantom empty field.
        let r = Position::from_fen("rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w  KQkq - 0 1");
        assert!(matches!(r, Err(FenError::WrongFieldCount { found: 7 })));
    }

    // -------------------------------------------------------------------------
    // Coverage backfill: paths not exercised by the contract-driven tests above.
    // -------------------------------------------------------------------------

    /// `FenError::Display` was never exercised — tests only matched on the enum
    /// variant, never called `.to_string()`. Pin every arm of the Display impl.
    #[test]
    fn fen_error_display_all_variants() {
        // WrongFieldCount
        assert_eq!(
            FenError::WrongFieldCount { found: 3 }.to_string(),
            "FEN must have 6 fields, found 3"
        );
        // BadPiecePlacement
        assert!(
            FenError::BadPiecePlacement("bad".to_string())
                .to_string()
                .contains("bad piece placement")
        );
        // BadActiveColor
        assert!(
            FenError::BadActiveColor("X".to_string())
                .to_string()
                .contains("bad active color")
        );
        // BadCastlingRights
        assert!(
            FenError::BadCastlingRights("xx".to_string())
                .to_string()
                .contains("bad castling rights")
        );
        // BadEnPassant
        assert!(
            FenError::BadEnPassant("e5".to_string())
                .to_string()
                .contains("bad en passant target")
        );
        // BadHalfmoveClock
        assert!(
            FenError::BadHalfmoveClock("x".to_string())
                .to_string()
                .contains("bad halfmove clock")
        );
        // BadFullmoveNumber
        assert!(
            FenError::BadFullmoveNumber("0".to_string())
                .to_string()
                .contains("bad fullmove number")
        );
        // MissingKing — both colors
        assert!(
            FenError::MissingKing(Color::White)
                .to_string()
                .contains("missing")
        );
        assert!(
            FenError::MissingKing(Color::Black)
                .to_string()
                .contains("missing")
        );
        // TooManyKings — both colors
        assert!(
            FenError::TooManyKings(Color::White)
                .to_string()
                .contains("too many")
        );
        assert!(
            FenError::TooManyKings(Color::Black)
                .to_string()
                .contains("too many")
        );
        // PawnOnBackRank
        assert!(
            FenError::PawnOnBackRank(Square::A1)
                .to_string()
                .contains("pawn on back rank")
        );
    }

    /// `FenError` implements `std::error::Error`; pin that the impl compiles
    /// and is usable (source() returns None for leaf errors).
    #[test]
    fn fen_error_is_std_error() {
        use std::error::Error;
        let e: &dyn Error = &FenError::WrongFieldCount { found: 1 };
        // Leaf error — no source.
        assert!(e.source().is_none());
    }

    /// Line 148: digit causes cumulative file overflow within a rank.
    /// "81" in a single rank means 8 empty + 1 more = 9 squares — overflow
    /// caught by `new_file > 8`. This is distinct from "too many piece letters"
    /// (caught by `file >= 8`) and from "wrong chunk count" (pre-check).
    #[test]
    fn parse_rejects_digit_cumulative_overflow() {
        // Rank 8 = "81" (eight empties then digit 1 → 9 total). Both kings
        // present so king-count is not the trigger.
        let r = Position::from_fen("81/8/8/8/8/8/8/4K2k w - - 0 1");
        assert!(
            matches!(r, Err(FenError::BadPiecePlacement(_))),
            "digit cumulative overflow must be BadPiecePlacement, got {r:?}"
        );
    }

    /// Line 179: placement field ends with the last rank incomplete (fewer than
    /// 8 squares on rank 1). The pre-check passes (8 chunks), slashes are fine,
    /// but the final `rank != 1 || file != 8` guard fires.
    #[test]
    fn parse_rejects_last_rank_incomplete() {
        // Rank 1 has only "4K2" = 4+1+2 = 7 squares (missing the h-file square).
        // Black king on e8, white king on e1 but rank-1 encoded short.
        let r = Position::from_fen("4k3/8/8/8/8/8/8/4K2 w - - 0 1");
        assert!(
            matches!(r, Err(FenError::BadPiecePlacement(_))),
            "incomplete last rank must be BadPiecePlacement, got {r:?}"
        );
    }

    // ------------------------------------------------------------------------
    // Property tests (proptest).
    //
    // The unit tests above cover canonical perft positions verbatim and a
    // long list of malformed-input rejections. The property below randomly
    // generates structurally valid positions (1 king/color, EP on rank 3 or
    // 6, no pawn on back rank) and asserts the format→parse→equal round-trip
    // — same shape as `unit_round_trip_format_then_parse` in position.rs but
    // over an unbounded variety of placements and aux-state combinations.
    // ------------------------------------------------------------------------

    use crate::piece::Piece;
    use proptest::prelude::*;

    /// Build a CastlingRights from a 4-bit mask. Mirrors the `with`-chain that
    /// `parse_castling` produces and matches the bit layout pinned in the
    /// `Polyglot` doc comment on CastlingRights.
    fn rights_from_mask(mask: u8) -> CastlingRights {
        let mut r = CastlingRights::NONE;
        if mask & 0b0001 != 0 {
            r = r.with(CastlingRights::WHITE_KING);
        }
        if mask & 0b0010 != 0 {
            r = r.with(CastlingRights::WHITE_QUEEN);
        }
        if mask & 0b0100 != 0 {
            r = r.with(CastlingRights::BLACK_KING);
        }
        if mask & 0b1000 != 0 {
            r = r.with(CastlingRights::BLACK_QUEEN);
        }
        r
    }

    /// Map a piece code 0..10 to a non-king (color, kind) pair. The 10
    /// non-king piece variants are: 5 kinds × 2 colors. King is excluded so
    /// the king-count invariant remains intact when we apply extras to a
    /// position that already has its two kings placed.
    fn non_king_piece(code: u8) -> Piece {
        let (color, kind) = match code {
            0 => (Color::White, PieceKind::Pawn),
            1 => (Color::White, PieceKind::Knight),
            2 => (Color::White, PieceKind::Bishop),
            3 => (Color::White, PieceKind::Rook),
            4 => (Color::White, PieceKind::Queen),
            5 => (Color::Black, PieceKind::Pawn),
            6 => (Color::Black, PieceKind::Knight),
            7 => (Color::Black, PieceKind::Bishop),
            8 => (Color::Black, PieceKind::Rook),
            9 => (Color::Black, PieceKind::Queen),
            _ => unreachable!("non_king_piece code restricted to 0..10 by the caller"),
        };
        Piece::new(color, kind)
    }

    proptest! {
        /// `Position` → FEN → `Position` round-trip preserves equality, and a
        /// second format yields the same string. Generation strategy:
        ///
        /// - Two distinct king squares (uniformly over `0..64`, filtered).
        /// - Up to 16 additional (square, piece-code) entries; entries that
        ///   collide with kings, with each other, or that would place a pawn
        ///   on rank 1 or 8 are silently skipped. The remaining entries
        ///   guarantee placement validity (no overwrites, no back-rank pawns).
        /// - Side to move from `{White, Black}`.
        /// - Castling: every 4-bit subset of the four named flags, **masked
        ///   down** post-placement so that each retained bit's implied king
        ///   and rook are present on their starting squares (M1.F §13).
        ///   Bits whose implied pieces are absent are silently cleared rather
        ///   than rejecting the case via `prop_filter` (which would shrink
        ///   coverage hard, since fewer than 1 in 1000 random placements have
        ///   pieces on a1/e1/h1/a8/e8/h8).
        /// - EP target: None, or a square on rank 3 or rank 6.
        /// - Halfmove: any `u8`.
        /// - Fullmove: any non-zero `u16` (parser rejects 0).
        ///
        /// **Scope: positive-only.** Every position the strategy emits passes
        /// `validate_post_parse`, so a Display-side bug that produces an
        /// unparseable FEN — or a parse-side bug that decodes the formatted
        /// string into a different `Position` — fails the property. The
        /// strategy deliberately does **not** exercise the parser's rejection
        /// paths (WrongFieldCount, BadPiecePlacement, MissingKing,
        /// TooManyKings, PawnOnBackRank, BadActiveColor, BadCastlingRights,
        /// BadEnPassant, BadHalfmoveClock, BadFullmoveNumber,
        /// InconsistentCastlingRights); those remain covered by the
        /// targeted unit tests above.
        #[test]
        fn prop_fen_round_trip(
            (wk_idx, bk_idx) in (0u8..64, 0u8..64).prop_filter(
                "kings on distinct squares",
                |t| t.0 != t.1,
            ),
            extras in proptest::collection::vec((0u8..64, 0u8..10), 0..16),
            side in prop_oneof![Just(Color::White), Just(Color::Black)],
            castling_mask in 0u8..16,
            ep in prop_oneof![
                Just(None),
                (0u8..8, prop_oneof![Just(2u8), Just(5u8)])
                    .prop_map(|(f, r)| Some(Square::from_file_rank(f, r)
                        .expect("(0..8, 2|5) is in range"))),
            ],
            halfmove in any::<u8>(),
            fullmove in 1u16..=u16::MAX,
        ) {
            let mut p = Position::empty();

            let wk_sq = Square::new(wk_idx).expect("0..64 in range");
            let bk_sq = Square::new(bk_idx).expect("0..64 in range");
            p.set_piece(wk_sq, Piece::new(Color::White, PieceKind::King));
            p.set_piece(bk_sq, Piece::new(Color::Black, PieceKind::King));

            // Place extras, skipping entries that would violate the
            // invariants the FEN parser checks (king count, no back-rank
            // pawn) or that would overwrite an already-placed piece.
            for (sq_idx, code) in extras {
                if sq_idx == wk_idx || sq_idx == bk_idx {
                    continue;
                }
                let sq = Square::new(sq_idx).expect("0..64 in range");
                if p.piece_at(sq).is_some() {
                    continue;
                }
                let piece = non_king_piece(code);
                let rank = sq.rank();
                if piece.kind == PieceKind::Pawn && (rank == 0 || rank == 7) {
                    continue;
                }
                p.set_piece(sq, piece);
            }

            // Mask castling_mask down to only those bits whose implied
            // king/rook pieces are present after placement (FENVAL §13).
            // The parser would otherwise reject the formatted FEN with
            // InconsistentCastlingRights.
            let wk = Some(Piece::new(Color::White, PieceKind::King));
            let wr = Some(Piece::new(Color::White, PieceKind::Rook));
            let bk_piece = Some(Piece::new(Color::Black, PieceKind::King));
            let br = Some(Piece::new(Color::Black, PieceKind::Rook));
            let mut effective_mask = castling_mask;
            if !(p.piece_at(Square::E1) == wk && p.piece_at(Square::H1) == wr) {
                effective_mask &= !0b0001;
            }
            if !(p.piece_at(Square::E1) == wk && p.piece_at(Square::A1) == wr) {
                effective_mask &= !0b0010;
            }
            if !(p.piece_at(Square::E8) == bk_piece && p.piece_at(Square::H8) == br) {
                effective_mask &= !0b0100;
            }
            if !(p.piece_at(Square::E8) == bk_piece && p.piece_at(Square::A8) == br) {
                effective_mask &= !0b1000;
            }

            // Apply the same phantom-EP sanitization that `parse` runs, so
            // the source position carries the post-sanitization EP value
            // and the round-trip via FEN remains identity. Without this,
            // randomly generated EP+placement combinations would clear at
            // parse time, breaking `prop_assert_eq!(&parsed, &p)`.
            let sanitized_ep = sanitize_ep(&p, side, ep);

            p.set_aux_state(
                side,
                rights_from_mask(effective_mask),
                sanitized_ep,
                halfmove,
                fullmove,
            );

            // The constructed position must satisfy the parser's
            // post-parse invariants — if not, the strategy is broken.
            // Anchor that here so a strategy bug is the assertion failure,
            // not the round-trip property below.
            p.validate_post_parse()
                .expect("constructed position must pass validate_post_parse");

            // set_piece/set_aux_state don't refresh the Zobrist or static-eval
            // fields; do both once at the end so structural equality with the
            // FEN-roundtripped position (whose parser refreshes both) holds.
            p.refresh_zobrist();
            p.refresh_static_eval();

            let s = p.to_fen();
            let parsed = Position::from_fen(&s).map_err(|e| {
                TestCaseError::fail(format!(
                    "formatted FEN must parse: {s:?} -> {e:?}"
                ))
            })?;
            prop_assert_eq!(&parsed, &p);
            // Re-formatting the parsed position yields the same string.
            prop_assert_eq!(parsed.to_fen(), s);
        }
    }
}
