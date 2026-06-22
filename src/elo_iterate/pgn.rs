//! PGN tag-roster + body emission.
//!
//! Move tokens are UCI long-algebraic (e.g. `e2e4`, `e7e8q`, `e1g1`).
//! This is non-standard PGN (strict consumers expect SAN) but is used for
//! archival inspection by harness-internal tooling. See plan §3.5.

use super::driver::LastInfo;

/// Seven Tag Roster plus harness extensions.
#[derive(Debug)]
pub(crate) struct PgnHeader {
    pub event: String,
    pub site: String,
    pub date: String,
    pub round: u32,
    pub white: String,
    pub black: String,
    /// "1-0", "0-1", or "1/2-1/2".
    pub result: String,
    /// E.g. `"10+0.1"`. `None` → tag omitted.
    pub time_control: Option<String>,
    /// E.g. `"adjudication: insufficient material"`. `None` → tag omitted.
    pub termination: Option<String>,
    /// Non-startpos starting FEN. `None` → `[FEN …]` / `[SetUp "1"]` omitted.
    pub setup_fen: Option<String>,
}

/// A single half-move with its associated info snapshot.
#[derive(Debug)]
pub(crate) struct PgnMove {
    /// UCI move string.
    pub uci: String,
    /// Info from the engine that chose this move. `None` → no `{…}` comment.
    pub last_info: Option<LastInfo>,
}

/// Emit a complete PGN string from a header + move list.
///
/// Format:
/// ```text
/// [Event "..."]
/// ...
/// 1. e2e4 {depth=12 score=cp 35 time=237} e7e5 {...}
/// ...
/// 1-0
/// ```
pub(crate) fn format_pgn(header: &PgnHeader, moves: &[PgnMove]) -> String {
    use super::driver::Score;

    let mut out = String::new();

    // Seven Tag Roster — mandatory, in this exact order.
    out.push_str(&format!("[Event \"{}\"]\n", header.event));
    out.push_str(&format!("[Site \"{}\"]\n", header.site));
    out.push_str(&format!("[Date \"{}\"]\n", header.date));
    out.push_str(&format!("[Round \"{}\"]\n", header.round));
    out.push_str(&format!("[White \"{}\"]\n", header.white));
    out.push_str(&format!("[Black \"{}\"]\n", header.black));
    out.push_str(&format!("[Result \"{}\"]\n", header.result));

    // Optional extension tags.
    if let Some(tc) = &header.time_control {
        out.push_str(&format!("[TimeControl \"{}\"]\n", tc));
    }
    if let Some(term) = &header.termination {
        out.push_str(&format!("[Termination \"{}\"]\n", term));
    }
    if let Some(fen) = &header.setup_fen {
        out.push_str(&format!("[FEN \"{}\"]\n", fen));
        out.push_str("[SetUp \"1\"]\n");
    }

    // Blank line separating header from body.
    out.push('\n');

    // Move body: emit move pairs with numbers and optional comments.
    let format_comment = |info: &Option<LastInfo>| -> String {
        let Some(li) = info else { return String::new() };
        let (Some(depth), Some(score), Some(time_ms)) = (li.depth, li.score.as_ref(), li.time_ms)
        else {
            return String::new();
        };
        let score_str = match score {
            Score::Cp(n) => format!("score=cp {n}"),
            Score::Mate(n) => format!("score=mate {n}"),
        };
        format!(" {{depth={depth} {score_str} time={time_ms}}}")
    };

    let mut i = 0;
    while i < moves.len() {
        let move_number = i / 2 + 1;
        let white_move = &moves[i];
        let white_comment = format_comment(&white_move.last_info);

        if i + 1 < moves.len() {
            let black_move = &moves[i + 1];
            let black_comment = format_comment(&black_move.last_info);
            out.push_str(&format!(
                "{}. {}{} {}{}",
                move_number, white_move.uci, white_comment, black_move.uci, black_comment,
            ));
            i += 2;
            if i < moves.len() {
                out.push(' ');
            }
        } else {
            // Odd-length list: trailing white move with no black reply.
            out.push_str(&format!(
                "{}. {}{}",
                move_number, white_move.uci, white_comment,
            ));
            i += 1;
        }
    }

    // Result marker at end of body.
    if !moves.is_empty() {
        out.push(' ');
    }
    out.push_str(&header.result);
    out.push('\n');

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::elo_iterate::driver::{LastInfo, Score};

    fn base_header(result: &str) -> PgnHeader {
        PgnHeader {
            event: "Test event".into(),
            site: "localhost".into(),
            date: "2026.04.29".into(),
            round: 1,
            white: "clawfish".into(),
            black: "opponent".into(),
            result: result.into(),
            time_control: Some("10+0.1".into()),
            termination: None,
            setup_fen: None,
        }
    }

    fn info_with(depth: u32, score_cp: i32, time_ms: u64) -> Option<LastInfo> {
        Some(LastInfo {
            depth: Some(depth),
            score: Some(Score::Cp(score_cp)),
            time_ms: Some(time_ms),
        })
    }

    #[test]
    fn pgn_white_wins_startpos_formats_to_seven_tag_roster_plus_comments() {
        let header = base_header("1-0");
        let moves = vec![
            PgnMove {
                uci: "e2e4".into(),
                last_info: info_with(12, 35, 237),
            },
            PgnMove {
                uci: "e7e5".into(),
                last_info: info_with(11, -32, 205),
            },
            PgnMove {
                uci: "d1h5".into(),
                last_info: info_with(10, 200, 180),
            },
            PgnMove {
                uci: "e8e7".into(),
                last_info: info_with(9, -500, 160),
            },
        ];
        let pgn = format_pgn(&header, &moves);

        // Mandatory tags: properly quoted with literal value.
        assert!(
            pgn.contains(r#"[Event "Test event"]"#),
            "Event tag missing/malformed; got:\n{pgn}"
        );
        assert!(
            pgn.contains(r#"[Site "localhost"]"#),
            "Site tag missing/malformed"
        );
        assert!(
            pgn.contains(r#"[Date "2026.04.29"]"#),
            "Date tag missing/malformed"
        );
        assert!(
            pgn.contains(r#"[Round "1"]"#),
            "Round tag missing/malformed"
        );
        assert!(
            pgn.contains(r#"[White "clawfish"]"#),
            "White tag missing/malformed"
        );
        assert!(
            pgn.contains(r#"[Black "opponent"]"#),
            "Black tag missing/malformed"
        );
        assert!(
            pgn.contains(r#"[Result "1-0"]"#),
            "Result tag missing/malformed"
        );
        assert!(
            pgn.contains(r#"[TimeControl "10+0.1"]"#),
            "TimeControl tag missing/malformed"
        );

        // Move ordering: e2e4 must precede e7e5 must precede d1h5 must precede e8e7.
        let p_e2e4 = pgn.find("e2e4").expect("missing e2e4");
        let p_e7e5 = pgn.find("e7e5").expect("missing e7e5");
        let p_d1h5 = pgn.find("d1h5").expect("missing d1h5");
        let p_e8e7 = pgn.find("e8e7").expect("missing e8e7");
        assert!(
            p_e2e4 < p_e7e5 && p_e7e5 < p_d1h5 && p_d1h5 < p_e8e7,
            "move order corrupted; positions e2e4={p_e2e4} e7e5={p_e7e5} d1h5={p_d1h5} e8e7={p_e8e7}"
        );

        // Move-numbered prefixes for the white moves.
        assert!(
            pgn.contains("1. e2e4") || pgn.contains("1.e2e4"),
            "move number prefix '1.' missing for first move"
        );
        assert!(
            pgn.contains("2. d1h5") || pgn.contains("2.d1h5"),
            "move number prefix '2.' missing for second white move"
        );

        // Per-move comment is the FULL `{depth=N score=cp X time=T}` block,
        // not just an isolated `depth=N`.  Pin the exact comment shape on
        // the first move (depth 12, score cp 35, time 237).
        let comment_re_e2e4 = "{depth=12 score=cp 35 time=237}";
        assert!(
            pgn.contains(comment_re_e2e4),
            "expected exact comment {comment_re_e2e4:?} on e2e4; got:\n{pgn}"
        );

        // The comment must be attached to the right move (appears after
        // e2e4 and before e7e5 in document order).
        let p_comment = pgn.find(comment_re_e2e4).expect("comment present?");
        assert!(
            p_e2e4 < p_comment && p_comment < p_e7e5,
            "e2e4 comment is not attached to e2e4"
        );

        // Negative-score format (cp -32 on e7e5) — confirm minus sign rendered correctly.
        assert!(
            pgn.contains("score=cp -32"),
            "expected score=cp -32 in body; got:\n{pgn}"
        );

        // Result marker at the end (ignore optional trailing whitespace).
        assert!(
            pgn.trim_end().ends_with("1-0"),
            "PGN body must end with the result marker '1-0'; trimmed end: {:?}",
            &pgn[pgn.len().saturating_sub(20)..]
        );
    }

    #[test]
    fn pgn_black_wins_with_termination_tag() {
        let mut header = base_header("0-1");
        header.termination = Some("adjudication: insufficient material".into());
        let pgn = format_pgn(&header, &[]);
        assert!(pgn.contains("[Termination "), "missing Termination tag");
        assert!(
            pgn.contains("insufficient material"),
            "wrong termination value"
        );
        assert!(pgn.contains("0-1"), "missing result marker");
    }

    #[test]
    fn pgn_setup_tag_omitted_for_startpos() {
        let header = base_header("1/2-1/2");
        let pgn = format_pgn(&header, &[]);
        assert!(
            !pgn.contains("[FEN "),
            "FEN tag should be absent for startpos"
        );
        assert!(
            !pgn.contains("[SetUp "),
            "SetUp tag should be absent for startpos"
        );
    }

    #[test]
    fn pgn_move_comment_omitted_when_lastinfo_none() {
        let header = base_header("1-0");
        let moves = vec![
            PgnMove {
                uci: "e2e4".into(),
                last_info: None,
            },
            PgnMove {
                uci: "e7e5".into(),
                last_info: None,
            },
        ];
        let pgn = format_pgn(&header, &moves);
        assert!(
            !pgn.contains('{'),
            "no move comment expected when last_info is None"
        );
    }

    #[test]
    fn pgn_odd_move_count_emits_trailing_white_move_no_black() {
        // Pins the move-pair iteration boundary: odd-length move list
        // emits the trailing white move WITHOUT a black follow-up.
        // Catches `replace < with > in format_pgn`, `replace < with <= in
        // format_pgn`, `replace < with == in format_pgn`, and
        // `replace += with -=` / `*=` mutations on the move-index step.
        let header = base_header("1-0");
        let moves = vec![
            PgnMove {
                uci: "e2e4".into(),
                last_info: None,
            },
            PgnMove {
                uci: "e7e5".into(),
                last_info: None,
            },
            PgnMove {
                uci: "g1f3".into(),
                last_info: None,
            },
        ];
        let pgn = format_pgn(&header, &moves);
        // Move 1 has both white and black; move 2 has only white.
        assert!(pgn.contains("1. e2e4 e7e5"), "missing 1. e2e4 e7e5: {pgn}");
        assert!(pgn.contains("2. g1f3"), "missing 2. g1f3: {pgn}");
        // The third move's UCI must NOT be followed by a non-numbered token
        // (i.e., it stands alone without a black reply).
        let g1f3_pos = pgn.find("2. g1f3").expect("missing 2. g1f3");
        let after = &pgn[g1f3_pos + "2. g1f3".len()..];
        // After "2. g1f3", the only valid content is the result marker
        // (preceded by a single space) and a trailing newline.
        assert!(
            after.trim_end() == " 1-0",
            "expected only ' 1-0' after '2. g1f3'; got: {after:?}"
        );
    }

    #[test]
    fn pgn_single_move_emits_one_white_only() {
        // Boundary: 1-move list should emit "1. <move> <result>".
        // Catches the `i + 1 < moves.len()` predicate boundary at the
        // first iteration when moves.len() == 1.
        let header = base_header("1-0");
        let moves = vec![PgnMove {
            uci: "e2e4".into(),
            last_info: None,
        }];
        let pgn = format_pgn(&header, &moves);
        assert!(pgn.contains("1. e2e4"), "missing '1. e2e4': {pgn}");
        assert!(!pgn.contains("e7e5"), "should not contain e7e5: {pgn}");
        assert!(pgn.trim_end().ends_with("1-0"), "missing result: {pgn}");
    }

    #[test]
    fn pgn_empty_moves_omits_body_but_keeps_result() {
        // Edge: empty move list — header + result only.
        let header = base_header("1/2-1/2");
        let pgn = format_pgn(&header, &[]);
        assert!(pgn.contains(r#"[Result "1/2-1/2"]"#));
        assert!(pgn.trim_end().ends_with("1/2-1/2"));
        // No move tokens, no comments.
        assert!(!pgn.contains('{'), "no comments for empty body");
    }

    // ---- ELOH.D §6.6: pgn_time_control_tag_reflects_sampled_tc ----

    #[test]
    fn pgn_time_control_tag_reflects_sampled_tc() {
        // Construct PgnHeader { time_control: Some("20+0.2"), .. }; format;
        // assert the produced PGN contains exactly one [TimeControl "20+0.2"] line.
        let header = PgnHeader {
            event: "test".into(),
            site: "localhost".into(),
            date: "2026.04.30".into(),
            round: 1,
            white: "clawfish".into(),
            black: "opponent".into(),
            result: "1/2-1/2".into(),
            time_control: Some("20+0.2".into()),
            termination: None,
            setup_fen: None,
        };
        let pgn = format_pgn(&header, &[]);
        let tc_tag_count = pgn
            .lines()
            .filter(|l| *l == r#"[TimeControl "20+0.2"]"#)
            .count();
        assert_eq!(
            tc_tag_count, 1,
            "must contain exactly one [TimeControl \"20+0.2\"] line; got:\n{pgn}"
        );
    }

    /// Boundary sweep across move counts {0, 1, 2, 3, 4, 5}.
    ///
    /// Pins the move-pair separator at line `if i < moves.len()` after
    /// `i += 2` (kills `< → <=` and `< → ==` mutants — both produce a
    /// trailing space after the last pair) and the result-marker spacing
    /// at `if !moves.is_empty()` (kills the `delete !` mutant — produces
    /// a leading space at n=0 and skips the separator at n≥1).
    ///
    /// Existing pgn tests use `pgn.contains(...)` and
    /// `pgn.trim_end().ends_with(...)` which silently absorb internal
    /// whitespace mutations.  This sweep asserts the body shape exactly.
    #[test]
    fn format_pgn_pins_separator_and_result_spacing() {
        let header = base_header("1-0");
        for n in [0_usize, 1, 2, 3, 4, 5] {
            let moves: Vec<PgnMove> = (0..n)
                .map(|i| PgnMove {
                    uci: format!("a{}a{}", (i % 8) + 1, ((i + 1) % 8) + 1),
                    last_info: None,
                })
                .collect();
            let pgn = format_pgn(&header, &moves);

            // Body is everything after the header's blank-line separator.
            // `split_once("\n\n")` is correct because `base_header` produces
            // header tags that contain no newlines, so the only `"\n\n"` is
            // the mandatory header/body blank line.
            let body = pgn
                .split_once("\n\n")
                .map(|(_, b)| b)
                .unwrap_or_else(|| panic!("n={n}: missing header/body separator in:\n{pgn}"));

            if n == 0 {
                // Empty body: result marker on its own line, no leading space.
                // Catches `delete !` at line 5390 (would produce " 1-0\n").
                assert_eq!(body, "1-0\n", "n=0 body must be '1-0\\n', got {body:?}");
                continue;
            }

            // n ≥ 1: body must end with " 1-0\n" — exactly one space before
            // the result marker.  Catches `delete !` at line 5390 in the
            // non-empty case (would skip the separator → no space).
            assert!(
                body.ends_with(" 1-0\n"),
                "n={n} body must end with ' 1-0\\n', got {body:?}"
            );

            // Body before the trailing " 1-0\n" must end with a
            // non-space character (the last move's UCI).  Catches the
            // L5376 mutants `< → <=` and `< → ==` (both push a spurious
            // space after the last move pair, manifesting as `"  "`
            // immediately before the " 1-0\n" result marker).
            let body_moves = &body[..body.len() - " 1-0\n".len()];
            assert!(
                !body_moves.is_empty() && body_moves.as_bytes().last().is_none_or(|b| *b != b' '),
                "n={n} body before result marker must end with a non-space character, got {body_moves:?}"
            );

            // Move-pair separator: each move number prefix `K. ` for K≥2
            // must be preceded by a single space.  `n.div_ceil(2)`: for
            // even n, equals n/2 (covers pairs 1..=n/2); for odd n,
            // equals (n+1)/2 (includes the trailing white-only pair);
            // for n=1, range `2..=1` is empty (no separators expected).
            for k in 2..=n.div_ceil(2) {
                let sep = format!(" {k}. ");
                assert!(
                    body_moves.contains(&sep),
                    "n={n} expected move-pair separator {sep:?}, got {body_moves:?}"
                );
            }
        }
    }
}
