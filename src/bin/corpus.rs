//! `corpus` — M6.G corpus-construction CLI driver.
//!
//! Subcommands: `calibrate-ladder`, `selfplay`, `ingest-pgn`, `build`,
//! `quality-gate`, `export`, `rerun`. R5/R6/R7: streaming, all-cores
//! workers, graceful SIGTERM/SIGINT, crash-safe via the per-game
//! append-block log. Implemented in the M6.G integration slice per
//! `docs/plans/m6.g.md` §2/§5.

use std::process::ExitCode;

fn main() -> ExitCode {
    eprintln!("corpus: M6.G integration slice not yet implemented");
    ExitCode::FAILURE
}
