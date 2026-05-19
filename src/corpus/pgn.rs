//! Streaming PGN reader (CCRL / Lichess shape): Seven-Tag-Roster + SAN
//! movetext → per-game position stream + original game-result label.
//!
//! SAN disambiguation resolves against the LEGAL move set (`generate_moves`
//! is legal-direct — ADR-0007, no pseudo-legal companion), so the
//! pinned-piece case is correct. ANY parse failure drops the WHOLE game and
//! increments `PgnStats.parse_failures` — never "skip the move and
//! continue" (that desynchronizes the stream and mislabels every later
//! position: the Texel-poisoning the roadmap warns of). Streaming, one game
//! buffered at a time (R6).
//!
//! Implemented by the M6.G `pgn` (Opus) coder slice per
//! `docs/plans/m6.g.md` §3.2.

use std::io::BufRead;

use crate::piece::PieceKind;
use crate::square::Square;
use crate::{Move, MoveList, Position, generate_moves};

use super::{CorpusError, Label};

/// Extracted tag-roster fields relevant to filtering.
#[derive(Clone, Debug, Default)]
pub struct PgnTags {
    /// `Result` tag → label (`*`/missing ⇒ `None`, game dropped).
    pub result: Option<Label>,
    /// `Termination` tag (e.g. `"Normal"`, `"Time forfeit"`).
    pub termination: Option<String>,
    /// `WhiteElo` tag, parsed.
    pub white_elo: Option<u32>,
    /// `BlackElo` tag, parsed.
    pub black_elo: Option<u32>,
    /// `TimeControl` tag (PGN spec form, e.g. `"600+5"`).
    pub time_control: Option<String>,
}

/// Counters surfaced into `corpus_stats.txt` + the quality gate.
#[derive(Clone, Debug, Default)]
pub struct PgnStats {
    /// Games encountered (parsed or not).
    pub games_seen: u64,
    /// Games that parsed end-to-end with a usable result and were emitted.
    pub games_emitted: u64,
    /// Games dropped because a SAN/structure parse failed mid-game.
    pub parse_failures: u64,
    /// Games dropped because the result was `*`/missing.
    pub no_result_dropped: u64,
}

/// All positions of one fully-parsed game (label resolved, non-`*`).
pub struct GamePositions {
    /// Assigned game id (game-level split key).
    pub game_id: u64,
    /// The game's tag roster.
    pub tags: PgnTags,
    /// `(position, ply_from_start)` in game order.
    pub positions: Vec<(Position, u32)>,
}

/// Parse one SAN token against the legal move set of `pos`.
///
/// Disambiguation matches against `legal` (the `generate_moves` output),
/// never a pseudo-legal reconstruction: standard SAN omits the
/// disambiguator whenever only one move is *legal*, even if two same-kind
/// pieces pseudo-legally reach the target (the other is pinned). Matching
/// the legal set resolves that correctly; a pseudo-legal match would
/// mis-parse and silently mislabel every later position.
///
/// Returns the unique legal `Move` the token denotes, or
/// `CorpusError::Pgn` if the token is malformed, matches no legal move, or
/// is ambiguous (≠1 match) — the caller then drops the whole game.
pub fn parse_san(token: &str, pos: &Position, legal: &MoveList) -> Result<Move, CorpusError> {
    // 1. Strip trailing decoration: check/mate (`+`/`#`) and annotation
    //    glyphs (`!`, `?`, and their doubled/mixed forms `!!`, `?!`, ...).
    //    These never affect move identity.
    let core = token.trim_end_matches(['+', '#', '!', '?']);
    if core.is_empty() {
        return Err(CorpusError::Pgn(format!("empty SAN token: {token:?}")));
    }

    // 2. Castling. PGN uses the letter `O`; some sources emit the digit
    //    `0`. Accept both. Match against the castling move in the legal
    //    set rather than hard-coding king squares (Chess960 is out of
    //    scope but matching the legal set is simpler and exactly correct
    //    for standard chess too).
    if core == "O-O" || core == "0-0" {
        return unique_legal(legal, |m| m.flag() == crate::MoveFlag::KingCastle, token);
    }
    if core == "O-O-O" || core == "0-0-0" {
        return unique_legal(legal, |m| m.flag() == crate::MoveFlag::QueenCastle, token);
    }

    let bytes = core.as_bytes();

    // 3. Determine the moving piece kind. An uppercase leading letter
    //    `N/B/R/Q/K` names a piece; otherwise it is a pawn move.
    let (kind, rest) = match bytes[0] {
        b'N' => (PieceKind::Knight, &core[1..]),
        b'B' => (PieceKind::Bishop, &core[1..]),
        b'R' => (PieceKind::Rook, &core[1..]),
        b'Q' => (PieceKind::Queen, &core[1..]),
        b'K' => (PieceKind::King, &core[1..]),
        _ => (PieceKind::Pawn, core),
    };

    // 4. Split off an optional promotion suffix `=Q` / `=R` / `=B` / `=N`
    //    (only valid for pawn moves; we let the legal-set match enforce
    //    that rather than special-casing here). Some sources omit the `=`
    //    (e.g. `e8Q`); accept a bare trailing promotion letter too.
    let (body, promo) = split_promotion(rest)?;

    // 5. The body now is `[disambig] [x] dest`. The destination is the
    //    final two chars (a file letter + a rank digit).
    if body.len() < 2 {
        return Err(CorpusError::Pgn(format!("SAN too short: {token:?}")));
    }
    let dest_str = &body[body.len() - 2..];
    let dest = Square::parse_uci(dest_str)
        .ok_or_else(|| CorpusError::Pgn(format!("bad SAN destination in {token:?}")))?;

    // Everything before the destination is `[disambig][x]`. Strip a
    //  capture marker `x` (its position is informational only — the legal
    //  set already knows whether the move captures).
    let prefix = &body[..body.len() - 2];
    let disambig = prefix.strip_suffix('x').unwrap_or(prefix);

    // Disambiguator: 0–2 chars, each either a file letter (a-h) or a rank
    // digit (1-8).
    let (mut want_file, mut want_rank): (Option<u8>, Option<u8>) = (None, None);
    for ch in disambig.bytes() {
        match ch {
            b'a'..=b'h' => want_file = Some(ch - b'a'),
            b'1'..=b'8' => want_rank = Some(ch - b'1'),
            _ => {
                return Err(CorpusError::Pgn(format!(
                    "bad SAN disambiguator in {token:?}"
                )));
            }
        }
    }

    // 6. Match against the LEGAL set: same moving-piece kind, same
    //    destination, promotion kind matching, and the from-square
    //    consistent with any file/rank disambiguator. Exactly one legal
    //    move must satisfy this — SAN guarantees uniqueness against the
    //    legal set by construction; ≠1 is a parse failure (drop the game).
    let stm = pos.side_to_move();
    unique_legal(
        legal,
        |m| {
            let from = m.from_square();
            let Some(p) = pos.piece_at(from) else {
                return false;
            };
            p.color == stm
                && p.kind == kind
                && m.to_square() == dest
                && m.promotion_kind() == promo
                && want_file.map(|f| from.file() == f).unwrap_or(true)
                && want_rank.map(|r| from.rank() == r).unwrap_or(true)
        },
        token,
    )
}

/// Split an optional promotion suffix off the SAN move body. Accepts the
/// PGN-standard `=Q` form and the `=`-less `Q` form some sources emit.
fn split_promotion(rest: &str) -> Result<(&str, Option<PieceKind>), CorpusError> {
    let bytes = rest.as_bytes();
    if let Some(eq) = rest.find('=') {
        let after = &rest[eq + 1..];
        let kind = match after.as_bytes().first() {
            Some(b'N') => PieceKind::Knight,
            Some(b'B') => PieceKind::Bishop,
            Some(b'R') => PieceKind::Rook,
            Some(b'Q') => PieceKind::Queen,
            _ => {
                return Err(CorpusError::Pgn(format!("bad promotion suffix: {rest:?}")));
            }
        };
        return Ok((&rest[..eq], Some(kind)));
    }
    // `=`-less trailing promotion letter: only when the char before it is
    // a rank digit (the destination rank), so we never mistake the leading
    // piece letter of a non-pawn move for a promotion.
    if bytes.len() >= 2 {
        let last = bytes[bytes.len() - 1];
        let prev = bytes[bytes.len() - 2];
        if prev.is_ascii_digit() {
            let kind = match last {
                b'N' => Some(PieceKind::Knight),
                b'B' => Some(PieceKind::Bishop),
                b'R' => Some(PieceKind::Rook),
                b'Q' => Some(PieceKind::Queen),
                _ => None,
            };
            if let Some(k) = kind {
                return Ok((&rest[..rest.len() - 1], Some(k)));
            }
        }
    }
    Ok((rest, None))
}

/// Return the single legal move satisfying `pred`, or a `Pgn` error if zero
/// or more than one match (the whole-game-drop discipline).
fn unique_legal(
    legal: &MoveList,
    pred: impl Fn(&Move) -> bool,
    token: &str,
) -> Result<Move, CorpusError> {
    let mut found: Option<Move> = None;
    for m in legal.iter() {
        if pred(&m) {
            if found.is_some() {
                return Err(CorpusError::Pgn(format!(
                    "ambiguous SAN against legal set: {token:?}"
                )));
            }
            found = Some(m);
        }
    }
    found.ok_or_else(|| CorpusError::Pgn(format!("no legal move for SAN: {token:?}")))
}

// ---------------------------------------------------------------------------
// Movetext tokenizer.
//
// The movetext after the tag roster is a flat sequence interrupted by
// comments `{…}` (possibly nested), NAGs `$n`, recursive variations `(…)`,
// move numbers (`12.` / `12...`), the result token, and SAN tokens. The
// tokenizer yields only the SAN tokens and the terminal result; everything
// else is structurally skipped.
// ---------------------------------------------------------------------------

/// One movetext element we care about.
enum MoveTok {
    /// A SAN move token (decorations not yet stripped).
    San(String),
    /// The game-terminating result token (`1-0` / `0-1` / `1/2-1/2` / `*`).
    Result(()),
}

/// Tokenize one game's movetext string into SAN moves + the terminal
/// result. Skips comments (incl. nested), NAGs, and recursive variations.
/// Returns `Err` only on structurally broken input (unterminated comment /
/// variation) — that drops the whole game (never mislabels).
fn tokenize_movetext(text: &str) -> Result<Vec<MoveTok>, CorpusError> {
    let bytes = text.as_bytes();
    let mut i = 0;
    let mut out = Vec::new();
    while i < bytes.len() {
        let c = bytes[i];
        match c {
            // Whitespace.
            b' ' | b'\t' | b'\r' | b'\n' => i += 1,
            // Comment `{ … }` — nesting is non-standard but some exporters
            // produce it; track depth so a nested `{` does not end early.
            b'{' => {
                let mut depth = 1;
                i += 1;
                while i < bytes.len() && depth > 0 {
                    match bytes[i] {
                        b'{' => depth += 1,
                        b'}' => depth -= 1,
                        _ => {}
                    }
                    i += 1;
                }
                if depth != 0 {
                    return Err(CorpusError::Pgn("unterminated comment".into()));
                }
            }
            // Rest-of-line comment `;` (PGN spec) — to end of line.
            b';' => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            // Recursive variation `( … )` — nested; skip wholesale
            // (variations are not played moves).
            b'(' => {
                let mut depth = 1;
                i += 1;
                while i < bytes.len() && depth > 0 {
                    match bytes[i] {
                        b'(' => depth += 1,
                        b')' => depth -= 1,
                        // A comment inside a variation must not let a `)`
                        // inside the comment close the variation.
                        b'{' => {
                            let mut cd = 1;
                            i += 1;
                            while i < bytes.len() && cd > 0 {
                                match bytes[i] {
                                    b'{' => cd += 1,
                                    b'}' => cd -= 1,
                                    _ => {}
                                }
                                i += 1;
                            }
                            if cd != 0 {
                                return Err(CorpusError::Pgn(
                                    "unterminated comment in variation".into(),
                                ));
                            }
                            continue;
                        }
                        _ => {}
                    }
                    i += 1;
                }
                if depth != 0 {
                    return Err(CorpusError::Pgn("unterminated variation".into()));
                }
            }
            // NAG `$n` — skip the `$` and following digits.
            b'$' => {
                i += 1;
                while i < bytes.len() && bytes[i].is_ascii_digit() {
                    i += 1;
                }
            }
            // A word: move number, SAN move, or result token.
            _ => {
                let start = i;
                while i < bytes.len() {
                    match bytes[i] {
                        b' ' | b'\t' | b'\r' | b'\n' | b'{' | b'}' | b'(' | b')' | b';' | b'$' => {
                            break;
                        }
                        _ => i += 1,
                    }
                }
                let word = &text[start..i];
                classify_word(word, &mut out);
            }
        }
    }
    Ok(out)
}

/// Classify one whitespace-delimited word from the movetext.
fn classify_word(word: &str, out: &mut Vec<MoveTok>) {
    // Result tokens terminate the game.
    if word == "1-0" || word == "0-1" || word == "1/2-1/2" || word == "*" {
        out.push(MoveTok::Result(()));
        return;
    }
    // Move-number tokens: `12.` / `12...` / a bare `12` (rare). They may
    // also be glued to the move (`12.e4`); strip a leading
    // `digits` + `.`/`...` run and keep any SAN remainder.
    let bytes = word.as_bytes();
    let mut p = 0;
    while p < bytes.len() && bytes[p].is_ascii_digit() {
        p += 1;
    }
    if p > 0 {
        // Consumed a leading number; consume the dots that follow it.
        let mut q = p;
        while q < bytes.len() && bytes[q] == b'.' {
            q += 1;
        }
        if q == bytes.len() {
            // Pure move-number token (`12.`, `12...`, `12`).
            return;
        }
        if q > p {
            // `12.e4` form — the SAN move is the remainder.
            out.push(MoveTok::San(word[q..].to_string()));
            return;
        }
        // Leading digits but no dot and more chars: a pawn move can't
        // start with a digit, so this is malformed; emit it as SAN and
        // let `parse_san` reject it (drops the game — never mislabels).
        out.push(MoveTok::San(word.to_string()));
        return;
    }
    // An Elo-style ellipsis-only `...` continuation marker (Black to move
    // after a comment) carries no move.
    if word.bytes().all(|b| b == b'.') {
        return;
    }
    out.push(MoveTok::San(word.to_string()));
}

// ---------------------------------------------------------------------------
// Tag-roster parsing.
// ---------------------------------------------------------------------------

/// Parse one `[Key "Value"]` tag line. Returns `(key, value)` or `None` if
/// the line is not a well-formed tag pair.
fn parse_tag_line(line: &str) -> Option<(&str, String)> {
    let line = line.trim();
    let inner = line.strip_prefix('[')?.strip_suffix(']')?;
    let key_end = inner.find(' ')?;
    let key = &inner[..key_end];
    let rest = inner[key_end + 1..].trim();
    let val = rest.strip_prefix('"')?.strip_suffix('"')?;
    // PGN escapes `\\` and `\"` inside tag values.
    let val = val.replace("\\\"", "\"").replace("\\\\", "\\");
    Some((key, val))
}

/// Apply a parsed tag pair to the roster.
fn apply_tag(tags: &mut PgnTags, key: &str, val: String) {
    match key {
        "Result" => tags.result = Label::from_result_tag(&val),
        "Termination" => tags.termination = Some(val),
        "WhiteElo" => tags.white_elo = val.trim().parse().ok(),
        "BlackElo" => tags.black_elo = val.trim().parse().ok(),
        "TimeControl" => tags.time_control = Some(val),
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Streaming reader.
// ---------------------------------------------------------------------------

/// One buffered raw game: its tag lines + its movetext block, as text.
#[derive(Default)]
struct RawGame {
    tag_lines: Vec<String>,
    movetext: String,
    /// `true` once at least one tag or movetext line has been seen — used
    /// to detect a real (non-empty) game at EOF.
    nonempty: bool,
}

/// Stream games from a PGN reader. `on_game` is invoked ONCE per game that
/// parses end-to-end with a non-`*` `Result`; a parse failure or `*`/
/// missing result drops the whole game (counted in `stats`). Returns the
/// next free `game_id`.
///
/// One game is buffered at a time (R6): the reader accumulates tag lines
/// then movetext until the next game's tag block begins (a `[` line after
/// movetext has started) or EOF, then processes and releases that game's
/// buffer before reading the next.
pub fn stream_pgn<R: BufRead>(
    r: R,
    base_game_id: u64,
    on_game: &mut dyn FnMut(GamePositions),
    stats: &mut PgnStats,
) -> Result<u64, CorpusError> {
    let mut next_id = base_game_id;
    let mut cur = RawGame::default();
    // `true` while we are inside a game's movetext (tag block already
    // closed) — a `[` line then starts the NEXT game.
    let mut in_movetext = false;

    for line in r.lines() {
        let line = line?;
        let trimmed = line.trim();

        if trimmed.starts_with('[') && trimmed.ends_with(']') && trimmed.contains('"') {
            // A tag line. If we were in movetext, this opens the next game.
            if in_movetext {
                process_game(&cur, &mut next_id, on_game, stats);
                cur = RawGame::default();
                in_movetext = false;
            }
            cur.tag_lines.push(trimmed.to_string());
            cur.nonempty = true;
        } else if trimmed.is_empty() {
            // Blank line: the tag/movetext separator. If we have tags but
            // no movetext yet, this is the roster terminator; otherwise a
            // blank line inside/after movetext is just whitespace.
            if !cur.tag_lines.is_empty() {
                in_movetext = true;
            }
        } else {
            // Movetext content. Preserve the line boundary with `\n`
            // (not a space) so the tokenizer's rest-of-line `;` comment
            // handling stays line-scoped.
            in_movetext = true;
            cur.nonempty = true;
            cur.movetext.push_str(trimmed);
            cur.movetext.push('\n');
        }
    }

    // Final buffered game at EOF.
    if cur.nonempty {
        process_game(&cur, &mut next_id, on_game, stats);
    }

    Ok(next_id)
}

/// Process one buffered raw game: parse tags + replay SAN. On ANY failure
/// the whole game is dropped and the matching stat incremented; `on_game`
/// is invoked iff the game replays end-to-end with a non-`*` result.
fn process_game(
    raw: &RawGame,
    next_id: &mut u64,
    on_game: &mut dyn FnMut(GamePositions),
    stats: &mut PgnStats,
) {
    stats.games_seen += 1;

    let mut tags = PgnTags::default();
    for line in &raw.tag_lines {
        if let Some((k, v)) = parse_tag_line(line) {
            apply_tag(&mut tags, k, v);
        }
    }

    // `*` / missing result ⇒ drop the whole game, do NOT mislabel.
    if tags.result.is_none() {
        stats.no_result_dropped += 1;
        return;
    }

    let toks = match tokenize_movetext(&raw.movetext) {
        Ok(t) => t,
        Err(_) => {
            stats.parse_failures += 1;
            return;
        }
    };

    // Replay every SAN move from the start position. ANY failure ⇒ the
    // whole game is dropped (never "skip and continue" — that would
    // desynchronize the stream and mislabel every later position).
    let mut pos = Position::starting_position();
    let mut positions: Vec<(Position, u32)> = Vec::new();
    let mut ply: u32 = 0;
    // Ply 0 is the start position itself (a labeled position).
    positions.push((pos, ply));

    let mut legal = MoveList::new();
    for tok in &toks {
        match tok {
            MoveTok::Result(()) => break,
            MoveTok::San(san) => {
                generate_moves(&pos, &mut legal);
                let mv = match parse_san(san, &pos, &legal) {
                    Ok(m) => m,
                    Err(_) => {
                        stats.parse_failures += 1;
                        return;
                    }
                };
                pos.make_move(mv);
                ply += 1;
                positions.push((pos, ply));
            }
        }
    }

    let game_id = *next_id;
    *next_id += 1;
    stats.games_emitted += 1;
    on_game(GamePositions {
        game_id,
        tags,
        positions,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Parse a single SAN token in `fen`'s position; panics on parse error.
    fn san(fen: &str, token: &str) -> Move {
        let pos = Position::from_fen(fen).expect("test FEN must parse");
        let mut legal = MoveList::new();
        generate_moves(&pos, &mut legal);
        parse_san(token, &pos, &legal).expect("SAN must parse")
    }

    /// Apply a SAN token to `fen` and return the resulting FEN.
    #[allow(dead_code)] // planned test helper — used by golden-game tests not yet in scope
    fn apply(fen: &str, token: &str) -> String {
        let mut pos = Position::from_fen(fen).expect("test FEN must parse");
        let mut legal = MoveList::new();
        generate_moves(&pos, &mut legal);
        let mv = parse_san(token, &pos, &legal).expect("SAN must parse");
        pos.make_move(mv);
        pos.to_fen()
    }

    #[test]
    fn san_parse_piecemove() {
        // Knight from g1 to f3 in the start position.
        let mv = san(
            "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1",
            "Nf3",
        );
        assert_eq!(mv.from_square(), Square::G1);
        assert_eq!(mv.to_square(), Square::F3);
    }

    #[test]
    fn san_parse_capture() {
        // White pawn e4 takes black pawn d5: SAN `exd5`.
        let mv = san(
            "rnbqkbnr/ppp1pppp/8/3p4/4P3/8/PPPP1PPP/RNBQKBNR w KQkq d6 0 2",
            "exd5",
        );
        assert_eq!(mv.from_square(), Square::E4);
        assert_eq!(mv.to_square(), Square::D5);
        assert!(mv.is_capture());
    }

    #[test]
    fn san_parse_file_disambig() {
        // Two white rooks on a1 and h1, both reach d1 along a clear
        // rank 1 (the king is off the back rank so it does not block the
        // h1 rook). `Rad1` = a-file rook to d1, `Rhd1` = h-file rook.
        let fen = "4k3/8/8/8/4K3/8/8/R6R w - - 0 1";
        let mv = san(fen, "Rad1");
        assert_eq!(mv.from_square(), Square::A1);
        assert_eq!(mv.to_square(), Square::D1);
        let mv2 = san(fen, "Rhd1");
        assert_eq!(mv2.from_square(), Square::H1);
    }

    #[test]
    fn san_parse_rank_disambig() {
        // Two white rooks on a1 and a5, both reach a3. `R1a3` = rank-1
        // rook. (File is identical so file-disambig is impossible; SAN
        // uses the rank digit.)
        let fen = "4k3/8/8/R7/8/8/8/R3K3 w - - 0 1";
        let mv = san(fen, "R1a3");
        assert_eq!(mv.from_square(), Square::A1);
        assert_eq!(mv.to_square(), Square::A3);
        let mv2 = san(fen, "R5a3");
        assert_eq!(mv2.from_square(), Square::A5);
    }

    #[test]
    fn san_parse_both_disambig() {
        // Three queens (a1, a5, e1) can all reach a-file/rank-1
        // intersections; full file+rank disambiguation `Qa1a3` needed
        // when neither file nor rank alone is unique. Queens on a1, a5,
        // c1: all reach a3? a1→a3 (file), a5→a3 (file); c1 cannot reach
        // a3 along a clear path... use a square all three reach.
        // Queens on a1, c1, a3 all reach c3: a1→c3 (diagonal),
        // c1→c3 (file), a3→c3 (rank). a1 and a3 share file a; a1 and c1
        // share rank 1 — so the a1 queen needs both coords: `Qa1c3`.
        let fen = "4k3/8/8/8/8/Q7/8/Q1Q1K3 w - - 0 1";
        let mv = san(fen, "Qa1c3");
        assert_eq!(mv.from_square(), Square::A1);
        assert_eq!(mv.to_square(), Square::C3);
    }

    #[test]
    fn san_parse_castle_short() {
        let mv = san("4k3/8/8/8/8/8/8/R3K2R w KQ - 0 1", "O-O");
        assert_eq!(mv.from_square(), Square::E1);
        assert_eq!(mv.to_square(), Square::G1);
        assert_eq!(mv.flag(), crate::MoveFlag::KingCastle);
        // Digit form `0-0` also accepted.
        let mv2 = san("4k3/8/8/8/8/8/8/R3K2R w KQ - 0 1", "0-0");
        assert_eq!(mv2.flag(), crate::MoveFlag::KingCastle);
    }

    #[test]
    fn san_parse_castle_long() {
        let mv = san("r3k3/8/8/8/8/8/8/R3K2R w KQ - 0 1", "O-O-O");
        assert_eq!(mv.from_square(), Square::E1);
        assert_eq!(mv.to_square(), Square::C1);
        assert_eq!(mv.flag(), crate::MoveFlag::QueenCastle);
    }

    #[test]
    fn san_parse_promo() {
        // White pawn a7→a8 promoting to a queen: `a8=Q`.
        let mv = san("4k3/P7/8/8/8/8/8/4K3 w - - 0 1", "a8=Q");
        assert_eq!(mv.from_square(), Square::A7);
        assert_eq!(mv.to_square(), Square::A8);
        assert_eq!(mv.promotion_kind(), Some(PieceKind::Queen));
        // Underpromotion to knight.
        let mv2 = san("4k3/P7/8/8/8/8/8/4K3 w - - 0 1", "a8=N");
        assert_eq!(mv2.promotion_kind(), Some(PieceKind::Knight));
        // `=`-less form `a8Q` also accepted.
        let mv3 = san("4k3/P7/8/8/8/8/8/4K3 w - - 0 1", "a8Q");
        assert_eq!(mv3.promotion_kind(), Some(PieceKind::Queen));
    }

    #[test]
    fn san_parse_promo_capture() {
        // White pawn b7 captures a rook on a8 and promotes: `bxa8=Q`.
        let mv = san("r3k3/1P6/8/8/8/8/8/4K3 w - - 0 1", "bxa8=Q");
        assert_eq!(mv.from_square(), Square::B7);
        assert_eq!(mv.to_square(), Square::A8);
        assert_eq!(mv.promotion_kind(), Some(PieceKind::Queen));
        assert!(mv.is_capture());
    }

    #[test]
    fn san_parse_ep() {
        // White pawn e5, black just played d7-d5, EP target d6:
        // `exd6` is the en-passant capture.
        let fen = "4k3/8/8/3pP3/8/8/8/4K3 w - d6 0 1";
        let mv = san(fen, "exd6");
        assert_eq!(mv.from_square(), Square::E5);
        assert_eq!(mv.to_square(), Square::D6);
        assert_eq!(mv.flag(), crate::MoveFlag::EnPassant);
    }

    #[test]
    fn san_parse_check_mate_suffix_stripped() {
        // `+` (check) and `#` (mate) and annotation glyphs must not
        // affect move identity.
        let fen = "rnbqkbnr/pppp1ppp/8/4p3/4P3/8/PPPP1PPP/RNBQKBNR w KQkq - 0 1";
        let plain = san(fen, "Qh5");
        let checked = san(fen, "Qh5+");
        let annotated = san(fen, "Qh5!?");
        assert_eq!(plain, checked);
        assert_eq!(plain, annotated);
        // Scholar's-mate final move carrying the `#` (mate) suffix: from
        // the pre-mate position (White to move, queen on h5) `Qxf7#` is
        // the f7-pawn capture. The `#` must be stripped, not parsed.
        let mate = san(
            "r1bqkb1r/pppp1ppp/2n2n2/4p2Q/2B1P3/8/PPPP1PPP/RNB1K1NR w KQkq - 4 4",
            "Qxf7#",
        );
        assert_eq!(mate.from_square(), Square::H5);
        assert_eq!(mate.to_square(), Square::F7);
        assert!(mate.is_capture());
    }

    #[test]
    fn san_disambig_resolves_against_legal_not_pseudolegal() {
        // The pinned-piece case the legal-direct contract exists for.
        // White king e1; white knights on c4 and e4; black rook on e8;
        // black king a8. BOTH knights pseudo-legally reach d6 (c4→d6 and
        // e4→d6 are knight jumps). The e4 knight is *absolutely pinned*
        // on the e-file (the only piece between the e8 rook and the e1
        // king), so moving it is illegal — only the c4 knight's Nd6 is
        // legal. Standard SAN therefore omits the disambiguator and
        // writes plain `Nd6`. A pseudo-legal matcher would see two
        // candidates and mis-pick or treat it as ambiguous; matching the
        // LEGAL set (ADR-0007 legal-direct movegen) yields exactly one
        // and resolves it correctly.
        let fen = "k3r3/8/8/8/2N1N3/8/8/4K3 w - - 0 1";
        let pos = Position::from_fen(fen).expect("fen");
        let mut legal = MoveList::new();
        generate_moves(&pos, &mut legal);
        let mv = parse_san("Nd6", &pos, &legal).expect("undisambiguated SAN must resolve");
        assert_eq!(
            mv.from_square(),
            Square::C4,
            "must pick the LEGAL knight (c4), not the pinned e4 knight"
        );
        assert_eq!(mv.to_square(), Square::D6);
        // Confirm the fixture really is the pinned case: exactly one
        // LEGAL knight move targets d6 (the pseudo-legal count is two).
        let to_d6 = legal
            .iter()
            .filter(|m| {
                m.to_square() == Square::D6
                    && pos.piece_at(m.from_square()).unwrap().kind == PieceKind::Knight
            })
            .count();
        assert_eq!(to_d6, 1, "exactly one LEGAL knight move reaches d6");
    }

    #[test]
    fn result_tag_maps_label() {
        let pgn = "[Result \"1-0\"]\n\n1. e4 e5 1-0\n";
        let mut got: Vec<Label> = Vec::new();
        let mut stats = PgnStats::default();
        stream_pgn(
            Cursor::new(pgn),
            0,
            &mut |g| got.push(g.tags.result.unwrap()),
            &mut stats,
        )
        .expect("stream");
        assert_eq!(got, vec![Label::WhiteWin]);
        assert_eq!(stats.games_emitted, 1);

        for (tag, want) in [
            ("0-1", Label::BlackWin),
            ("1/2-1/2", Label::Draw),
            ("1-0", Label::WhiteWin),
        ] {
            let pgn = format!("[Result \"{tag}\"]\n\n1. e4 e5 {tag}\n");
            let mut out = None;
            let mut s = PgnStats::default();
            stream_pgn(Cursor::new(pgn), 0, &mut |g| out = g.tags.result, &mut s).expect("stream");
            assert_eq!(out, Some(want));
        }
    }

    #[test]
    fn no_result_or_star_game_dropped_not_mislabeled() {
        // `*` result and a missing Result tag must both drop the whole
        // game with zero emitted records — never a defaulted/mislabeled
        // outcome.
        for pgn in [
            "[Result \"*\"]\n\n1. e4 e5 *\n",
            "[White \"X\"]\n\n1. e4 e5\n", // no Result tag at all
        ] {
            let mut emitted = 0;
            let mut stats = PgnStats::default();
            stream_pgn(Cursor::new(pgn), 0, &mut |_| emitted += 1, &mut stats).expect("stream");
            assert_eq!(emitted, 0, "no game may be emitted for {pgn:?}");
            assert_eq!(stats.games_emitted, 0);
            assert_eq!(stats.no_result_dropped, 1);
            assert_eq!(stats.parse_failures, 0);
        }
    }

    #[test]
    fn parse_failure_drops_whole_game_and_counts_stat() {
        // A bad SAN token mid-game ⇒ ZERO records from that game ∧
        // parse_failures += 1 (never "skip the move and continue").
        let pgn = "[Result \"1-0\"]\n\n1. e4 e5 2. Zz9 Nc6 1-0\n";
        let mut emitted = 0;
        let mut stats = PgnStats::default();
        stream_pgn(Cursor::new(pgn), 0, &mut |_| emitted += 1, &mut stats).expect("stream");
        assert_eq!(emitted, 0, "a parse failure must drop the WHOLE game");
        assert_eq!(stats.parse_failures, 1);
        assert_eq!(stats.games_emitted, 0);

        // An *illegal* (but well-formed) SAN move likewise drops the
        // whole game: `Nf6` is not legal for White at move 2 here.
        let pgn2 = "[Result \"1-0\"]\n\n1. e4 e5 2. Nf6 1-0\n";
        let mut emitted2 = 0;
        let mut stats2 = PgnStats::default();
        stream_pgn(Cursor::new(pgn2), 0, &mut |_| emitted2 += 1, &mut stats2).expect("stream");
        assert_eq!(emitted2, 0);
        assert_eq!(stats2.parse_failures, 1);
    }

    #[test]
    fn comments_nags_rav_ignored() {
        // A comment (incl. nested braces), a NAG, a recursive variation
        // (with its own comment), and a rest-of-line `;` comment must all
        // be skipped without affecting the replayed mainline.
        let pgn = "[Result \"1-0\"]\n\n\
            1. e4 {good {nested} move} $1 e5 \
            (1... c5 {Sicilian} 2. Nf3) \
            2. Nf3 ; trailing comment\n\
            Nc6 1-0\n";
        let mut plies = 0usize;
        let mut stats = PgnStats::default();
        stream_pgn(
            Cursor::new(pgn),
            0,
            &mut |g| plies = g.positions.len(),
            &mut stats,
        )
        .expect("stream");
        // Start position + 3 mainline plies (e4, e5, Nf3, Nc6 = 4 moves)
        // → 5 positions. (e4 e5 Nf3 Nc6.)
        assert_eq!(stats.games_emitted, 1);
        assert_eq!(stats.parse_failures, 0);
        assert_eq!(plies, 5, "only the 4 mainline moves are replayed");
    }

    #[test]
    fn truncated_game_errors_clean() {
        // An unterminated comment is structurally broken: the whole game
        // is dropped, parse_failures += 1, and the call does NOT panic or
        // error out of the stream.
        let pgn = "[Result \"1-0\"]\n\n1. e4 e5 2. Nf3 {unterminated\n";
        let mut emitted = 0;
        let mut stats = PgnStats::default();
        let r = stream_pgn(Cursor::new(pgn), 0, &mut |_| emitted += 1, &mut stats);
        assert!(r.is_ok(), "a broken game must not abort the stream");
        assert_eq!(emitted, 0);
        assert_eq!(stats.parse_failures, 1);

        // An unterminated variation likewise.
        let pgn2 = "[Result \"1-0\"]\n\n1. e4 e5 (1... c5 2. Nf3 1-0\n";
        let mut s2 = PgnStats::default();
        stream_pgn(Cursor::new(pgn2), 0, &mut |_| {}, &mut s2).expect("stream");
        assert_eq!(s2.parse_failures, 1);
    }

    #[test]
    fn replay_known_ccrl_game_reaches_expected_fen() {
        // Scholar's Mate — a canonical short decisive game. CCRL-shaped
        // tag roster (engine names, Elo, TC, Termination). The expected
        // final FEN is hand-traced: after 4. Qxf7# the white queen sits
        // on f7 (capturing the black f7 pawn), Black is checkmated, it is
        // Black to move, fullmove 4.
        let pgn = "[Event \"CCRL 40/15\"]\n\
            [White \"Engine A 1.0 64-bit\"]\n\
            [Black \"Engine B 2.3 64-bit\"]\n\
            [Result \"1-0\"]\n\
            [WhiteElo \"3200\"]\n\
            [BlackElo \"3180\"]\n\
            [TimeControl \"40/15\"]\n\
            [Termination \"Normal\"]\n\n\
            1. e4 e5 2. Bc4 Nc6 3. Qh5 Nf6 4. Qxf7# 1-0\n";
        let mut final_fen = String::new();
        let mut tags = PgnTags::default();
        let mut stats = PgnStats::default();
        stream_pgn(
            Cursor::new(pgn),
            7,
            &mut |g| {
                tags = g.tags.clone();
                final_fen = g.positions.last().unwrap().0.to_fen();
                assert_eq!(g.game_id, 7, "base_game_id is honored");
                // ply indices are 0..=N contiguous.
                for (i, (_, ply)) in g.positions.iter().enumerate() {
                    assert_eq!(*ply as usize, i);
                }
            },
            &mut stats,
        )
        .expect("stream");
        assert_eq!(stats.games_emitted, 1);
        // Hand-verified expected position after 1.e4 e5 2.Bc4 Nc6 3.Qh5
        // Nf6 4.Qxf7#:
        assert_eq!(
            final_fen,
            "r1bqkb1r/pppp1Qpp/2n2n2/4p3/2B1P3/8/PPPP1PPP/RNB1K1NR b KQkq - 0 4"
        );
        assert_eq!(tags.result, Some(Label::WhiteWin));
        assert_eq!(tags.white_elo, Some(3200));
        assert_eq!(tags.black_elo, Some(3180));
        assert_eq!(tags.termination.as_deref(), Some("Normal"));
        assert_eq!(tags.time_control.as_deref(), Some("40/15"));
    }

    #[test]
    fn replay_known_lichess_game_reaches_expected_fen() {
        // Légal's Mate (the "Légal Trap"), a classic miniature that
        // exercises piece moves, a queen sacrifice capture (`Bxd1`),
        // a check (`Bxf7+`), and a knight mate (`Nd5#`). Lichess-shaped
        // tag roster. Final FEN hand-traced below.
        //
        //  1. e4 e5  2. Bc4 d6  3. Nf3 Bg4  4. Nc3 g6  5. Nxe5 Bxd1
        //  6. Bxf7+ Ke7  7. Nd5#
        let pgn = "[Event \"Rated Classical game\"]\n\
            [Site \"https://lichess.org/abcd1234\"]\n\
            [White \"alice\"]\n\
            [Black \"bob\"]\n\
            [Result \"1-0\"]\n\
            [WhiteElo \"2210\"]\n\
            [BlackElo \"2185\"]\n\
            [TimeControl \"600+5\"]\n\
            [Termination \"Normal\"]\n\n\
            1. e4 e5 2. Bc4 d6 3. Nf3 Bg4 4. Nc3 g6 \
            5. Nxe5 Bxd1 6. Bxf7+ Ke7 7. Nd5# 1-0\n";
        let mut final_fen = String::new();
        let mut stats = PgnStats::default();
        stream_pgn(
            Cursor::new(pgn),
            0,
            &mut |g| final_fen = g.positions.last().unwrap().0.to_fen(),
            &mut stats,
        )
        .expect("stream");
        assert_eq!(stats.games_emitted, 1);
        // Hand-verified final position after 7. Nd5# (independently
        // confirmed via the trusted `Move::from_uci` replay of the
        // hand-traced UCI line). Note `3NN3` — BOTH the e5 knight (5.
        // Nxe5) and the d5 knight (7. Nd5#) are on the board, and the
        // sacrificed black bishop survives on d1 (`R1BbK2R`).
        assert_eq!(
            final_fen,
            "rn1q1bnr/ppp1kB1p/3p2p1/3NN3/4P3/8/PPPP1PPP/R1BbK2R b KQ - 2 7"
        );
    }

    // -----------------------------------------------------------------------
    // Property test (proptest) — the internal-consistency complement to the
    // golden-FEN tests: every SAN we emit for a position resolves to a move
    // that is actually in that position's legal set. We generate SAN by
    // round-tripping legal moves through a minimal SAN writer, then assert
    // `parse_san` recovers a move in the legal set with the same
    // (from,to,promo). This is NOT a substitute for the golden-FEN games
    // (which pin disambiguation *correctness*); it guards the never-emit-an
    // -illegal-move invariant across a wide position space.
    // -----------------------------------------------------------------------

    use crate::movegen::test_strategies::arb_position;
    use proptest::prelude::*;

    /// Minimal SAN writer: enough to round-trip every legal move through
    /// `parse_san`. Always emits FULL file+rank disambiguation for non-pawn
    /// non-king moves (always unambiguous — a superset of what real PGN
    /// writes, and `parse_san` must accept it).
    fn write_san(pos: &Position, mv: Move) -> String {
        if mv.flag() == crate::MoveFlag::KingCastle {
            return "O-O".into();
        }
        if mv.flag() == crate::MoveFlag::QueenCastle {
            return "O-O-O".into();
        }
        let from = mv.from_square();
        let to = mv.to_square();
        let kind = pos.piece_at(from).expect("from must have a piece").kind;
        let mut s = String::new();
        if kind == PieceKind::Pawn {
            if mv.is_capture() {
                s.push((b'a' + from.file()) as char);
                s.push('x');
            }
            s.push_str(&to.to_string());
            if let Some(pk) = mv.promotion_kind() {
                s.push('=');
                s.push((pk.san_letter() as char).to_ascii_uppercase());
            }
        } else {
            s.push((kind.san_letter() as char).to_ascii_uppercase());
            // Full disambiguation (file then rank) — always unambiguous.
            s.push((b'a' + from.file()) as char);
            s.push((b'1' + from.rank()) as char);
            if mv.is_capture() {
                s.push('x');
            }
            s.push_str(&to.to_string());
        }
        s
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        #[test]
        fn parsed_move_is_in_legal_set(pos in arb_position()) {
            let mut legal = MoveList::new();
            generate_moves(&pos, &mut legal);
            for mv in legal.iter() {
                let san = write_san(&pos, mv);
                let parsed = parse_san(&san, &pos, &legal)
                    .expect("a SAN we wrote for a legal move must parse");
                // The parsed move must itself be in the legal set and
                // have the same (from, to, promotion).
                prop_assert!(
                    legal.iter().any(|m| m == parsed),
                    "parsed move {parsed:?} not in legal set for SAN {san:?}"
                );
                prop_assert_eq!(parsed.from_square(), mv.from_square());
                prop_assert_eq!(parsed.to_square(), mv.to_square());
                prop_assert_eq!(parsed.promotion_kind(), mv.promotion_kind());
            }
        }
    }
}
