#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|s: String| {
    let _ = clawfish::Position::from_fen(&s);
});
