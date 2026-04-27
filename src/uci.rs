//! UCI command parser.
//!
//! Pure-function string→AST parser for the GUI→engine command set per the
//! UCI 2006 spec (`docs/reference/uci-protocol-2006.txt`). No I/O, no
//! engine state, no `Result` — every input string maps to a [`Command`]
//! (use [`Command::Unknown`] for unrecognized or malformed input).
//!
//! Per-command leniency rules and design rationale: see
//! `docs/plans/m2.b.md`. Headline rules:
//!
//! - **Strict-first-token.** First non-whitespace token must be a
//!   recognized command keyword (`uci`, `debug`, …). No leading-skip
//!   (deviates from the spec's literal §preamble suggestion; matches
//!   Stockfish 18 empirically).
//! - **Per-command body rules** — `debug` is strict-exact-2-tokens;
//!   `position` is lenient-stop after the position spec; `go` is
//!   lenient-skip on unknown body tokens.
//! - **Move strings inside `position … moves …` and `go searchmoves …`
//!   are collected raw**, not parsed — that's M2.C's job via
//!   [`Move::from_uci`].
//! - **FEN strings inside `position fen …` are collected raw** (exactly
//!   6 tokens, joined with single spaces) — M2.C parses them.

// `parse_uci_line` is the sole public function; helper sub-parsers are
// private (tested via the public surface).

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// One parsed UCI command line. Total over input strings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    /// `uci`
    Uci,
    /// `debug on` / `debug off`
    Debug(DebugMode),
    /// `isready`
    IsReady,
    /// `setoption name <id> [value <x>]`. `name` is non-empty by parser
    /// contract; `value` is `Some(_)` iff the `value` keyword was given
    /// (and may be `Some(String::new())` if `value` had an empty body).
    SetOption { name: String, value: Option<String> },
    /// `register …`
    Register(Register),
    /// `ucinewgame`
    UciNewGame,
    /// `position [startpos|fen <…>] [moves <m1> <m2> …]`. `moves` are
    /// collected as raw `String`s; M2.C parses via [`Move::from_uci`].
    Position { spec: PositionSpec, moves: Vec<String> },
    /// `go [params …]`
    Go(GoParams),
    /// `stop`
    Stop,
    /// `ponderhit`
    PonderHit,
    /// `quit`
    Quit,
    /// Empty line, unrecognized command, or required-arg parse failure.
    /// Carries no diagnostic — the original line is M2.C's responsibility.
    Unknown,
}

/// `debug on` / `debug off` argument.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DebugMode {
    On,
    Off,
}

/// `register …` body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Register {
    /// `register later`
    Later,
    /// `register [name <name>] [code <code>]`. At least one field is
    /// `Some` by parser contract — `parse_uci_line` collapses the
    /// both-`None` case to [`Command::Unknown`]. Constructing
    /// `Identify { name: None, code: None }` directly is API misuse.
    Identify { name: Option<String>, code: Option<String> },
}

/// `position` first-arg.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PositionSpec {
    /// `position startpos …`
    StartPos,
    /// `position fen <field1> … <field6> …` — exactly the 6 FEN tokens
    /// collected after `fen`, joined with single spaces. Validation
    /// (correct field *content*) is deferred to M2.C.
    Fen(String),
}

/// `go [params …]` body.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GoParams {
    /// `searchmoves <m1> …`. `None` means the keyword was not given;
    /// `Some(vec![])` means the keyword was given with no moves following.
    pub searchmoves: Option<Vec<String>>,
    /// `ponder` keyword present
    pub ponder: bool,
    /// `wtime <x>` — milliseconds remaining for white. Signed because the
    /// UCI clock can go negative under time-trouble overshoot.
    pub wtime: Option<i64>,
    /// `btime <x>` — milliseconds remaining for black.
    pub btime: Option<i64>,
    /// `winc <x>` — white increment per move (ms). Spec: "if x > 0", so
    /// non-negative; negative inputs fail to parse and yield `None`.
    pub winc: Option<u64>,
    /// `binc <x>` — black increment per move (ms).
    pub binc: Option<u64>,
    pub movestogo: Option<u32>,
    pub depth: Option<u32>,
    pub nodes: Option<u64>,
    /// `mate <x>` — search for mate in x moves. `Some(0)` is a degenerate
    /// "is the current position checkmate?" query handled by consumers.
    pub mate: Option<u32>,
    /// `movetime <x>` — exact milliseconds; signed for symmetry with wtime/btime.
    pub movetime: Option<i64>,
    /// `infinite` keyword present
    pub infinite: bool,
}

// ---------------------------------------------------------------------------
// Private constants
// ---------------------------------------------------------------------------

/// Termination set for `searchmoves`'s collection loop in `parse_go`.
/// The 11 `go` body keywords other than `searchmoves` itself. Repeating
/// `searchmoves` is intentionally NOT in the set — the second occurrence
/// is appended to the current move list as a literal token (which M2.C's
/// `from_uci` rejects). See `docs/plans/m2.b.md` §6.c.
const SEARCHMOVES_TERMINATORS: &[&str] = &[
    "ponder",
    "infinite",
    "wtime",
    "btime",
    "winc",
    "binc",
    "movestogo",
    "depth",
    "nodes",
    "mate",
    "movetime",
];

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Parse one line of UCI input into a [`Command`].
///
/// Total function: every input string yields a `Command`. Use
/// [`Command::Unknown`] for unrecognized or malformed input. See module
/// docs for the spec-conformance and grammar details.
pub fn parse_uci_line(line: &str) -> Command {
    let tokens: Vec<&str> = line.split_whitespace().collect();
    let Some((&first, rest)) = tokens.split_first() else {
        return Command::Unknown;
    };
    match first {
        // No-arg commands — trailing tokens silently ignored (§3.2).
        "uci" => Command::Uci,
        "isready" => Command::IsReady,
        "ucinewgame" => Command::UciNewGame,
        "stop" => Command::Stop,
        "ponderhit" => Command::PonderHit,
        "quit" => Command::Quit,
        // Commands with body grammars.
        "debug" => parse_debug(rest),
        "setoption" => parse_setoption(rest),
        "register" => parse_register(rest),
        "position" => parse_position(rest),
        "go" => Command::Go(parse_go(rest)),
        // Strict-first-token: first token must be a known keyword (§3.3).
        _ => Command::Unknown,
    }
}

// ---------------------------------------------------------------------------
// Per-command sub-parsers (private — tested through parse_uci_line)
// ---------------------------------------------------------------------------

/// `debug on` / `debug off`. Strict-exact-2-tokens (§3.2): the rest must
/// contain exactly one token, and that token must be `on` or `off`.
fn parse_debug(rest: &[&str]) -> Command {
    if rest.len() != 1 {
        return Command::Unknown;
    }
    match rest[0] {
        "on" => Command::Debug(DebugMode::On),
        "off" => Command::Debug(DebugMode::Off),
        _ => Command::Unknown,
    }
}

/// `setoption name <id> [value <x>]`. See §8.a.
fn parse_setoption(rest: &[&str]) -> Command {
    let Some((&first, body)) = rest.split_first() else {
        return Command::Unknown;
    };
    if first != "name" {
        return Command::Unknown;
    }
    let mut name_tokens: Vec<&str> = Vec::new();
    let mut value_tokens: Vec<&str> = Vec::new();
    let mut value_keyword_seen = false;
    for &token in body {
        if !value_keyword_seen && token == "value" {
            value_keyword_seen = true;
        } else if value_keyword_seen {
            value_tokens.push(token);
        } else {
            name_tokens.push(token);
        }
    }
    if name_tokens.is_empty() {
        return Command::Unknown;
    }
    let name = name_tokens.join(" ");
    let value = if value_keyword_seen {
        Some(value_tokens.join(" "))
    } else {
        None
    };
    Command::SetOption { name, value }
}

/// `register [later | name <x> | code <y>]`. See §8.b.
fn parse_register(rest: &[&str]) -> Command {
    let Some((&first, _)) = rest.split_first() else {
        return Command::Unknown;
    };
    // `later` is a single-form variant; trailing tokens silently ignored.
    if first == "later" {
        return Command::Register(Register::Later);
    }
    // The Identify variant requires `name` or `code` as the immediate next
    // token; anything else is Unknown per §3.2.
    if first != "name" && first != "code" {
        return Command::Unknown;
    }

    // Walk all tokens. Each `name` / `code` keyword starts a fresh
    // accumulator for that field; non-keyword tokens append to the
    // current accumulator. Empty body → None (overwriting any prior
    // Some per pure last-wins, §8.b).
    let mut name: Option<String> = None;
    let mut code: Option<String> = None;
    let mut current_field: Option<&str> = None;
    let mut current_acc: Vec<&str> = Vec::new();

    for &token in rest {
        if token == "name" || token == "code" {
            // Flush previous field's accumulator.
            flush_register_field(current_field, &current_acc, &mut name, &mut code);
            current_field = Some(token);
            current_acc.clear();
        } else if current_field.is_some() {
            current_acc.push(token);
        }
        // else: pre-keyword garbage — first-token check above guarantees
        // this branch is unreachable in practice.
    }
    flush_register_field(current_field, &current_acc, &mut name, &mut code);

    if name.is_none() && code.is_none() {
        return Command::Unknown;
    }
    // Defense-in-depth: the `if` above is the actual guard. This
    // assertion documents the post-condition for readers and catches
    // the regression where someone deletes the `if` but forgets to
    // restructure the both-None handling. Per §8.b.
    debug_assert!(
        name.is_some() || code.is_some(),
        "Register::Identify invariant: at least one field must be Some",
    );
    Command::Register(Register::Identify { name, code })
}

/// Helper for [`parse_register`]: flush the current accumulator into the
/// named field. Empty body sets the field to `None` (overwriting any
/// prior `Some` per §8.b last-wins-meets-empty-body).
fn flush_register_field(
    field: Option<&str>,
    acc: &[&str],
    name: &mut Option<String>,
    code: &mut Option<String>,
) {
    let value = if acc.is_empty() {
        None
    } else {
        Some(acc.join(" "))
    };
    match field {
        None => {} // first flush before any keyword consumed — legitimate no-op
        Some("name") => *name = value,
        Some("code") => *code = value,
        Some(other) => unreachable!(
            "flush_register_field invariant: current_field is set only \
             to \"name\" or \"code\" by parse_register, got {other:?}",
        ),
    }
}

/// `position [startpos|fen <6-tokens>] [moves <…>]`. See §7.
fn parse_position(rest: &[&str]) -> Command {
    let Some((&first, after_first)) = rest.split_first() else {
        return Command::Unknown;
    };
    let (spec, remaining) = match first {
        "startpos" => (PositionSpec::StartPos, after_first),
        "fen" => {
            // Strict-6: collect exactly 6 tokens. Fewer → Unknown.
            if after_first.len() < 6 {
                return Command::Unknown;
            }
            let fen = after_first[..6].join(" ");
            (PositionSpec::Fen(fen), &after_first[6..])
        }
        _ => return Command::Unknown,
    };

    // Lenient-stop trailing clause: peek for `moves` keyword. If present,
    // consume + collect rest as raw move strings. Anything else (or
    // end-of-line): drop the rest, leave moves empty.
    let moves = match remaining.first() {
        Some(&"moves") => remaining[1..].iter().map(|s| s.to_string()).collect(),
        _ => Vec::new(),
    };

    Command::Position { spec, moves }
}

/// `go [params …]`. Lenient-skip body (§6).
fn parse_go(rest: &[&str]) -> GoParams {
    let mut params = GoParams::default();
    let mut iter = rest.iter().copied().peekable();
    while let Some(token) = iter.next() {
        match token {
            "ponder" => params.ponder = true,
            "infinite" => params.infinite = true,
            "wtime" => consume_numeric(&mut iter, &mut params.wtime),
            "btime" => consume_numeric(&mut iter, &mut params.btime),
            "winc" => consume_numeric(&mut iter, &mut params.winc),
            "binc" => consume_numeric(&mut iter, &mut params.binc),
            "movestogo" => consume_numeric(&mut iter, &mut params.movestogo),
            "depth" => consume_numeric(&mut iter, &mut params.depth),
            "nodes" => consume_numeric(&mut iter, &mut params.nodes),
            "mate" => consume_numeric(&mut iter, &mut params.mate),
            "movetime" => consume_numeric(&mut iter, &mut params.movetime),
            "searchmoves" => {
                // Collect raw move strings until next non-`searchmoves`
                // recognized go keyword (§6.c) or end of line. Last-wins
                // for repeated `searchmoves` keyword in the outer loop.
                let mut moves = Vec::new();
                while let Some(&peek) = iter.peek() {
                    if SEARCHMOVES_TERMINATORS.contains(&peek) {
                        break;
                    }
                    // Safe: peek returned Some, so next won't be None.
                    moves.push(iter.next().unwrap().to_string());
                }
                params.searchmoves = Some(moves);
            }
            // Unknown go-body token — silently skip per §6.a / §3.2.
            _ => {}
        }
    }
    params
}

/// Helper for [`parse_go`]: consume the next token from `iter`
/// unconditionally if one exists, then attempt to parse it into `T`.
/// Parse failure (or no token to consume) leaves `target` as-is; the
/// consumed token is NOT re-scanned as a possible keyword (§6.b).
fn consume_numeric<'a, T, I>(iter: &mut I, target: &mut Option<T>)
where
    T: std::str::FromStr,
    I: Iterator<Item = &'a str>,
{
    if let Some(value) = iter.next()
        && let Ok(parsed) = value.parse::<T>()
    {
        *target = Some(parsed);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    // -----------------------------------------------------------------------
    // Group A — anchor unit tests, recognized commands.
    //
    // One test per command shape per `docs/plans/m2.b.md` §A. 48 tests.
    // -----------------------------------------------------------------------

    // ─── No-arg commands ──────────────────────────────────────────────────

    #[test]
    fn parses_uci() {
        assert_eq!(parse_uci_line("uci"), Command::Uci);
    }

    #[test]
    fn parses_isready() {
        assert_eq!(parse_uci_line("isready"), Command::IsReady);
    }

    #[test]
    fn parses_ucinewgame() {
        assert_eq!(parse_uci_line("ucinewgame"), Command::UciNewGame);
    }

    #[test]
    fn parses_stop() {
        assert_eq!(parse_uci_line("stop"), Command::Stop);
    }

    #[test]
    fn parses_ponderhit() {
        assert_eq!(parse_uci_line("ponderhit"), Command::PonderHit);
    }

    #[test]
    fn parses_quit() {
        assert_eq!(parse_uci_line("quit"), Command::Quit);
    }

    // ─── debug ────────────────────────────────────────────────────────────

    #[test]
    fn parses_debug_on() {
        assert_eq!(parse_uci_line("debug on"), Command::Debug(DebugMode::On));
    }

    #[test]
    fn parses_debug_off() {
        assert_eq!(parse_uci_line("debug off"), Command::Debug(DebugMode::Off));
    }

    // ─── setoption ────────────────────────────────────────────────────────

    #[test]
    fn parses_setoption_name_only() {
        assert_eq!(
            parse_uci_line("setoption name Clear Hash"),
            Command::SetOption {
                name: "Clear Hash".to_string(),
                value: None,
            },
        );
    }

    #[test]
    fn parses_setoption_name_and_value() {
        assert_eq!(
            parse_uci_line("setoption name Hash value 32"),
            Command::SetOption {
                name: "Hash".to_string(),
                value: Some("32".to_string()),
            },
        );
    }

    #[test]
    fn parses_setoption_value_with_spaces() {
        // Spec example: setoption name UCI_Opponent value GM 2800 human Gary Kasparov
        assert_eq!(
            parse_uci_line(
                "setoption name UCI_Opponent value GM 2800 human Gary Kasparov"
            ),
            Command::SetOption {
                name: "UCI_Opponent".to_string(),
                value: Some("GM 2800 human Gary Kasparov".to_string()),
            },
        );
    }

    #[test]
    fn parses_setoption_empty_value() {
        // `value` keyword present, body empty → `Some(String::new())`.
        // Pins §8.a empty-value-is-Some("") rule (preserves the
        // distinction from "no value keyword").
        assert_eq!(
            parse_uci_line("setoption name Foo value"),
            Command::SetOption {
                name: "Foo".to_string(),
                value: Some(String::new()),
            },
        );
    }

    #[test]
    fn parses_setoption_value_path_with_semicolons() {
        // Spec example. Rust source has escaped backslashes; the literal
        // value is c:\chess\tb\4;c:\chess\tb\5, matching the spec text.
        // No round-trip / re-serialization is asserted — that's M2.C/D's
        // concern.
        assert_eq!(
            parse_uci_line(
                "setoption name NalimovPath value c:\\chess\\tb\\4;c:\\chess\\tb\\5"
            ),
            Command::SetOption {
                name: "NalimovPath".to_string(),
                value: Some("c:\\chess\\tb\\4;c:\\chess\\tb\\5".to_string()),
            },
        );
    }

    // ─── register ─────────────────────────────────────────────────────────

    #[test]
    fn parses_register_later() {
        assert_eq!(parse_uci_line("register later"), Command::Register(Register::Later));
    }

    #[test]
    fn parses_register_name_only() {
        assert_eq!(
            parse_uci_line("register name Stefan"),
            Command::Register(Register::Identify {
                name: Some("Stefan".to_string()),
                code: None,
            }),
        );
    }

    #[test]
    fn parses_register_code_only() {
        assert_eq!(
            parse_uci_line("register code 4359874324"),
            Command::Register(Register::Identify {
                name: None,
                code: Some("4359874324".to_string()),
            }),
        );
    }

    #[test]
    fn parses_register_name_and_code() {
        // Spec example: register name Stefan MK code 4359874324
        assert_eq!(
            parse_uci_line("register name Stefan MK code 4359874324"),
            Command::Register(Register::Identify {
                name: Some("Stefan MK".to_string()),
                code: Some("4359874324".to_string()),
            }),
        );
    }

    #[test]
    fn parses_register_name_with_empty_code_keyword() {
        // `code` keyword IS present; its body is empty → code: None.
        // Pins §8.b empty-body-is-None rule (NOT Some("")).
        assert_eq!(
            parse_uci_line("register name Stefan code"),
            Command::Register(Register::Identify {
                name: Some("Stefan".to_string()),
                code: None,
            }),
        );
    }

    #[test]
    fn parses_register_later_with_trailing_junk() {
        // `later` consumes the rest of the line implicitly per §8.b.
        assert_eq!(
            parse_uci_line("register later garbage"),
            Command::Register(Register::Later),
        );
    }

    #[test]
    fn parses_register_last_wins_with_empty_repeat() {
        // Last-wins meets empty-body: second `name` body is empty,
        // overwrites prior Some("Stefan") with None. `code` unaffected.
        // Pins §8.b last-wins-meets-empty-body rule.
        assert_eq!(
            parse_uci_line("register name Stefan code 123 name"),
            Command::Register(Register::Identify {
                name: None,
                code: Some("123".to_string()),
            }),
        );
    }

    // ─── position ─────────────────────────────────────────────────────────

    #[test]
    fn parses_position_startpos() {
        assert_eq!(
            parse_uci_line("position startpos"),
            Command::Position { spec: PositionSpec::StartPos, moves: vec![] },
        );
    }

    #[test]
    fn parses_position_startpos_with_moves() {
        assert_eq!(
            parse_uci_line("position startpos moves e2e4 e7e5"),
            Command::Position {
                spec: PositionSpec::StartPos,
                moves: vec!["e2e4".to_string(), "e7e5".to_string()],
            },
        );
    }

    #[test]
    fn parses_position_startpos_moves_with_garbage_token() {
        // Pins §5: post-`moves` tokens are raw move strings (validated in
        // M2.C, not here). A buggy impl that filtered post-`moves`
        // tokens by some heuristic (length-4-or-5, ASCII only, etc.)
        // would slip through `parses_position_startpos_with_moves`'s
        // legal-looking-only inputs. This test pins the raw-collection
        // contract.
        assert_eq!(
            parse_uci_line("position startpos moves e2e4 garbage"),
            Command::Position {
                spec: PositionSpec::StartPos,
                moves: vec!["e2e4".to_string(), "garbage".to_string()],
            },
        );
    }

    // ─── position — lenient-stop semantics (Stockfish-confirmed §3.1) ────

    #[test]
    fn parses_position_startpos_garbage_yields_empty_moves() {
        // Stockfish-confirmed (§3.1): `position`'s body is lenient-stop —
        // accept what's parsed, ignore the rest. A buggy impl that
        // defaulted to `go`'s lenient-skip rule (skip `garbage`,
        // continue) would ALSO ignore garbage and produce the same
        // result here — see `parses_position_startpos_foo_moves_e2e4`
        // for the disambiguating test.
        assert_eq!(
            parse_uci_line("position startpos garbage"),
            Command::Position { spec: PositionSpec::StartPos, moves: vec![] },
        );
    }

    #[test]
    fn parses_position_startpos_foo_moves_e2e4() {
        // Stockfish-confirmed (§3.1) via `d` command — board displayed
        // as starting-position FEN, NOT post-1.e4. The `foo` token
        // BETWEEN `startpos` and `moves` must DISCARD the `moves`
        // clause entirely — this is the behavior that distinguishes
        // §position's lenient-stop from §go's lenient-skip. A buggy
        // lenient-skip impl would skip `foo`, find `moves`, and produce
        // moves=["e2e4"]. Failing this test means we'd be applying
        // moves the spec/Stockfish say should be ignored.
        assert_eq!(
            parse_uci_line("position startpos foo moves e2e4"),
            Command::Position { spec: PositionSpec::StartPos, moves: vec![] },
        );
    }

    #[test]
    fn parses_position_startpos_moves_keyword_no_list() {
        // Pins §5: `moves` keyword present, list empty — equivalent to
        // no `moves`. The `Vec` representation collapses both cases
        // (no Option around moves), so this just verifies the parser
        // doesn't choke on an immediately-EOL `moves` keyword.
        assert_eq!(
            parse_uci_line("position startpos moves"),
            Command::Position { spec: PositionSpec::StartPos, moves: vec![] },
        );
    }

    #[test]
    fn parses_position_fen_moves_keyword_no_list() {
        // Symmetric to the startpos version above. Pins that the
        // trailing-moves clause logic is shared between startpos and
        // fen branches — a future divergence in dispatch (e.g. fen
        // branch checks `moves` but doesn't handle empty list)
        // would be caught here.
        let fen = "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1";
        assert_eq!(
            parse_uci_line(&format!("position fen {fen} moves")),
            Command::Position {
                spec: PositionSpec::Fen(fen.to_string()),
                moves: vec![],
            },
        );
    }

    #[test]
    fn parses_position_fen_with_trailing_garbage_no_moves() {
        // Pins §5 strict-6-FEN-tokens + lenient-stop combo. After the
        // 6 FEN tokens, `garbage` is not `moves`, so the rest is
        // dropped silently. FEN string is exactly the 6 tokens.
        let fen = "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1";
        assert_eq!(
            parse_uci_line(&format!("position fen {fen} garbage")),
            Command::Position {
                spec: PositionSpec::Fen(fen.to_string()),
                moves: vec![],
            },
        );
    }

    #[test]
    fn parses_position_fen_garbage_before_moves_discards_clause() {
        // Pins §5 worked example: junk between FEN body and `moves`
        // discards the `moves` clause entirely (lenient-stop). Same
        // behavior as `parses_position_startpos_foo_moves_e2e4`.
        let fen = "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1";
        assert_eq!(
            parse_uci_line(&format!("position fen {fen} garbage moves e2e4")),
            Command::Position {
                spec: PositionSpec::Fen(fen.to_string()),
                moves: vec![],
            },
        );
    }

    #[test]
    fn parses_position_fen_normalizes_inter_field_whitespace() {
        // Pins §7 "join with single spaces" — even if the input has
        // tabs / multi-space between FEN fields, the resulting Fen
        // string has exactly single spaces. A buggy impl that just
        // substring-extracts from the input would preserve the
        // non-canonical whitespace and fail this test.
        let canonical = "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1";
        assert_eq!(
            parse_uci_line("position fen rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR\tw\tKQkq\t-\t0\t1"),
            Command::Position {
                spec: PositionSpec::Fen(canonical.to_string()),
                moves: vec![],
            },
        );
    }

    #[test]
    fn parses_position_fen_no_moves() {
        let fen = "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1";
        assert_eq!(
            parse_uci_line(&format!("position fen {fen}")),
            Command::Position {
                spec: PositionSpec::Fen(fen.to_string()),
                moves: vec![],
            },
        );
    }

    #[test]
    fn parses_position_fen_with_moves() {
        let fen = "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1";
        assert_eq!(
            parse_uci_line(&format!("position fen {fen} moves e2e4 e7e5")),
            Command::Position {
                spec: PositionSpec::Fen(fen.to_string()),
                moves: vec!["e2e4".to_string(), "e7e5".to_string()],
            },
        );
    }

    #[test]
    fn parses_position_fen_kiwipete() {
        // Kiwipete — well-known perft test position. Round-trip the
        // bytes; no validation at this layer.
        let kiwipete = "r3k2r/p1ppqpb1/bn2pnp1/3PN3/1p2P3/2N2Q1p/PPPBBPPP/R3K2R w KQkq - 0 1";
        assert_eq!(
            parse_uci_line(&format!("position fen {kiwipete}")),
            Command::Position {
                spec: PositionSpec::Fen(kiwipete.to_string()),
                moves: vec![],
            },
        );
    }

    // ─── go ───────────────────────────────────────────────────────────────

    #[test]
    fn parses_go_empty() {
        assert_eq!(parse_uci_line("go"), Command::Go(GoParams::default()));
    }

    #[test]
    fn parses_go_infinite() {
        assert_eq!(
            parse_uci_line("go infinite"),
            Command::Go(GoParams { infinite: true, ..GoParams::default() }),
        );
    }

    #[test]
    fn parses_go_ponder() {
        assert_eq!(
            parse_uci_line("go ponder"),
            Command::Go(GoParams { ponder: true, ..GoParams::default() }),
        );
    }

    #[test]
    fn parses_go_movetime() {
        assert_eq!(
            parse_uci_line("go movetime 1500"),
            Command::Go(GoParams { movetime: Some(1500), ..GoParams::default() }),
        );
    }

    #[test]
    fn parses_go_full_time_control() {
        assert_eq!(
            parse_uci_line("go wtime 300000 btime 300000 winc 1000 binc 1000 movestogo 40"),
            Command::Go(GoParams {
                wtime: Some(300_000),
                btime: Some(300_000),
                winc: Some(1000),
                binc: Some(1000),
                movestogo: Some(40),
                ..GoParams::default()
            }),
        );
    }

    #[test]
    fn parses_go_depth() {
        assert_eq!(
            parse_uci_line("go depth 8"),
            Command::Go(GoParams { depth: Some(8), ..GoParams::default() }),
        );
    }

    #[test]
    fn parses_go_nodes() {
        assert_eq!(
            parse_uci_line("go nodes 1000000"),
            Command::Go(GoParams { nodes: Some(1_000_000), ..GoParams::default() }),
        );
    }

    #[test]
    fn parses_go_mate() {
        assert_eq!(
            parse_uci_line("go mate 5"),
            Command::Go(GoParams { mate: Some(5), ..GoParams::default() }),
        );
    }

    #[test]
    fn parses_go_mate_zero() {
        // Pins degenerate "is current position checkmate?" semantics
        // per §4 — `mate 0` parses cleanly to Some(0); consumers handle.
        assert_eq!(
            parse_uci_line("go mate 0"),
            Command::Go(GoParams { mate: Some(0), ..GoParams::default() }),
        );
    }

    #[test]
    fn parses_go_searchmoves_only() {
        assert_eq!(
            parse_uci_line("go searchmoves e2e4 d2d4"),
            Command::Go(GoParams {
                searchmoves: Some(vec!["e2e4".to_string(), "d2d4".to_string()]),
                ..GoParams::default()
            }),
        );
    }

    #[test]
    fn parses_go_searchmoves_then_keyword() {
        assert_eq!(
            parse_uci_line("go searchmoves e2e4 d2d4 wtime 5000"),
            Command::Go(GoParams {
                searchmoves: Some(vec!["e2e4".to_string(), "d2d4".to_string()]),
                wtime: Some(5000),
                ..GoParams::default()
            }),
        );
    }

    #[test]
    fn parses_go_searchmoves_empty() {
        // Keyword present, list empty — distinguishable from None per §4.
        assert_eq!(
            parse_uci_line("go searchmoves"),
            Command::Go(GoParams {
                searchmoves: Some(vec![]),
                ..GoParams::default()
            }),
        );
    }

    #[test]
    fn parses_go_searchmoves_immediately_followed_by_keyword() {
        assert_eq!(
            parse_uci_line("go searchmoves wtime 5000"),
            Command::Go(GoParams {
                searchmoves: Some(vec![]),
                wtime: Some(5000),
                ..GoParams::default()
            }),
        );
    }

    #[test]
    fn parses_go_negative_clock() {
        // wtime/btime are i64 — clock can go negative under time-trouble.
        assert_eq!(
            parse_uci_line("go wtime -1500 btime 5000"),
            Command::Go(GoParams {
                wtime: Some(-1500),
                btime: Some(5000),
                ..GoParams::default()
            }),
        );
    }

    #[test]
    fn parses_go_field_order_independent() {
        assert_eq!(
            parse_uci_line("go btime 5000 depth 4 wtime 3000"),
            Command::Go(GoParams {
                btime: Some(5000),
                depth: Some(4),
                wtime: Some(3000),
                ..GoParams::default()
            }),
        );
    }

    // -----------------------------------------------------------------------
    // Group B — anchor unit tests, Unknown cases.
    //
    // 26 tests per §B.
    // -----------------------------------------------------------------------

    #[test]
    fn rejects_empty_line() {
        assert_eq!(parse_uci_line(""), Command::Unknown);
    }

    #[test]
    fn rejects_whitespace_only() {
        assert_eq!(parse_uci_line("   \t  "), Command::Unknown);
    }

    #[test]
    fn rejects_no_recognized_keyword() {
        assert_eq!(parse_uci_line("foo bar baz"), Command::Unknown);
    }

    #[test]
    fn rejects_uppercase_command_keyword() {
        // Pins §3.3 "lowercase command keywords only" at the COMMAND
        // KEYWORD level (not just at arg level). A buggy impl that
        // case-folded the first token before dispatch would let `UCI`
        // through. `rejects_debug_uppercase_arg` only tests this at
        // the arg level, leaving the command-keyword case unguarded.
        assert_eq!(parse_uci_line("UCI"), Command::Unknown);
        assert_eq!(parse_uci_line("Uci"), Command::Unknown);
        assert_eq!(parse_uci_line("ISREADY"), Command::Unknown);
    }

    #[test]
    fn rejects_leading_unknown_before_uci() {
        // Stockfish-confirmed (§3.1): strict-first-token, no leading-skip.
        assert_eq!(parse_uci_line("joho uci"), Command::Unknown);
    }

    #[test]
    fn rejects_leading_unknown_before_isready() {
        // Stockfish-confirmed (§3.1).
        assert_eq!(parse_uci_line("xyzzybanana isready"), Command::Unknown);
    }

    #[test]
    fn rejects_debug_no_arg() {
        // Stockfish-confirmed (§3.1).
        assert_eq!(parse_uci_line("debug"), Command::Unknown);
    }

    #[test]
    fn rejects_debug_bad_arg() {
        assert_eq!(parse_uci_line("debug maybe"), Command::Unknown);
    }

    #[test]
    fn rejects_debug_uppercase_arg() {
        assert_eq!(parse_uci_line("debug ON"), Command::Unknown);
    }

    #[test]
    fn rejects_debug_with_intervening_token() {
        // Stockfish-confirmed (§3.1): strict body for debug.
        assert_eq!(parse_uci_line("debug joho on"), Command::Unknown);
    }

    #[test]
    fn rejects_debug_with_trailing_token() {
        // Stockfish-confirmed (§3.1): debug is strict-exact-2-tokens —
        // even valid arg + trailing junk is rejected.
        assert_eq!(parse_uci_line("debug on garbage"), Command::Unknown);
    }

    #[test]
    fn rejects_setoption_no_args() {
        assert_eq!(parse_uci_line("setoption"), Command::Unknown);
    }

    #[test]
    fn rejects_setoption_no_name_keyword() {
        assert_eq!(parse_uci_line("setoption value foo"), Command::Unknown);
    }

    #[test]
    fn rejects_setoption_token_before_name() {
        // Pins §3.2: `setoption`'s immediate next token must be `name`.
        // A buggy impl that scanned forward looking for `name` (rather
        // than requiring it as the next token) would silently accept
        // this and produce SetOption { name: "X", value: Some("Y") }.
        assert_eq!(parse_uci_line("setoption foo name X value Y"), Command::Unknown);
    }

    #[test]
    fn rejects_setoption_empty_name() {
        // No tokens follow `name`.
        assert_eq!(parse_uci_line("setoption name"), Command::Unknown);
    }

    #[test]
    fn rejects_setoption_name_immediately_value() {
        // Next token after `name` is the `value` keyword — name body collects 0.
        assert_eq!(parse_uci_line("setoption name value foo"), Command::Unknown);
    }

    #[test]
    fn rejects_register_no_args() {
        assert_eq!(parse_uci_line("register"), Command::Unknown);
    }

    #[test]
    fn rejects_register_bad_subcommand() {
        assert_eq!(parse_uci_line("register foo"), Command::Unknown);
    }

    #[test]
    fn rejects_register_name_empty_and_no_code() {
        // `name` keyword with empty body, no `code` keyword → both None → Unknown.
        assert_eq!(parse_uci_line("register name"), Command::Unknown);
    }

    #[test]
    fn rejects_register_code_empty_and_no_name() {
        assert_eq!(parse_uci_line("register code"), Command::Unknown);
    }

    #[test]
    fn rejects_register_both_keywords_empty_bodies() {
        // Both `name` and `code` keywords present, both bodies empty.
        assert_eq!(parse_uci_line("register name code"), Command::Unknown);
    }

    #[test]
    fn rejects_register_last_wins_to_both_none() {
        // Last-wins overwrites BOTH fields with None: second `name` body
        // is empty (terminated by `code`), second `code` body is empty
        // (EOL). Final state has both None → Unknown. Exercises the §4
        // debug_assert!(name.is_some() || code.is_some()) path.
        assert_eq!(
            parse_uci_line("register name Stefan code 123 name code"),
            Command::Unknown,
        );
    }

    #[test]
    fn rejects_position_no_args() {
        assert_eq!(parse_uci_line("position"), Command::Unknown);
    }

    #[test]
    fn rejects_position_moves_only() {
        // `startpos` / `fen` must come first.
        assert_eq!(parse_uci_line("position moves e2e4"), Command::Unknown);
    }

    #[test]
    fn rejects_position_fen_no_body() {
        // Strict-6-FEN-tokens; got 0.
        assert_eq!(parse_uci_line("position fen"), Command::Unknown);
    }

    #[test]
    fn rejects_position_fen_only_5_tokens() {
        // Strict-6-FEN-tokens; got 5 (missing fullmove counter).
        assert_eq!(
            parse_uci_line("position fen rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0"),
            Command::Unknown,
        );
    }

    // -----------------------------------------------------------------------
    // Group C — whitespace tolerance (table-driven). 3 tests per §C.
    // -----------------------------------------------------------------------

    #[test]
    fn whitespace_equivalence_debug_on() {
        let canonical = parse_uci_line("debug on");
        for variant in [
            "debug on",
            "  debug   on  ",
            "\tdebug\ton\t",
            "  debug\t  \t\ton\t  ",
            "debug\t\t\t\ton",
            "\n\tdebug on\r",
        ] {
            assert_eq!(parse_uci_line(variant), canonical, "variant: {variant:?}");
        }
        assert_eq!(canonical, Command::Debug(DebugMode::On));
    }

    #[test]
    fn whitespace_equivalence_position_startpos_with_moves() {
        let canonical = parse_uci_line("position startpos moves e2e4");
        for variant in [
            "position startpos moves e2e4",
            "  position   startpos   moves   e2e4  ",
            "\tposition\tstartpos\tmoves\te2e4",
            "position\n\tstartpos moves e2e4\r",
        ] {
            assert_eq!(parse_uci_line(variant), canonical, "variant: {variant:?}");
        }
    }

    #[test]
    fn whitespace_equivalence_go_full_tc() {
        let canonical = parse_uci_line("go wtime 5000 btime 3000");
        for variant in [
            "go wtime 5000 btime 3000",
            "  go   wtime\t5000  btime  3000  ",
            "go\twtime\t5000\tbtime\t3000",
        ] {
            assert_eq!(parse_uci_line(variant), canonical, "variant: {variant:?}");
        }
    }

    // -----------------------------------------------------------------------
    // Group D — trailing-junk-after-no-arg-command. 5 tests per §D.
    // -----------------------------------------------------------------------

    #[test]
    fn trailing_junk_isready() {
        // Stockfish-confirmed (§3.1).
        assert_eq!(parse_uci_line("isready foo bar"), Command::IsReady);
    }

    #[test]
    fn trailing_junk_uci() {
        assert_eq!(parse_uci_line("uci x y z"), Command::Uci);
    }

    #[test]
    fn trailing_junk_quit() {
        assert_eq!(parse_uci_line("quit garbage"), Command::Quit);
    }

    #[test]
    fn trailing_junk_stop() {
        assert_eq!(parse_uci_line("stop now please"), Command::Stop);
    }

    #[test]
    fn trailing_junk_ucinewgame() {
        assert_eq!(parse_uci_line("ucinewgame x"), Command::UciNewGame);
    }

    // -----------------------------------------------------------------------
    // Group E — `Go` semantics + data-invariant. 11 tests per §E.
    // -----------------------------------------------------------------------

    #[test]
    fn go_malformed_numeric_skipped() {
        // Stockfish-confirmed (§3.1) lenient-skip on bad numerics.
        assert_eq!(
            parse_uci_line("go wtime abc btime 5000"),
            Command::Go(GoParams {
                btime: Some(5000),
                ..GoParams::default()
            }),
        );
    }

    #[test]
    fn go_truncated_numeric() {
        // `wtime` keyword with no value to consume — field stays None.
        // The semantic intent is "equivalent to bare `go`" — comparing
        // to `parse_uci_line("go")` (rather than to GoParams::default()
        // directly) catches drift in EITHER the truncated-numeric branch
        // or the bare-go branch.
        assert_eq!(parse_uci_line("go wtime"), parse_uci_line("go"));
    }

    #[test]
    fn go_unknown_token_in_body_ignored() {
        // Stockfish-confirmed (§3.1).
        assert_eq!(
            parse_uci_line("go foo wtime 5000"),
            Command::Go(GoParams { wtime: Some(5000), ..GoParams::default() }),
        );
    }

    #[test]
    fn go_unknown_token_between_keywords_ignored() {
        // Stockfish-confirmed (§3.1).
        assert_eq!(
            parse_uci_line("go wtime 5000 foo btime 3000"),
            Command::Go(GoParams {
                wtime: Some(5000),
                btime: Some(3000),
                ..GoParams::default()
            }),
        );
    }

    #[test]
    fn go_repeated_keyword_last_wins() {
        assert_eq!(
            parse_uci_line("go depth 4 depth 8"),
            Command::Go(GoParams { depth: Some(8), ..GoParams::default() }),
        );
    }

    #[test]
    fn go_consumed_bad_token_not_re_scanned_as_keyword() {
        // Pins §6.b: the consumed bad numeric token is NOT re-scanned
        // as a possible keyword. The disambiguating input puts a
        // RECOGNIZED keyword (`wtime`) where the failed value would be:
        //
        //   `go wtime wtime 5000`
        //
        // Per-spec (consume-bad-value): first `wtime` consumes the
        // second `wtime` as its (unparseable) value — `wtime` doesn't
        // parse as i64, wtime stays None. Loop continues at `5000`,
        // which is not a keyword → dropped. Result: wtime: None.
        //
        // Buggy (re-scan-bad-token): first `wtime` peeks the second
        // `wtime`, recognizes it as a keyword, leaves it on the iterator.
        // Second `wtime` then consumes `5000`. Result: wtime: Some(5000).
        //
        // The two impls produce DIFFERENT results — this test
        // disambiguates §6.b. (`go wtime abc wtime 5000` would NOT
        // disambiguate, because `abc` isn't a keyword either way.)
        assert_eq!(
            parse_uci_line("go wtime wtime 5000"),
            Command::Go(GoParams::default()),
        );
    }

    #[test]
    fn go_repeated_searchmoves_treated_as_move_string() {
        // Pins §6.c: searchmoves is NOT in its own termination set —
        // a second `searchmoves` keyword is appended as a literal token.
        // M2.C's from_uci will reject "searchmoves" as Malformed.
        assert_eq!(
            parse_uci_line("go searchmoves e2e4 searchmoves d2d4"),
            Command::Go(GoParams {
                searchmoves: Some(vec![
                    "e2e4".to_string(),
                    "searchmoves".to_string(),
                    "d2d4".to_string(),
                ]),
                ..GoParams::default()
            }),
        );
    }

    #[test]
    fn go_winc_negative_rejected() {
        // winc is u64; negative parse fails → field stays None.
        assert_eq!(
            parse_uci_line("go winc -100 binc 200"),
            Command::Go(GoParams { binc: Some(200), ..GoParams::default() }),
        );
    }

    #[test]
    fn go_nodes_overflow_skipped() {
        // 99999999999999999999999 doesn't fit in u64 → parse fails →
        // field stays None. depth still parsed.
        assert_eq!(
            parse_uci_line("go nodes 99999999999999999999999 depth 4"),
            Command::Go(GoParams { depth: Some(4), ..GoParams::default() }),
        );
    }

    #[test]
    fn go_combined_infinite_and_searchmoves() {
        assert_eq!(
            parse_uci_line("go infinite searchmoves e2e4"),
            Command::Go(GoParams {
                infinite: true,
                searchmoves: Some(vec!["e2e4".to_string()]),
                ..GoParams::default()
            }),
        );
    }

    #[test]
    fn searchmoves_terminators_constant_matches_6a_table() {
        // Pins §11.5 data invariant — the SEARCHMOVES_TERMINATORS const
        // must contain exactly the 11 §6.a non-`searchmoves` keywords.
        //
        // MAINTENANCE: this test is a sanity check, not a derivation —
        // the `expected` literal below is the same data as the source
        // constant, deliberately duplicated so a single-place edit (in
        // src or test) trips the comparison. If you modify either, you
        // MUST also update `docs/plans/m2.b.md` §6.a so all three
        // sources stay in sync. The duplication is by design.
        let expected_from_plan_6a: &[&str] = &[
            "ponder",
            "infinite",
            "wtime",
            "btime",
            "winc",
            "binc",
            "movestogo",
            "depth",
            "nodes",
            "mate",
            "movetime",
        ];
        assert_eq!(SEARCHMOVES_TERMINATORS.len(), 11);
        assert_eq!(SEARCHMOVES_TERMINATORS, expected_from_plan_6a);
    }

    // -----------------------------------------------------------------------
    // Group F — property tests. 3 properties per §F.
    // -----------------------------------------------------------------------

    /// Tokens drawn from the union of: known UCI keywords + typical
    /// option-name pieces + numeric strings + UCI moves + arbitrary
    /// short ASCII identifiers. Used by F.1 and F.2 strategies.
    fn arbitrary_uci_token() -> impl Strategy<Value = String> {
        prop_oneof![
            // Known keywords.
            prop::sample::select(vec![
                "uci", "debug", "isready", "setoption", "register",
                "ucinewgame", "position", "go", "stop", "ponderhit", "quit",
                "on", "off", "name", "value", "later", "code",
                "startpos", "fen", "moves",
                "ponder", "infinite", "wtime", "btime", "winc", "binc",
                "movestogo", "depth", "nodes", "mate", "movetime", "searchmoves",
            ])
                .prop_map(String::from),
            // Typical option-name pieces.
            prop::sample::select(vec!["Hash", "Clear", "UCI_Opponent", "Foo", "Bar"])
                .prop_map(String::from),
            // Numerics (some valid, some malformed).
            prop::sample::select(vec!["5000", "0", "-300", "abc", "1500", "99999"])
                .prop_map(String::from),
            // UCI moves.
            prop::sample::select(vec!["e2e4", "e7e5", "e7e8q", "0000", "g1f3", "e1g1"])
                .prop_map(String::from),
            // Short ASCII identifiers (deterministic small set; we
            // intentionally avoid prop::string::string_regex here to
            // keep the strategy's outputs tractable for grep-by-eye
            // when proptest shrinks).
            prop::sample::select(vec!["foo", "bar", "baz", "qux", "quux"])
                .prop_map(String::from),
        ]
    }

    /// Whitespace-only run, 1–4 chars. Used by F.1.
    fn arbitrary_whitespace_run() -> impl Strategy<Value = String> {
        prop::collection::vec(prop::sample::select(vec![' ', '\t', '\n', '\r']), 1..=4)
            .prop_map(|chars| chars.into_iter().collect())
    }

    /// Tokens that are NOT recognized command keywords. Used by F.2.
    fn arbitrary_unknown_token() -> impl Strategy<Value = String> {
        prop::sample::select(vec!["xyz", "joho", "blarg", "wibble", "foobar"])
            .prop_map(String::from)
    }

    proptest! {
        // F.1. Whitespace insensitivity within a fixed token list.
        // Pins §2 tokenization invariant.
        #[test]
        fn prop_whitespace_does_not_change_command(
            tokens in prop::collection::vec(arbitrary_uci_token(), 1..8),
            ws_runs in prop::collection::vec(arbitrary_whitespace_run(), 8),
        ) {
            let baseline = tokens.join(" ");
            // Interleave: token, ws_run, token, ws_run, ... ending with
            // a token. Use ws_runs cyclically to fill all gaps.
            let mut interleaved = String::new();
            for (i, t) in tokens.iter().enumerate() {
                if i > 0 {
                    interleaved.push_str(&ws_runs[(i - 1) % ws_runs.len()]);
                }
                interleaved.push_str(t);
            }
            prop_assert_eq!(parse_uci_line(&baseline), parse_uci_line(&interleaved));
        }

        // F.2. Trailing junk after no-arg command is ignored.
        // Pins §3 trailing-junk rule.
        #[test]
        fn prop_trailing_junk_after_no_arg_command_ignored(
            cmd in prop::sample::select(vec![
                "uci", "isready", "ucinewgame", "stop", "ponderhit", "quit",
            ]),
            junk in prop::collection::vec(arbitrary_unknown_token(), 0..4),
        ) {
            let baseline = parse_uci_line(cmd);
            // A no-arg command MUST yield a non-Unknown. Using
            // `prop_assert_ne!` (not `prop_assume!`) is load-bearing:
            // if the parser regresses to returning Unknown for valid
            // no-arg commands, this property must FAIL loudly rather
            // than silently skip every case.
            prop_assert_ne!(&baseline, &Command::Unknown);
            let with_trailing = if junk.is_empty() {
                cmd.to_string()
            } else {
                format!("{} {}", cmd, junk.join(" "))
            };
            prop_assert_eq!(baseline, parse_uci_line(&with_trailing));
        }

        // F.3. Total function — no panics on any input.
        // Pins §11 panic-freedom guarantee.
        #[test]
        fn prop_parse_never_panics(line in "(?s).*") {
            // Just call it; we don't care about the result. Any panic
            // is a test failure (proptest catches panics as failures).
            let _ = parse_uci_line(&line);
        }
    }
}
