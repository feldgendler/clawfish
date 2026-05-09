//! Thin binary entry point for the `elo-iterate` tournament harness.
//!
//! All logic lives in `clawfish::elo_iterate`. This file exists only so
//! Cargo has a `[[bin]]` target to compile.

fn main() -> std::process::ExitCode {
    clawfish::elo_iterate::main()
}
