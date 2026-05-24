#![deny(missing_docs)]
//! clawfish — a Rust chess engine, written from scratch and grown
//! incrementally toward GM-level standard-chess strength.
//!
//! See `docs/architecture.md` and `docs/roadmap.md` for the project's
//! current architectural state and milestone plan.

pub mod bench;
pub mod bitboard;
/// M6.G — reusable game-result-labeled quiet-position corpus (data infra).
pub mod corpus;
/// Tournament-harness binary glue (`elo-iterate`). See `docs/decisions/0020-eloh-harness.md`.
pub mod elo_iterate;
pub mod engine;
pub mod eval;
pub mod fen;
pub mod history;
pub mod magic;
pub mod match_clock;
pub mod mov;
pub mod movegen;
pub mod perft;
pub mod piece;
pub mod position;
pub mod search;
pub mod slow_attacks;
pub mod square;
/// M6.I — Texel tuning harness (deferred eval-weight calibration). See
/// `docs/decisions/0037-texel-tuning-harness.md`.
pub mod texel;
pub mod tt;
pub mod uci;
pub mod zobrist;

pub use bitboard::Bitboard;
pub use engine::{reader_loop, run_stdio};
pub use eval::evaluate;
pub use fen::FenError;
pub use magic::{bishop_attacks, queen_attacks, rook_attacks};
pub use match_clock::{MatchTimeMode, PerSideClock};
pub use mov::{Move, MoveFlag, UciMoveError, Undo};
pub use movegen::{MoveList, generate_moves, in_check};
pub use perft::{PerftCounts, divide, perft, perft_bulk, perft_categorized};
pub use piece::{Color, Piece, PieceKind};
pub use position::Position;
pub use search::{Search, SearchContext, SearchLimits, SearchResult};
pub use square::Square;
pub use uci::{Command, DebugMode, GoParams, PositionSpec, Register, parse_uci_line};
