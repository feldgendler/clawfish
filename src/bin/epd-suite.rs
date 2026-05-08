//! EPD diagnostic suites harness — WAC and STS best-move scorer.
//!
//! Drives the engine over UCI to score Win-at-Chess (WAC) and Strategic Test
//! Suite (STS) positions deterministically. Each position is searched at a
//! fixed move time; the engine's best move is converted to SAN and compared
//! against the EPD annotation. Results are aggregated into a summary with
//! per-theme breakdown for STS.
//!
//! # Usage
//!
//! ```text
//! epd-suite --engine <path> --suite <wac|sts> --epd <path> [options]
//! ```

// ---------------------------------------------------------------------------
// CLI parsing
// ---------------------------------------------------------------------------

mod cli {
    //! Command-line argument parsing.

    /// Parsed and validated command-line configuration.
    pub(crate) struct Config {
        /// Path to the engine binary.
        pub engine: String,
        /// Which suite we are scoring.
        pub suite: Suite,
        /// Path to the EPD data file.
        pub epd: String,
        /// Per-position move time in milliseconds.
        pub movetime_ms: u64,
        /// Transposition-table size in MiB passed to the engine.
        pub hash_mib: u32,
        /// Number of parallel engine workers.
        pub concurrency: usize,
        /// If set, only the first `limit` positions are run.
        pub limit: Option<usize>,
        /// Optional path to write the summary in addition to stdout.
        pub output: Option<String>,
    }

    /// Which suite to score.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(crate) enum Suite {
        /// Win at Chess — 300 tactical positions.
        Wac,
        /// Strategic Test Suite — 1500 themed positions.
        Sts,
    }

    /// Parse `std::env::args()`. Returns `Ok(Config)` or `Err(message)`.
    pub(crate) fn parse(args: &[String]) -> Result<Config, String> {
        let mut engine: Option<String> = None;
        let mut suite: Option<Suite> = None;
        let mut epd: Option<String> = None;
        let mut movetime_ms: u64 = 1000;
        let mut hash_mib: u32 = 16;
        let mut concurrency: usize = 1;
        let mut limit: Option<usize> = None;
        let mut output: Option<String> = None;

        let mut i = 0usize;
        while i < args.len() {
            match args[i].as_str() {
                "--help" | "-h" => {
                    return Err(usage());
                }
                "--engine" => {
                    i += 1;
                    engine = Some(next_arg(args, i, "--engine")?);
                }
                "--suite" => {
                    i += 1;
                    let s = next_arg(args, i, "--suite")?;
                    suite = Some(match s.as_str() {
                        "wac" => Suite::Wac,
                        "sts" => Suite::Sts,
                        other => {
                            return Err(format!("--suite must be 'wac' or 'sts', got {other:?}"));
                        }
                    });
                }
                "--epd" => {
                    i += 1;
                    epd = Some(next_arg(args, i, "--epd")?);
                }
                "--movetime" => {
                    i += 1;
                    let s = next_arg(args, i, "--movetime")?;
                    movetime_ms = s.parse::<u64>().ok().filter(|&v| v > 0).ok_or_else(|| {
                        format!("--movetime must be a positive integer, got {s:?}")
                    })?;
                }
                "--hash" => {
                    i += 1;
                    let s = next_arg(args, i, "--hash")?;
                    hash_mib =
                        s.parse::<u32>().ok().filter(|&v| v > 0).ok_or_else(|| {
                            format!("--hash must be a positive integer, got {s:?}")
                        })?;
                }
                "--concurrency" => {
                    i += 1;
                    let s = next_arg(args, i, "--concurrency")?;
                    concurrency = s.parse::<usize>().ok().filter(|&v| v > 0).ok_or_else(|| {
                        format!("--concurrency must be a positive integer, got {s:?}")
                    })?;
                }
                "--limit" => {
                    i += 1;
                    let s = next_arg(args, i, "--limit")?;
                    limit =
                        Some(s.parse::<usize>().ok().filter(|&v| v > 0).ok_or_else(|| {
                            format!("--limit must be a positive integer, got {s:?}")
                        })?);
                }
                "--output" => {
                    i += 1;
                    output = Some(next_arg(args, i, "--output")?);
                }
                other => {
                    return Err(format!("unknown argument {other:?}; try --help"));
                }
            }
            i += 1;
        }

        let engine = engine.ok_or_else(|| "--engine is required".to_string())?;
        let suite = suite.ok_or_else(|| "--suite is required".to_string())?;
        let epd = epd.ok_or_else(|| "--epd is required".to_string())?;

        if !std::path::Path::new(&engine).exists() {
            return Err(format!("engine binary not found: {engine:?}"));
        }
        if !std::path::Path::new(&epd).exists() {
            return Err(format!("EPD file not found: {epd:?}"));
        }

        Ok(Config {
            engine,
            suite,
            epd,
            movetime_ms,
            hash_mib,
            concurrency,
            limit,
            output,
        })
    }

    fn next_arg(args: &[String], i: usize, flag: &str) -> Result<String, String> {
        args.get(i)
            .cloned()
            .ok_or_else(|| format!("{flag} requires a value"))
    }

    fn usage() -> String {
        "\
Usage: epd-suite --engine <path> --suite <wac|sts> --epd <path> [options]

Options:
  --engine <path>       Engine binary path (required)
  --suite <wac|sts>     Which suite to score (required)
  --epd <path>          EPD data file path (required)
  --movetime <ms>       Per-position move time in ms (default: 1000)
  --hash <MiB>          Engine hash table size (default: 16)
  --concurrency <N>     Parallel engine workers (default: 1)
  --limit <N>           Only run the first N positions
  --output <path>       Write summary to file in addition to stdout
  --help, -h            Show this help"
            .to_string()
    }
}

// ---------------------------------------------------------------------------
// EPD parsing
// ---------------------------------------------------------------------------

mod epd {
    //! EPD (Extended Position Description) parser.
    //!
    //! Supports `bm`, `c0`, and `id` opcodes. Handles both WAC and STS formats.
    //! Quote-aware tokenization: `"..."` strings span across `;` characters.

    use clawfish::{FenError, Position};

    /// A parsed EPD entry.
    pub(crate) struct EpdEntry {
        /// The FEN string as written (4 or 6 fields).
        pub fen: String,
        /// The parsed position.
        pub position: Position,
        /// Best-move SAN candidates from the `bm` opcode (raw, unstripped).
        pub bm: Vec<String>,
        /// Weighted move list from the `c0` opcode (STS only). SANs are raw (unstripped).
        pub c0: Option<Vec<(String, u32)>>,
        /// Position identifier from the `id` opcode.
        pub id: Option<String>,
    }

    /// Errors that can occur when parsing an EPD line.
    #[derive(Debug)]
    pub(crate) enum EpdParseError {
        /// The FEN portion of the line could not be parsed.
        BadFen(FenError),
        /// No `bm` opcode was found.
        MissingBm,
        /// The line was empty or a comment.
        EmptyLine,
    }

    impl std::fmt::Display for EpdParseError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                EpdParseError::BadFen(e) => write!(f, "bad FEN: {e:?}"),
                EpdParseError::MissingBm => write!(f, "missing 'bm' opcode"),
                EpdParseError::EmptyLine => write!(f, "empty line"),
            }
        }
    }

    // -----------------------------------------------------------------------
    // Quote-aware tokenizer
    // -----------------------------------------------------------------------

    /// A token from the EPD opcode section.
    #[derive(Debug, PartialEq)]
    enum Token {
        /// A bare word (no surrounding quotes).
        Word(String),
        /// A quoted string (surrounding quotes removed, `\"` and `\\` unescaped).
        Quoted(String),
        /// A semicolon terminator.
        Semi,
    }

    /// Tokenize the opcode section of an EPD line.
    ///
    /// Handles quoted strings so that `;` inside `"..."` is treated as content,
    /// not as a terminator. `\"` and `\\` are unescaped inside strings; other
    /// backslash sequences are passed through verbatim.
    fn tokenize_opcodes(text: &str) -> Vec<Token> {
        let mut tokens = Vec::new();
        let chars: Vec<char> = text.chars().collect();
        let mut i = 0;

        while i < chars.len() {
            match chars[i] {
                // Skip ASCII whitespace between tokens.
                c if c.is_ascii_whitespace() => {
                    i += 1;
                }
                ';' => {
                    tokens.push(Token::Semi);
                    i += 1;
                }
                '"' => {
                    // Quoted string: consume until matching unescaped `"`.
                    i += 1; // skip opening quote
                    let mut s = String::new();
                    while i < chars.len() && chars[i] != '"' {
                        if chars[i] == '\\' && i + 1 < chars.len() {
                            match chars[i + 1] {
                                '"' => {
                                    s.push('"');
                                    i += 2;
                                }
                                '\\' => {
                                    s.push('\\');
                                    i += 2;
                                }
                                _ => {
                                    // Pass through other backslash sequences.
                                    s.push(chars[i]);
                                    s.push(chars[i + 1]);
                                    i += 2;
                                }
                            }
                        } else {
                            s.push(chars[i]);
                            i += 1;
                        }
                    }
                    if i < chars.len() {
                        i += 1; // skip closing quote
                    }
                    tokens.push(Token::Quoted(s));
                }
                _ => {
                    // Bare word: consume until whitespace, `;`, or `"`.
                    let start = i;
                    while i < chars.len()
                        && !chars[i].is_ascii_whitespace()
                        && chars[i] != ';'
                        && chars[i] != '"'
                    {
                        i += 1;
                    }
                    let word: String = chars[start..i].iter().collect();
                    tokens.push(Token::Word(word));
                }
            }
        }
        tokens
    }

    // -----------------------------------------------------------------------
    // EPD line parser
    // -----------------------------------------------------------------------

    /// Parse a single EPD line.
    ///
    /// Returns `Err(EpdParseError::EmptyLine)` for blank lines and lines starting with `#`.
    /// (The `Result<Option<EpdEntry>, _>` shape is preserved historically; the `Some` case is
    /// always taken for valid entries.)
    pub(crate) fn parse_epd_line(line: &str) -> Result<Option<EpdEntry>, EpdParseError> {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            return Err(EpdParseError::EmptyLine);
        }

        // The first four space-separated tokens are the FEN base fields:
        // piece-placement stm castling ep
        // EPD may also include halfmove + fullmove as tokens 5 and 6.
        let mut parts = line.splitn(10, ' ');
        let piece_placement = parts.next().unwrap_or("");
        let stm = parts.next().unwrap_or("");
        let castling = parts.next().unwrap_or("");
        let ep = parts.next().unwrap_or("");

        let rest_fields: Vec<&str> = parts.collect();

        // Decide whether tokens 5 and 6 are halfmove/fullmove (both all-digit)
        // or the start of opcodes.
        let (fen_str, opcode_text) = if rest_fields.len() >= 2
            && rest_fields[0].chars().all(|c| c.is_ascii_digit())
            && rest_fields[1].chars().all(|c| c.is_ascii_digit())
        {
            let half = rest_fields[0];
            let full = rest_fields[1];
            let fen = format!("{piece_placement} {stm} {castling} {ep} {half} {full}");
            let remaining = rest_fields[2..].join(" ");
            (fen, remaining)
        } else {
            let fen = format!("{piece_placement} {stm} {castling} {ep} 0 1");
            (fen, rest_fields.join(" "))
        };

        let position = Position::from_fen(&fen_str).map_err(EpdParseError::BadFen)?;

        // Parse the opcode section using the quote-aware tokenizer.
        let tokens = tokenize_opcodes(&opcode_text);
        let mut bm: Option<Vec<String>> = None;
        let mut c0: Option<Vec<(String, u32)>> = None;
        let mut id: Option<String> = None;

        parse_opcode_tokens(&tokens, &mut bm, &mut c0, &mut id)?;

        let bm = bm.ok_or(EpdParseError::MissingBm)?;

        Ok(Some(EpdEntry {
            fen: fen_str,
            position,
            bm,
            c0,
            id,
        }))
    }

    /// Process the token stream, filling in bm/c0/id.
    fn parse_opcode_tokens(
        tokens: &[Token],
        bm: &mut Option<Vec<String>>,
        c0: &mut Option<Vec<(String, u32)>>,
        id: &mut Option<String>,
    ) -> Result<(), EpdParseError> {
        let mut i = 0;
        while i < tokens.len() {
            // Expect a Word token as the opcode keyword.
            let opcode = match &tokens[i] {
                Token::Word(w) => w.clone(),
                Token::Semi => {
                    i += 1;
                    continue;
                }
                Token::Quoted(_) => {
                    // Stray quoted token without an opcode; skip.
                    i += 1;
                    continue;
                }
            };
            i += 1;

            // Collect the value tokens up to the next `;`.
            let mut value_tokens: Vec<&Token> = Vec::new();
            while i < tokens.len() && tokens[i] != Token::Semi {
                value_tokens.push(&tokens[i]);
                i += 1;
            }
            // Consume the `;` if present.
            if i < tokens.len() && tokens[i] == Token::Semi {
                i += 1;
            }

            match opcode.as_str() {
                "bm" => {
                    // `bm` operand: bare word tokens (SAN moves) separated by whitespace.
                    let moves: Vec<String> = value_tokens
                        .iter()
                        .filter_map(|t| match t {
                            Token::Word(w) => Some(w.clone()),
                            _ => None,
                        })
                        .collect();
                    *bm = Some(moves);
                }
                "id" => {
                    // `id` operand: a single quoted string or bare word.
                    let val = match value_tokens.first() {
                        Some(Token::Quoted(s)) => s.clone(),
                        Some(Token::Word(w)) => w.clone(),
                        _ => String::new(),
                    };
                    *id = Some(val);
                }
                "c0" => {
                    // `c0` operand: a single quoted string containing comma-separated `san=weight` pairs.
                    let val = match value_tokens.first() {
                        Some(Token::Quoted(s)) => s.clone(),
                        Some(Token::Word(w)) => w.clone(),
                        _ => String::new(),
                    };
                    *c0 = Some(parse_c0_string(&val));
                }
                _ => {
                    // Skip unknown opcodes silently.
                }
            }
        }
        Ok(())
    }

    /// Parse the inner content of a `c0` quoted string.
    ///
    /// Format: `Bf8=10, Bxd5=2, Be8=2, Bxc6=2` (comma-separated `san=int` pairs).
    /// Uses `rfind('=')` to handle promotions like `a8=Q=10`.
    fn parse_c0_string(s: &str) -> Vec<(String, u32)> {
        let mut result = Vec::new();
        for entry in s.split(',') {
            let entry = entry.trim();
            if entry.is_empty() {
                continue;
            }
            // Use rfind to split at the last `=` so SAN promotions like `a8=Q` keep their `=Q`.
            if let Some(eq_idx) = entry.rfind('=') {
                let san = entry[..eq_idx].trim().to_string();
                let weight_str = entry[eq_idx + 1..].trim();
                if let Ok(weight) = weight_str.parse::<u32>()
                    && !san.is_empty()
                {
                    result.push((san, weight));
                }
                // Silently skip malformed weight entries.
            }
            // Entries without `=` are silently ignored.
        }
        result
    }

    /// Parse every non-blank, non-comment line of an EPD file.
    pub(crate) fn parse_epd_file(text: &str) -> Vec<Result<EpdEntry, EpdParseError>> {
        text.lines()
            .filter_map(|line| match parse_epd_line(line) {
                Err(EpdParseError::EmptyLine) => None,
                Ok(None) => None,
                Ok(Some(entry)) => Some(Ok(entry)),
                Err(e) => Some(Err(e)),
            })
            .collect()
    }

    /// Extract the STS theme number and theme name from an `id` string.
    ///
    /// Accepts patterns like `"STS(v1.0) Undermine.001"` and
    /// `"STS(v2.2) Open Files and Diagonals.001"`.
    ///
    /// Returns `Some((theme_num, theme_name))` on success, `None` otherwise.
    pub(crate) fn parse_sts_id(id: &str) -> Option<(u32, String)> {
        // Strip "STS(v" prefix — the `v` is optional per some distributions,
        // but all observed STS files use it.
        let rest = id
            .strip_prefix("STS(v")
            .or_else(|| id.strip_prefix("STS("))?;

        // Major version number up to the first `.` inside the parens.
        let dot_idx = rest.find('.')?;
        let major_str = &rest[..dot_idx];
        let theme_num: u32 = major_str.parse().ok()?;

        // Skip to closing `)`.
        let close_paren = rest.find(')')?;
        let after_paren = &rest[close_paren + 1..];

        // Theme name comes after optional `: ` or ` ` separator.
        let name_part = after_paren.trim_start_matches(':').trim_start();

        // The name ends at the LAST `.`, which separates name from position number.
        let last_dot = name_part.rfind('.')?;
        let theme_name = name_part[..last_dot].trim().to_string();

        if theme_name.is_empty() {
            return None;
        }
        Some((theme_num, theme_name))
    }
}

// ---------------------------------------------------------------------------
// SAN renderer
// ---------------------------------------------------------------------------

mod san {
    //! SAN (Standard Algebraic Notation) renderer for legal moves.
    //!
    //! We render SAN for every legal move and compare against EPD annotations.
    //! SAN parsing is not needed — we only compare canonical forms.

    use clawfish::{Move, MoveFlag, MoveList, PieceKind, Position, generate_moves};

    /// Render a legal move in canonical SAN, given the position before the move.
    ///
    /// No check or mate suffix is appended; the comparator applies
    /// [`canonicalize_san`] to both sides before comparison.
    pub(crate) fn san_of_legal_move(pos: &Position, mv: Move) -> String {
        let flag = mv.flag();
        let from = mv.from_square();
        let to = mv.to_square();

        if flag == MoveFlag::KingCastle {
            return "O-O".to_string();
        }
        if flag == MoveFlag::QueenCastle {
            return "O-O-O".to_string();
        }

        let moving_piece = pos
            .piece_at(from)
            .expect("legal move must have a piece on from-square");
        let is_pawn = moving_piece.kind == PieceKind::Pawn;
        let is_capture = mv.is_capture();

        let mut s = String::with_capacity(8);

        if is_pawn {
            if is_capture {
                s.push((b'a' + from.file()) as char);
                s.push('x');
            }
            s.push((b'a' + to.file()) as char);
            s.push((b'1' + to.rank()) as char);
            if let Some(promo_kind) = mv.promotion_kind() {
                s.push('=');
                s.push(piece_kind_letter(promo_kind));
            }
        } else {
            s.push(piece_kind_letter(moving_piece.kind));
            s.push_str(&disambig(pos, mv));
            if is_capture {
                s.push('x');
            }
            s.push((b'a' + to.file()) as char);
            s.push((b'1' + to.rank()) as char);
        }

        s
    }

    /// Compute the minimal disambiguation string for a piece move.
    ///
    /// Uses `generate_moves` (legal moves only), so pinned same-kind pieces
    /// are automatically excluded — the disambiguation is always correct.
    fn disambig(pos: &Position, mv: Move) -> String {
        let from = mv.from_square();
        let to = mv.to_square();
        let moving_piece = pos.piece_at(from).expect("piece on from-square");
        let us = pos.side_to_move();

        let mut ml = MoveList::new();
        generate_moves(pos, &mut ml);

        let ambiguous: Vec<Move> = ml
            .as_slice()
            .iter()
            .copied()
            .filter(|&m| {
                m != mv
                    && m.to_square() == to
                    && !m.is_castling()
                    && pos
                        .piece_at(m.from_square())
                        .map(|p| p.kind == moving_piece.kind && p.color == us)
                        .unwrap_or(false)
            })
            .collect();

        if ambiguous.is_empty() {
            return String::new();
        }

        let same_file = ambiguous
            .iter()
            .any(|m| m.from_square().file() == from.file());
        let same_rank = ambiguous
            .iter()
            .any(|m| m.from_square().rank() == from.rank());

        if !same_file {
            return format!("{}", (b'a' + from.file()) as char);
        }
        if !same_rank {
            return format!("{}", (b'1' + from.rank()) as char);
        }
        // Need full square to uniquely identify.
        format!(
            "{}{}",
            (b'a' + from.file()) as char,
            (b'1' + from.rank()) as char
        )
    }

    /// Uppercase SAN piece letter. Panics on `Pawn` — callers guard this.
    fn piece_kind_letter(kind: PieceKind) -> char {
        match kind {
            PieceKind::Knight => 'N',
            PieceKind::Bishop => 'B',
            PieceKind::Rook => 'R',
            PieceKind::Queen => 'Q',
            PieceKind::King => 'K',
            PieceKind::Pawn => unreachable!("pawn SAN does not use a piece letter"),
        }
    }

    /// Strip trailing `+`, `#`, `?`, `!` and surrounding whitespace.
    pub(crate) fn strip_san_decoration(s: &str) -> &str {
        let mut s = s.trim();
        loop {
            let stripped = s.trim_end_matches(['+', '#', '?', '!']);
            if stripped.len() == s.len() {
                break;
            }
            s = stripped;
        }
        s
    }

    /// Canonical SAN form for comparison purposes.
    ///
    /// - Strips surrounding whitespace.
    /// - Strips trailing `+`, `#`, `?`, `!` decorators (repeating).
    /// - Normalizes `0-0` → `O-O` and `0-0-0` → `O-O-O`.
    /// - Strips an `e.p.` suffix if present.
    pub(crate) fn canonicalize_san(s: &str) -> String {
        let s = strip_san_decoration(s);
        // Strip trailing "e.p." suffix.
        let s = s.trim_end_matches("e.p.").trim_end();
        // Normalize castling notation.
        match s {
            "0-0-0" | "O-O-O" => "O-O-O".to_string(),
            "0-0" | "O-O" => "O-O".to_string(),
            other => other.to_string(),
        }
    }

    /// Find the unique legal move whose canonical SAN matches `san_target`.
    ///
    /// Handles both standard SAN (`Nf3`, `Bf8`) and over-qualified SAN from some
    /// EPD distributors (`Bg7f8`, `Nbd2`). First attempts exact canonical-SAN match;
    /// on failure, falls back to `parse_overqualified_san` which extracts
    /// `(piece, from, to)` triples from longer forms.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn legal_move_from_san(pos: &Position, san_target: &str) -> Option<Move> {
        let target = canonicalize_san(san_target);
        let mut ml = MoveList::new();
        generate_moves(pos, &mut ml);

        // First pass: exact canonical-SAN match.
        let exact = ml
            .as_slice()
            .iter()
            .copied()
            .find(|&mv| canonicalize_san(&san_of_legal_move(pos, mv)) == target);
        if exact.is_some() {
            return exact;
        }

        // Second pass: over-qualified SAN (e.g., `Bg7f8` instead of `Bf8`).
        // Parse the from-square and to-square from the longer form.
        let (kind, from_sq, to_sq) = parse_overqualified_san(&target)?;
        ml.as_slice().iter().copied().find(|&mv| {
            mv.from_square() == from_sq
                && mv.to_square() == to_sq
                && pos
                    .piece_at(mv.from_square())
                    .map(|p| p.kind == kind)
                    .unwrap_or(false)
        })
    }

    /// Parse an over-qualified SAN like `Bg7f8` or `Nbd2` into `(PieceKind, from_sq, to_sq)`.
    ///
    /// Expected patterns (after decoration stripping):
    /// - `<P><file><rank><file><rank>` — piece letter + from-square + to-square (5 chars total)
    ///   e.g. `Bg7f8` or `Rc1c4`
    ///
    /// Returns `None` if the string doesn't fit this pattern.
    fn parse_overqualified_san(
        san: &str,
    ) -> Option<(clawfish::PieceKind, clawfish::Square, clawfish::Square)> {
        use clawfish::{PieceKind, Square};

        let bytes = san.as_bytes();
        // Remove leading `x` capture indicator if present in unusual positions.
        // Form: P<f><r><f><r> where P is a piece letter (5 chars)
        //    or P<f><r>x<f><r> (6 chars with capture x)
        let (kind_byte, rest) = bytes.split_first()?;
        let kind = match kind_byte {
            b'N' => PieceKind::Knight,
            b'B' => PieceKind::Bishop,
            b'R' => PieceKind::Rook,
            b'Q' => PieceKind::Queen,
            b'K' => PieceKind::King,
            _ => return None, // Pawn or garbage
        };

        // Strip a leading `x` if present after the piece letter.
        let rest = if rest.first() == Some(&b'x') {
            &rest[1..]
        } else {
            rest
        };

        // From-square: 2 chars (file + rank).
        if rest.len() < 4 {
            return None;
        }
        let from_str = std::str::from_utf8(&rest[..2]).ok()?;
        let from_sq = Square::parse_uci(from_str)?;

        // Skip optional `x` capture marker between from and to.
        let rest = if rest.len() > 2 && rest[2] == b'x' {
            &rest[3..]
        } else {
            &rest[2..]
        };

        if rest.len() < 2 {
            return None;
        }
        let to_str = std::str::from_utf8(&rest[..2]).ok()?;
        let to_sq = Square::parse_uci(to_str)?;

        Some((kind, from_sq, to_sq))
    }
}

// ---------------------------------------------------------------------------
// Scorer
// ---------------------------------------------------------------------------

mod scorer {
    //! WAC and STS scoring.
    //!
    //! Both scorers compare the engine's move against EPD annotations via SAN.
    //! Crucially, the c0/bm SANs from EPD may include over-qualified disambiguation
    //! (e.g., `Bg7f8` instead of `Bf8`). We normalize ALL SAN strings — both the
    //! engine's output and the EPD annotations — through the position: find the
    //! legal move corresponding to each SAN, then re-render via `san_of_legal_move`.
    //! This makes disambiguation mismatches transparent.

    use crate::epd::EpdEntry;
    use crate::san::{canonicalize_san, legal_move_from_san, san_of_legal_move};
    use clawfish::{Move, Position};

    /// The scoring result for a single position.
    pub(crate) struct ScoringResult {
        /// Credit earned (WAC: 0 or 1; STS: 0–max).
        pub credit: u32,
        /// Maximum possible credit (WAC: 1; STS: max weight in c0).
        pub max_credit: u32,
    }

    /// Score a WAC position.
    pub(crate) fn score_wac(entry: &EpdEntry, engine_uci: &str) -> ScoringResult {
        let engine_san = uci_to_normalized_san(&entry.position, engine_uci);
        let credit = if engine_san
            .as_deref()
            .map(|s| bm_contains(&entry.position, &entry.bm, s))
            .unwrap_or(false)
        {
            1
        } else {
            0
        };
        ScoringResult {
            credit,
            max_credit: 1,
        }
    }

    /// Score an STS position.
    pub(crate) fn score_sts(entry: &EpdEntry, engine_uci: &str) -> ScoringResult {
        let c0 = match &entry.c0 {
            Some(c) => c,
            None => {
                return ScoringResult {
                    credit: 0,
                    max_credit: 1,
                };
            }
        };

        let max_credit = c0.iter().map(|(_, w)| *w).max().unwrap_or(1);

        let engine_san = uci_to_normalized_san(&entry.position, engine_uci);
        let credit = engine_san
            .as_deref()
            .and_then(|s| find_weight(&entry.position, c0, s))
            .unwrap_or(0);

        ScoringResult { credit, max_credit }
    }

    /// Convert a UCI move to normalized SAN via `san_of_legal_move`.
    ///
    /// Returns `None` for `"0000"` (null move) or any invalid UCI string.
    fn uci_to_normalized_san(pos: &Position, uci: &str) -> Option<String> {
        if uci == "0000" {
            return None;
        }
        let mv = Move::from_uci(uci, pos).ok()?;
        Some(canonicalize_san(&san_of_legal_move(pos, mv)))
    }

    /// Normalize a SAN string through the position: find the legal move, re-render.
    ///
    /// This handles over-qualified SAN like `Bg7f8` → `Bf8`. Falls back to
    /// `canonicalize_san` on the raw string if no legal move matches (e.g., for
    /// free-text c0 comments in WAC).
    fn normalize_epd_san(pos: &Position, san: &str) -> String {
        let stripped = canonicalize_san(san);
        if let Some(mv) = legal_move_from_san(pos, &stripped) {
            canonicalize_san(&san_of_legal_move(pos, mv))
        } else {
            stripped
        }
    }

    /// Check if `engine_san` (normalized) matches any `bm` entry (also normalized).
    fn bm_contains(pos: &Position, bm: &[String], engine_san: &str) -> bool {
        bm.iter().any(|b| normalize_epd_san(pos, b) == engine_san)
    }

    /// Find the weight for `engine_san` (normalized) in the c0 list.
    fn find_weight(pos: &Position, c0: &[(String, u32)], engine_san: &str) -> Option<u32> {
        c0.iter()
            .find(|(san, _)| normalize_epd_san(pos, san) == engine_san)
            .map(|(_, w)| *w)
    }
}

// ---------------------------------------------------------------------------
// UCI subprocess driver
// ---------------------------------------------------------------------------

mod driver {
    //! Minimal UCI subprocess driver.
    //!
    //! Sends `position fen <FEN>` + `go movetime <T>`, waits for `bestmove`.
    //! Each `EngineDriver` owns one subprocess with a reader thread draining
    //! stdout into an `mpsc::sync_channel`.

    use std::io::{self, BufRead, BufReader, Write};
    use std::process::{Child, ChildStdin, Command, Stdio};
    use std::sync::mpsc::{self, Receiver};
    use std::thread::JoinHandle;
    use std::time::{Duration, Instant};

    /// Sentinel line sent by the reader thread when stdout closes.
    const EOF_SENTINEL: &str = "\x00EOF\x00";

    /// A live UCI engine subprocess with a background reader thread.
    pub(crate) struct EngineDriver {
        pub(crate) child: Child,
        pub(crate) stdin: Option<ChildStdin>,
        pub(crate) rx: Receiver<String>,
        pub(crate) reader: Option<JoinHandle<()>>,
    }

    impl EngineDriver {
        /// Like `spawn`, but also injects `extra_env` into the child process.
        ///
        /// `extra_env` is a list of `(key, value)` pairs to add to the child's
        /// environment; used in tests to inject `MOCK_ENGINE_RECORD_PATH` without
        /// mutating the calling process's environment. Pass `&[]` for a standard spawn.
        pub(crate) fn spawn_with_env(
            path: &str,
            hash_mib: u32,
            extra_env: &[(&str, &str)],
        ) -> io::Result<Self> {
            let mut cmd = Command::new(path);
            cmd.stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null());
            for (k, v) in extra_env {
                cmd.env(k, v);
            }
            let mut child = cmd.spawn()?;

            let stdin = child.stdin.take().expect("stdin piped");
            let stdout = child.stdout.take().expect("stdout piped");

            let (tx, rx) = mpsc::sync_channel::<String>(1024);
            let reader = std::thread::spawn(move || {
                for line in BufReader::new(stdout).lines() {
                    match line {
                        Ok(l) => {
                            if tx
                                .send(l.trim_end_matches(['\r', '\n']).to_string())
                                .is_err()
                            {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
                let _ = tx.send(EOF_SENTINEL.to_string());
            });

            let mut driver = EngineDriver {
                child,
                stdin: Some(stdin),
                rx,
                reader: Some(reader),
            };

            // Handshake: uci → uciok.
            driver.send_line("uci")?;
            driver.drain_until("uciok", Duration::from_secs(10))?;

            // Set hash size.
            driver.send_line(&format!("setoption name Hash value {hash_mib}"))?;

            // isready → readyok.
            driver.send_line("isready")?;
            driver.drain_until("readyok", Duration::from_secs(10))?;

            Ok(driver)
        }

        /// Send `ucinewgame` + `isready`/`readyok` to clear per-position TT state.
        pub(crate) fn new_game(&mut self) -> io::Result<()> {
            self.send_line("ucinewgame")?;
            self.send_line("isready")?;
            self.drain_until("readyok", Duration::from_secs(10))
        }

        /// Send `position fen <fen>` + `go movetime <ms>`; drain until `bestmove`.
        ///
        /// Returns the UCI move string. Hard wall-clock ceiling at `10 × movetime_ms`.
        pub(crate) fn search(&mut self, fen: &str, movetime_ms: u64) -> io::Result<String> {
            self.send_line(&format!("position fen {fen}"))?;
            self.send_line(&format!("go movetime {movetime_ms}"))?;

            let ceiling = Duration::from_millis(movetime_ms * 10);
            let deadline = Instant::now() + ceiling;

            loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!("engine timed out (ceiling {ceiling:?})"),
                    ));
                }
                match self.rx.recv_timeout(remaining) {
                    Ok(line) if line == EOF_SENTINEL => {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "engine stdout closed",
                        ));
                    }
                    Ok(line) if line.starts_with("bestmove ") => {
                        let uci = line["bestmove ".len()..]
                            .split_ascii_whitespace()
                            .next()
                            .unwrap_or("0000")
                            .to_string();
                        return Ok(uci);
                    }
                    Ok(_) => {}
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            format!("engine timed out (ceiling {ceiling:?})"),
                        ));
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "engine reader disconnected",
                        ));
                    }
                }
            }
        }

        /// Send `quit`, drop stdin (closes the pipe), and join the reader thread.
        pub(crate) fn quit(mut self) {
            let _ = self.send_line("quit");
            drop(self.stdin.take());
            if let Some(reader) = self.reader.take() {
                let _ = reader.join();
            }
        }

        fn send_line(&mut self, line: &str) -> io::Result<()> {
            if let Some(stdin) = &mut self.stdin {
                stdin.write_all(line.as_bytes())?;
                stdin.write_all(b"\n")?;
                stdin.flush()?;
            }
            Ok(())
        }

        /// Drain the channel until a line starting with `marker` is found.
        fn drain_until(&mut self, marker: &str, timeout: Duration) -> io::Result<()> {
            let deadline = Instant::now() + timeout;
            loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!("timed out waiting for {marker:?}"),
                    ));
                }
                match self.rx.recv_timeout(remaining) {
                    Ok(line) if line == EOF_SENTINEL => {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "engine exited during handshake",
                        ));
                    }
                    Ok(line) if line.starts_with(marker) => return Ok(()),
                    Ok(_) => {}
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            format!("timed out waiting for {marker:?}"),
                        ));
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "reader disconnected during handshake",
                        ));
                    }
                }
            }
        }
    }

    impl Drop for EngineDriver {
        fn drop(&mut self) {
            let _ = self.child.kill();
        }
    }
}

// ---------------------------------------------------------------------------
// Runner (worker pool)
// ---------------------------------------------------------------------------

mod runner {
    //! Parallel worker pool for running positions against the engine.

    use std::collections::BTreeMap;
    use std::collections::VecDeque;
    use std::io;
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    use crate::cli::Suite;
    use crate::driver::EngineDriver;
    use crate::epd::{EpdEntry, parse_sts_id};
    use crate::san::{canonicalize_san, san_of_legal_move};
    use crate::scorer::{score_sts, score_wac};

    /// Configuration for a run.
    pub(crate) struct RunConfig {
        /// Path to the engine binary.
        pub engine_path: String,
        /// Path to the EPD file (informational; entries are pre-parsed by caller).
        #[allow(dead_code)]
        pub epd_path: String,
        /// Per-position move time in milliseconds.
        pub movetime_ms: u64,
        /// Engine hash table size in MiB.
        pub hash_mib: u32,
        /// Number of parallel workers.
        pub concurrency: usize,
        /// Which suite.
        pub suite: Suite,
        /// Optional limit: only the first `limit` entries are queued.
        pub limit: Option<usize>,
        /// Extra environment variables to inject into each engine subprocess.
        /// Used in tests to pass `MOCK_ENGINE_RECORD_PATH` without mutating the
        /// calling process's environment.
        pub extra_env: Vec<(String, String)>,
    }

    /// Result for a single position.
    pub(crate) struct PositionResult {
        /// 0-based position index in the original file.
        pub index: usize,
        /// Position identifier string.
        pub id: Option<String>,
        /// STS theme `(theme_num, theme_name)` or `None` for WAC.
        pub theme: Option<(u32, String)>,
        /// Credit earned.
        pub credit: u32,
        /// Maximum possible credit.
        pub max_credit: u32,
        /// UCI move string from the engine (used in tests and summary output).
        #[allow(dead_code)]
        pub engine_uci: String,
        /// SAN rendering of the engine's move (canonical form).
        pub engine_san: String,
        /// Wall-clock elapsed time in milliseconds.
        pub elapsed_ms: u128,
    }

    /// Run the suite and return results ordered by index.
    /// Build a sentinel `PositionResult` that scores 0 for the position at `idx`.
    /// Used when the worker can't obtain a real engine answer (driver failure).
    fn fallback_result(
        suite: Suite,
        entries: &[EpdEntry],
        idx: usize,
        elapsed_ms: u128,
    ) -> PositionResult {
        let entry = &entries[idx];
        let max_credit = match suite {
            Suite::Wac => 1,
            Suite::Sts => entry
                .c0
                .as_ref()
                .map(|c| c.iter().map(|(_, w)| *w).max().unwrap_or(0))
                .unwrap_or(0),
        };
        let theme = entry.id.as_deref().and_then(parse_sts_id);
        PositionResult {
            index: idx,
            id: entry.id.clone(),
            theme,
            credit: 0,
            max_credit,
            engine_uci: "0000".to_string(),
            engine_san: "0000".to_string(),
            elapsed_ms,
        }
    }

    pub(crate) fn run(cfg: &RunConfig, entries: &[EpdEntry]) -> io::Result<Vec<PositionResult>> {
        let total = entries.len().min(cfg.limit.unwrap_or(usize::MAX));
        let entries = &entries[..total];

        let queue: Arc<Mutex<VecDeque<usize>>> = Arc::new(Mutex::new((0..total).collect()));

        let (result_tx, result_rx) = std::sync::mpsc::channel::<PositionResult>();
        // Tracks how many workers came up successfully — used by the collector
        // to error out cleanly if every worker failed to spawn.
        let workers_alive = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let n_workers = cfg.concurrency.min(total.max(1));

        std::thread::scope(|scope| -> io::Result<Vec<PositionResult>> {
            for _ in 0..n_workers {
                let queue = Arc::clone(&queue);
                let result_tx = result_tx.clone();
                let workers_alive = Arc::clone(&workers_alive);
                let engine_path = cfg.engine_path.clone();
                let hash_mib = cfg.hash_mib;
                let movetime_ms = cfg.movetime_ms;
                let suite = cfg.suite;
                let extra_env: Vec<(String, String)> = cfg.extra_env.clone();

                scope.spawn(move || {
                    let env_refs: Vec<(&str, &str)> = extra_env
                        .iter()
                        .map(|(k, v)| (k.as_str(), v.as_str()))
                        .collect();
                    let mut driver =
                        match EngineDriver::spawn_with_env(&engine_path, hash_mib, &env_refs) {
                            Ok(d) => d,
                            Err(e) => {
                                eprintln!("error: failed to spawn engine: {e}");
                                return;
                            }
                        };
                    workers_alive.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

                    loop {
                        let idx = {
                            let mut q = queue.lock().unwrap();
                            q.pop_front()
                        };
                        let Some(idx) = idx else { break };

                        let entry = &entries[idx];

                        // Per ADR-0018: clear TT before each position for determinism.
                        if let Err(e) = driver.new_game() {
                            eprintln!("error: new_game failed for position {idx}: {e}");
                            // Emit a sentinel 0-credit result so the position is
                            // counted (silent dropping would understate `total`).
                            let _ = result_tx.send(fallback_result(suite, entries, idx, 0));
                            continue;
                        }

                        let start = Instant::now();
                        let engine_uci = match driver.search(&entry.fen, movetime_ms) {
                            Ok(uci) => uci,
                            Err(e) => {
                                eprintln!("error: search failed for position {idx}: {e}");
                                "0000".to_string()
                            }
                        };
                        let elapsed_ms = start.elapsed().as_millis();

                        let engine_san = if engine_uci == "0000" {
                            "0000".to_string()
                        } else {
                            use clawfish::Move;
                            Move::from_uci(&engine_uci, &entry.position)
                                .map(|mv| canonicalize_san(&san_of_legal_move(&entry.position, mv)))
                                .unwrap_or_else(|_| engine_uci.clone())
                        };

                        let scoring = match suite {
                            Suite::Wac => score_wac(entry, &engine_uci),
                            Suite::Sts => score_sts(entry, &engine_uci),
                        };

                        let theme = entry.id.as_deref().and_then(parse_sts_id);

                        let _ = result_tx.send(PositionResult {
                            index: idx,
                            id: entry.id.clone(),
                            theme,
                            credit: scoring.credit,
                            max_credit: scoring.max_credit,
                            engine_uci,
                            engine_san,
                            elapsed_ms,
                        });
                    }

                    driver.quit();
                });
            }
            drop(result_tx);

            // Collect results; flush progress in ascending index order via cursor.
            let mut pending: BTreeMap<usize, PositionResult> = BTreeMap::new();
            let mut cursor = 0usize;
            let mut all_results: Vec<PositionResult> = Vec::with_capacity(total);

            for result in result_rx {
                let idx = result.index;
                pending.insert(idx, result);

                while let Some(r) = pending.remove(&cursor) {
                    eprint!(
                        "info: position {}/{} credit={}/{} san={} elapsed={}ms",
                        cursor + 1,
                        total,
                        r.credit,
                        r.max_credit,
                        r.engine_san,
                        r.elapsed_ms,
                    );
                    if let Some(id) = &r.id {
                        eprint!(" id={id:?}");
                    }
                    eprintln!();
                    all_results.push(r);
                    cursor += 1;
                }
            }

            // Flush any remaining out-of-order results.
            for (_, r) in pending {
                eprint!(
                    "info: position {}/{} credit={}/{} san={} elapsed={}ms",
                    r.index + 1,
                    total,
                    r.credit,
                    r.max_credit,
                    r.engine_san,
                    r.elapsed_ms,
                );
                if let Some(id) = &r.id {
                    eprint!(" id={id:?}");
                }
                eprintln!();
                all_results.push(r);
            }

            all_results.sort_by_key(|r| r.index);

            // Surface "no worker came up" as an error rather than silently
            // returning a zero-length result vector that main would print as
            // a 0/0 summary with success exit.
            if total > 0 && workers_alive.load(std::sync::atomic::Ordering::Relaxed) == 0 {
                return Err(io::Error::other(
                    "no engine worker could be spawned — see stderr for the spawn error",
                ));
            }

            Ok(all_results)
        })
    }
}

// ---------------------------------------------------------------------------
// Summary
// ---------------------------------------------------------------------------

mod summary {
    //! Aggregate result summaries for WAC and STS.

    use crate::runner::PositionResult;

    /// Summary for a WAC run.
    pub(crate) struct WacSummary {
        /// Total positions scored.
        pub total: usize,
        /// Positions where the engine found the best move.
        pub solved: usize,
    }

    /// Per-theme summary for STS.
    pub(crate) struct ThemeSummary {
        /// Theme number (1–15).
        pub theme_num: u32,
        /// Theme name.
        pub name: String,
        /// Credit earned in this theme.
        pub credit: u32,
        /// Maximum possible credit in this theme.
        pub max: u32,
        /// Number of positions in this theme (used in tests and future reporting).
        #[allow(dead_code)]
        pub positions: usize,
    }

    /// Summary for an STS run.
    pub(crate) struct StsSummary {
        /// Total credit earned.
        pub total_credit: u32,
        /// Maximum possible credit.
        pub max_credit: u32,
        /// Per-theme breakdown (theme-number ascending).
        pub per_theme: Vec<ThemeSummary>,
        /// STS-Elo estimate (Swaminathan regression).
        pub elo_estimate: f64,
    }

    /// Compute the WAC summary from scored results.
    pub(crate) fn summarize_wac(results: &[PositionResult]) -> WacSummary {
        WacSummary {
            total: results.len(),
            solved: results.iter().filter(|r| r.credit > 0).count(),
        }
    }

    /// Compute the STS summary from scored results.
    pub(crate) fn summarize_sts(results: &[PositionResult]) -> StsSummary {
        use std::collections::BTreeMap;

        let mut themes: BTreeMap<u32, (String, u32, u32, usize)> = BTreeMap::new();
        let mut total_credit = 0u32;
        let mut max_credit = 0u32;

        for r in results {
            total_credit += r.credit;
            max_credit += r.max_credit;

            if let Some((num, name)) = &r.theme {
                let entry = themes
                    .entry(*num)
                    .or_insert_with(|| (name.clone(), 0, 0, 0));
                entry.1 += r.credit;
                entry.2 += r.max_credit;
                entry.3 += 1;
            }
        }

        let per_theme: Vec<ThemeSummary> = themes
            .into_iter()
            .map(|(num, (name, credit, max, positions))| ThemeSummary {
                theme_num: num,
                name,
                credit,
                max,
                positions,
            })
            .collect();

        // Swaminathan's published STS-Elo regression: Elo ≈ 44.523 * score_pct - 242.85,
        // where `score_pct` is the percentage of max credit earned (e.g. 58.9% → 58.9).
        // The formula is calibrated against the CCRL 2000-2800 band; extrapolation
        // outside degrades.
        let score_pct = if max_credit > 0 {
            (total_credit as f64 / max_credit as f64) * 100.0
        } else {
            0.0
        };
        let elo_estimate = sts_elo_estimate(score_pct);

        StsSummary {
            total_credit,
            max_credit,
            per_theme,
            elo_estimate,
        }
    }

    /// Swaminathan's STS-Elo regression: `Elo ≈ 44.523 * score_pct − 242.85`.
    ///
    /// `score_pct` is the percentage of max credit earned (a 58.9% scoring engine
    /// passes `58.9`, not `0.589`). Source: STS site
    /// (<https://sites.google.com/site/strategictestsuite/>) and Chess Programming
    /// Wiki's Strategic Test Suite article. Calibrated for CCRL 2000-2800;
    /// extrapolation outside the band degrades.
    pub(crate) fn sts_elo_estimate(score_pct: f64) -> f64 {
        44.523 * score_pct - 242.85
    }

    /// Write the WAC summary to `w`.
    pub(crate) fn print_wac_summary(w: &mut impl std::fmt::Write, s: &WacSummary) {
        writeln!(w, "WAC summary").unwrap();
        writeln!(w, "  positions: {}", s.total).unwrap();
        writeln!(w, "  solved:    {}/{}", s.solved, s.total).unwrap();
        if s.total > 0 {
            writeln!(
                w,
                "  score:     {:.1}%",
                100.0 * s.solved as f64 / s.total as f64
            )
            .unwrap();
        }
    }

    /// Write the STS summary to `w`.
    pub(crate) fn print_sts_summary(w: &mut impl std::fmt::Write, s: &StsSummary) {
        writeln!(w, "STS summary").unwrap();
        writeln!(w, "  [CCRL band 2000-2800; extrapolation outside degrades]").unwrap();
        writeln!(w, "  total credit:  {}/{}", s.total_credit, s.max_credit).unwrap();
        if s.max_credit > 0 {
            writeln!(
                w,
                "  score:         {:.1}%",
                100.0 * s.total_credit as f64 / s.max_credit as f64
            )
            .unwrap();
        }
        writeln!(w, "  STS-Elo est.:  {:.0}", s.elo_estimate).unwrap();
        writeln!(w).unwrap();
        writeln!(
            w,
            "  {:>3}  {:<35}  {:>8}  {:>8}  {:>6}",
            "#", "Theme", "Credit", "Max", "Pct"
        )
        .unwrap();
        writeln!(
            w,
            "  {:-<3}  {:-<35}  {:-<8}  {:-<8}  {:-<6}",
            "", "", "", "", ""
        )
        .unwrap();
        for t in &s.per_theme {
            let pct = if t.max > 0 {
                100.0 * t.credit as f64 / t.max as f64
            } else {
                0.0
            };
            writeln!(
                w,
                "  {:>3}  {:<35}  {:>8}  {:>8}  {:>5.1}%",
                t.theme_num, t.name, t.credit, t.max, pct
            )
            .unwrap();
        }
    }
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let cfg = match cli::parse(&args) {
        Ok(c) => c,
        Err(msg) => {
            eprintln!("{msg}");
            return ExitCode::FAILURE;
        }
    };

    let text = match std::fs::read_to_string(&cfg.epd) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("error reading EPD file {:?}: {e}", cfg.epd);
            return ExitCode::FAILURE;
        }
    };

    let parse_results = epd::parse_epd_file(&text);
    let mut entries = Vec::with_capacity(parse_results.len());
    let mut had_errors = false;
    for (lineno, r) in parse_results.into_iter().enumerate() {
        match r {
            Ok(entry) => entries.push(entry),
            Err(e) => {
                eprintln!("parse error on line {}: {e}", lineno + 1);
                had_errors = true;
            }
        }
    }
    if had_errors {
        eprintln!("aborting due to parse errors");
        return ExitCode::FAILURE;
    }

    // For STS: informational warning when bm[0] is not at max c0 weight.
    // Normalize through position to handle over-qualified SAN (e.g., `Bg7f8` vs `Bf8`).
    if cfg.suite == cli::Suite::Sts {
        for entry in &entries {
            if let (Some(bm0), Some(c0)) = (entry.bm.first(), &entry.c0) {
                let bm0_norm = normalize_san_through_pos(&entry.position, bm0);
                let max_w = c0.iter().map(|(_, w)| *w).max().unwrap_or(0);
                let bm0_w = c0
                    .iter()
                    .find(|(s, _)| normalize_san_through_pos(&entry.position, s) == bm0_norm)
                    .map(|(_, w)| *w)
                    .unwrap_or(0);
                if bm0_w < max_w {
                    eprintln!(
                        "warning: bm {:?} has weight {} but max c0 weight is {} in {:?}",
                        bm0,
                        bm0_w,
                        max_w,
                        entry.id.as_deref().unwrap_or("?")
                    );
                }
            }
        }
    }

    /// Normalize a SAN through the position (find legal move, re-render).
    /// Used by the main STS warning check.
    fn normalize_san_through_pos(pos: &clawfish::Position, san: &str) -> String {
        let stripped = san::canonicalize_san(san);
        if let Some(mv) = san::legal_move_from_san(pos, &stripped) {
            san::canonicalize_san(&san::san_of_legal_move(pos, mv))
        } else {
            stripped
        }
    }

    let run_cfg = runner::RunConfig {
        engine_path: cfg.engine.clone(),
        epd_path: cfg.epd.clone(),
        movetime_ms: cfg.movetime_ms,
        hash_mib: cfg.hash_mib,
        concurrency: cfg.concurrency,
        suite: cfg.suite,
        limit: cfg.limit,
        extra_env: vec![],
    };

    let results = match runner::run(&run_cfg, &entries) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("run failed: {e}");
            return ExitCode::FAILURE;
        }
    };

    let mut summary_text = String::new();
    match cfg.suite {
        cli::Suite::Wac => {
            let s = summary::summarize_wac(&results);
            summary::print_wac_summary(&mut summary_text, &s);
        }
        cli::Suite::Sts => {
            let s = summary::summarize_sts(&results);
            summary::print_sts_summary(&mut summary_text, &s);
        }
    }

    print!("{summary_text}");

    if let Some(out_path) = &cfg.output
        && let Err(e) = std::fs::write(out_path, &summary_text)
    {
        eprintln!("error writing output to {out_path:?}: {e}");
        return ExitCode::FAILURE;
    }

    ExitCode::SUCCESS
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use clawfish::{Move, MoveFlag, Position, Square};

    // -----------------------------------------------------------------------
    // mod epd tests
    // -----------------------------------------------------------------------

    #[test]
    fn t1_parse_epd_line_4field_bm_id() {
        let line = "rnbqkbnr/pppppppp/8/8/4P3/8/PPPP1PPP/RNBQKBNR b KQkq - bm e5; id \"test1\";";
        let entry = crate::epd::parse_epd_line(line).unwrap().unwrap();
        assert_eq!(entry.bm, vec!["e5"]);
        assert_eq!(entry.id.as_deref(), Some("test1"));
        assert!(entry.c0.is_none());
    }

    #[test]
    fn t2_parse_epd_line_6field() {
        let line =
            "rnbqkbnr/pppppppp/8/8/4P3/8/PPPP1PPP/RNBQKBNR b KQkq - 0 1 bm e5; id \"test2\";";
        let a = crate::epd::parse_epd_line(line).unwrap().unwrap();
        let line4 = "rnbqkbnr/pppppppp/8/8/4P3/8/PPPP1PPP/RNBQKBNR b KQkq - bm e5; id \"test2\";";
        let b = crate::epd::parse_epd_line(line4).unwrap().unwrap();
        assert_eq!(a.position, b.position);
        assert_eq!(a.bm, vec!["e5"]);
    }

    #[test]
    fn t3_bm_multiple_moves() {
        let line = "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - bm Nf3 Nc3; id \"t3\";";
        let entry = crate::epd::parse_epd_line(line).unwrap().unwrap();
        assert_eq!(entry.bm, vec!["Nf3", "Nc3"]);
    }

    #[test]
    fn t4_c0_parsing() {
        // Note: `;` inside the quoted string must NOT terminate the c0 value.
        let line = "1kr5/3n4/q3p2p/p2n2p1/PppB1P2/5BP1/1P2Q2P/3R2K1 w - - bm f5; id \"STS(v1.0) Undermine.001\"; c0 \"f5=10, Be5+=2, Bf2=3, Bg4=2\";";
        let entry = crate::epd::parse_epd_line(line).unwrap().unwrap();
        let c0 = entry.c0.unwrap();
        assert_eq!(c0.len(), 4);
        assert_eq!(c0[0], ("f5".to_string(), 10));
        assert_eq!(c0[1], ("Be5+".to_string(), 2));
        assert_eq!(c0[2], ("Bf2".to_string(), 3));
        assert_eq!(c0[3], ("Bg4".to_string(), 2));
    }

    #[test]
    fn t5_c0_extra_spaces_trailing_comma() {
        let line = "1kr5/3n4/q3p2p/p2n2p1/PppB1P2/5BP1/1P2Q2P/3R2K1 w - - bm f5; c0 \"f5=10 , Be5+=2 , \";";
        let entry = crate::epd::parse_epd_line(line).unwrap().unwrap();
        let c0 = entry.c0.unwrap();
        assert_eq!(c0.len(), 2);
        assert_eq!(c0[0].0, "f5");
        assert_eq!(c0[1].0, "Be5+");
    }

    #[test]
    fn t6_empty_and_comment_line() {
        assert!(matches!(
            crate::epd::parse_epd_line(""),
            Err(crate::epd::EpdParseError::EmptyLine)
        ));
        assert!(matches!(
            crate::epd::parse_epd_line("# comment"),
            Err(crate::epd::EpdParseError::EmptyLine)
        ));
    }

    #[test]
    fn t7_missing_bm() {
        let line = "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - id \"no-bm\";";
        assert!(matches!(
            crate::epd::parse_epd_line(line),
            Err(crate::epd::EpdParseError::MissingBm)
        ));
    }

    #[test]
    fn t8_bad_fen() {
        let line = "not_a_fen bm Nf3;";
        assert!(matches!(
            crate::epd::parse_epd_line(line),
            Err(crate::epd::EpdParseError::BadFen(_))
        ));
    }

    #[test]
    fn t9_parse_sts_id() {
        assert_eq!(
            crate::epd::parse_sts_id("STS(v7.0) Knight Outposts.42"),
            Some((7, "Knight Outposts".to_string()))
        );
        assert_eq!(
            crate::epd::parse_sts_id("STS(v1.0) Undermine.001"),
            Some((1, "Undermine".to_string()))
        );
        assert_eq!(
            crate::epd::parse_sts_id("STS(v2.2) Open Files and Diagonals.001"),
            Some((2, "Open Files and Diagonals".to_string()))
        );
        assert_eq!(crate::epd::parse_sts_id("garbage"), None);
        assert_eq!(crate::epd::parse_sts_id(""), None);
    }

    // -----------------------------------------------------------------------
    // mod san tests
    // -----------------------------------------------------------------------

    fn startpos() -> Position {
        Position::from_fen(Position::STARTING_FEN).unwrap()
    }

    #[test]
    fn s1_pawn_push() {
        let pos = startpos();
        let mv = Move::new(Square::E2, Square::E4, MoveFlag::DoublePush);
        assert_eq!(crate::san::san_of_legal_move(&pos, mv), "e4");
    }

    #[test]
    fn s2_pawn_capture() {
        // After 1.e4 d5, white exd5.
        let pos =
            Position::from_fen("rnbqkbnr/ppp1pppp/8/3p4/4P3/8/PPPP1PPP/RNBQKBNR w KQkq d6 0 2")
                .unwrap();
        let mv = Move::new(Square::E4, Square::D5, MoveFlag::Capture);
        assert_eq!(crate::san::san_of_legal_move(&pos, mv), "exd5");
    }

    #[test]
    fn s3_castling() {
        let pos = Position::from_fen("r3k2r/pppppppp/8/8/8/8/PPPPPPPP/R3K2R w KQkq - 0 1").unwrap();
        let ks = Move::new(Square::E1, Square::G1, MoveFlag::KingCastle);
        let qs = Move::new(Square::E1, Square::C1, MoveFlag::QueenCastle);
        assert_eq!(crate::san::san_of_legal_move(&pos, ks), "O-O");
        assert_eq!(crate::san::san_of_legal_move(&pos, qs), "O-O-O");
    }

    #[test]
    fn s4_knight_file_disambiguation() {
        // Two knights on b1 and f3, both can reach d2.
        let pos = Position::from_fen("4k3/8/8/8/8/5N2/8/1N2K3 w - - 0 1").unwrap();
        let mv_b1d2 = Move::new(Square::B1, Square::D2, MoveFlag::Quiet);
        let mv_f3d2 = Move::new(Square::F3, Square::D2, MoveFlag::Quiet);
        let san_b1d2 = crate::san::san_of_legal_move(&pos, mv_b1d2);
        let san_f3d2 = crate::san::san_of_legal_move(&pos, mv_f3d2);
        assert_ne!(san_b1d2, san_f3d2, "disambiguation must differ");
        // Both should be Nd2 variants with different file letters.
        assert!(
            san_b1d2.contains("bd2") || san_b1d2.to_lowercase().contains('b'),
            "got: {san_b1d2}"
        );
        assert!(
            san_f3d2.contains("fd2") || san_f3d2.to_lowercase().contains('f'),
            "got: {san_f3d2}"
        );
    }

    #[test]
    fn s5_rook_rank_disambiguation() {
        // Two rooks on a1 and a5, both can go to a3 — rank disambiguates.
        let pos = Position::from_fen("4k3/8/8/R7/8/8/8/R3K3 w - - 0 1").unwrap();
        let mv_a1a3 = Move::new(Square::A1, Square::A3, MoveFlag::Quiet);
        let mv_a5a3 = Move::new(Square::A5, Square::A3, MoveFlag::Quiet);
        let san_a1 = crate::san::san_of_legal_move(&pos, mv_a1a3);
        let san_a5 = crate::san::san_of_legal_move(&pos, mv_a5a3);
        assert_ne!(san_a1, san_a5);
        // Rank disambiguation: should contain rank digit.
        assert!(
            san_a1.contains('1') || san_a1.contains('a'),
            "got: {san_a1}"
        );
        assert!(
            san_a5.contains('5') || san_a5.contains('a'),
            "got: {san_a5}"
        );
    }

    #[test]
    fn s6_queen_full_square_disambiguation() {
        // Three white queens at a1, a4, h1 can all reach d1:
        //   a1 → d1 along rank 1
        //   a4 → d1 along diagonal a4-d1
        //   h1 → d1 along rank 1
        // The a1 mover shares file with a4 AND shares rank with h1, so neither
        // file alone nor rank alone disambiguates — full square required.
        // Kings at e8/e2 keep the position legal and out of the way.
        let pos = Position::from_fen("4k3/8/8/8/Q7/8/4K3/Q6Q w - - 0 1").unwrap();

        let mv_a1d1 = clawfish::Move::new(Square::A1, Square::D1, MoveFlag::Quiet);
        let mv_a4d1 = clawfish::Move::new(Square::A4, Square::D1, MoveFlag::Quiet);
        let mv_h1d1 = clawfish::Move::new(Square::H1, Square::D1, MoveFlag::Quiet);

        // a1 mover: file 'a' shared with a4, rank '1' shared with h1 → full square.
        assert_eq!(crate::san::san_of_legal_move(&pos, mv_a1d1), "Qa1d1");
        // a4 mover: file 'a' shared with a1, but rank '4' unique → rank disambiguation.
        assert_eq!(crate::san::san_of_legal_move(&pos, mv_a4d1), "Q4d1");
        // h1 mover: file 'h' unique → file disambiguation.
        assert_eq!(crate::san::san_of_legal_move(&pos, mv_h1d1), "Qhd1");
    }

    #[test]
    fn s7_promotion() {
        let pos = Position::from_fen("4k3/P7/8/8/8/8/8/4K3 w - - 0 1").unwrap();
        let mv_quiet = Move::new(Square::A7, Square::A8, MoveFlag::QueenPromo);
        assert_eq!(crate::san::san_of_legal_move(&pos, mv_quiet), "a8=Q");

        let pos2 = Position::from_fen("1r2k3/P7/8/8/8/8/8/4K3 w - - 0 1").unwrap();
        let mv_cap = Move::new(Square::A7, Square::B8, MoveFlag::RookPromoCapture);
        assert_eq!(crate::san::san_of_legal_move(&pos2, mv_cap), "axb8=R");
    }

    #[test]
    fn s8_en_passant() {
        let pos = Position::from_fen("4k3/8/8/3pP3/8/8/8/4K3 w - d6 0 1").unwrap();
        let mv = Move::new(Square::E5, Square::D6, MoveFlag::EnPassant);
        assert_eq!(crate::san::san_of_legal_move(&pos, mv), "exd6");
    }

    #[test]
    fn s9_legal_move_from_san_round_trip() {
        let pos = startpos();
        use clawfish::{MoveList, generate_moves};
        let mut ml = MoveList::new();
        generate_moves(&pos, &mut ml);
        for mv in ml.as_slice().iter().copied() {
            let san = crate::san::san_of_legal_move(&pos, mv);
            let mv2 = crate::san::legal_move_from_san(&pos, &san)
                .unwrap_or_else(|| panic!("SAN {san:?} round-trip failed"));
            assert_eq!(mv, mv2, "round-trip mismatch for {san:?}");
        }
    }

    #[test]
    fn s10_strip_san_decoration() {
        assert_eq!(crate::san::strip_san_decoration("Nf3+"), "Nf3");
        assert_eq!(crate::san::strip_san_decoration("Qxh7#"), "Qxh7");
        assert_eq!(crate::san::strip_san_decoration("e4?!"), "e4");
        assert_eq!(crate::san::strip_san_decoration("  Bd5+  "), "Bd5");
        assert_eq!(crate::san::strip_san_decoration("O-O"), "O-O");
    }

    #[test]
    fn s11_canonicalize_san_zero_castling() {
        assert_eq!(crate::san::canonicalize_san("0-0"), "O-O");
        assert_eq!(crate::san::canonicalize_san("0-0-0"), "O-O-O");
        assert_eq!(crate::san::canonicalize_san("O-O+"), "O-O");
    }

    // -----------------------------------------------------------------------
    // mod scorer tests
    // -----------------------------------------------------------------------

    fn make_wac_entry(fen: &str, bm: &[&str]) -> crate::epd::EpdEntry {
        crate::epd::EpdEntry {
            fen: fen.to_string(),
            position: Position::from_fen(fen).unwrap(),
            bm: bm.iter().map(|s| s.to_string()).collect(),
            c0: None,
            id: None,
        }
    }

    fn make_sts_entry(fen: &str, bm: &[&str], c0: &[(&str, u32)]) -> crate::epd::EpdEntry {
        crate::epd::EpdEntry {
            fen: fen.to_string(),
            position: Position::from_fen(fen).unwrap(),
            bm: bm.iter().map(|s| s.to_string()).collect(),
            c0: Some(c0.iter().map(|(s, w)| (s.to_string(), *w)).collect()),
            id: None,
        }
    }

    const STARTPOS: &str = Position::STARTING_FEN;

    #[test]
    fn sc1_score_wac_correct() {
        let entry = make_wac_entry(STARTPOS, &["e4"]);
        let result = crate::scorer::score_wac(&entry, "e2e4");
        assert_eq!(result.credit, 1);
        assert_eq!(result.max_credit, 1);
    }

    #[test]
    fn sc2_score_wac_wrong() {
        let entry = make_wac_entry(STARTPOS, &["e4"]);
        let result = crate::scorer::score_wac(&entry, "d2d4");
        assert_eq!(result.credit, 0);
        assert_eq!(result.max_credit, 1);
    }

    #[test]
    fn sc3_score_sts_top_credit() {
        let entry = make_sts_entry(STARTPOS, &["e4"], &[("e4", 10), ("d4", 2)]);
        let result = crate::scorer::score_sts(&entry, "e2e4");
        assert_eq!(result.credit, 10);
        assert_eq!(result.max_credit, 10);
    }

    #[test]
    fn sc4_score_sts_partial_credit() {
        let entry = make_sts_entry(STARTPOS, &["e4"], &[("e4", 10), ("d4", 2)]);
        let result = crate::scorer::score_sts(&entry, "d2d4");
        assert_eq!(result.credit, 2);
        assert_eq!(result.max_credit, 10);
    }

    #[test]
    fn sc5_score_sts_no_credit() {
        let entry = make_sts_entry(STARTPOS, &["e4"], &[("e4", 10), ("d4", 2)]);
        let result = crate::scorer::score_sts(&entry, "c2c4");
        assert_eq!(result.credit, 0);
        assert_eq!(result.max_credit, 10);
    }

    #[test]
    fn sc6_null_move_scores_zero() {
        let wac = make_wac_entry(STARTPOS, &["e4"]);
        assert_eq!(crate::scorer::score_wac(&wac, "0000").credit, 0);

        let sts = make_sts_entry(STARTPOS, &["e4"], &[("e4", 10)]);
        assert_eq!(crate::scorer::score_sts(&sts, "0000").credit, 0);
    }

    #[test]
    fn sc7_decoration_tolerance() {
        // bm contains "Nf3+" — engine returns "g1f3" which renders as "Nf3".
        let entry = make_wac_entry(STARTPOS, &["Nf3+"]);
        let result = crate::scorer::score_wac(&entry, "g1f3");
        assert_eq!(result.credit, 1);
    }

    // -----------------------------------------------------------------------
    // mod driver tests (use mock-engine)
    // -----------------------------------------------------------------------

    /// Path to the mock-engine binary.
    ///
    /// `option_env!` returns `None` when the variable is not set (e.g., during
    /// `cargo clippy --all-targets` without a prior build). The helper panics at
    /// runtime with a clear message in that case.
    const MOCK_ENGINE_EXE: Option<&str> = option_env!("CARGO_BIN_EXE_mock-engine");

    fn mock_engine_path() -> String {
        if let Some(p) = MOCK_ENGINE_EXE {
            return p.to_owned();
        }
        // Fallback: look for the binary adjacent to the test executable.
        let exe = std::env::current_exe().expect("current_exe");
        let deps_dir = exe.parent().expect("deps dir");
        let release_dir = deps_dir.parent().expect("release or debug dir");
        let candidate = release_dir.join("mock-engine");
        if candidate.exists() {
            return candidate.to_str().expect("valid utf8").to_owned();
        }
        panic!(
            "could not find mock-engine binary — build with `cargo build` first, \
             then run via `cargo test --bin epd-suite`"
        );
    }

    fn temp_record_file() -> String {
        let id = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos();
        format!("{}/epd_suite_mock_{id}.txt", std::env::temp_dir().display())
    }

    #[test]
    fn d1_driver_spawn_and_handshake() {
        let record = temp_record_file();
        // Verify we can spawn the mock binary; env var passed via Command::env.
        let mut child = std::process::Command::new(mock_engine_path())
            .env("MOCK_ENGINE_RECORD_PATH", &record)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("mock-engine spawn");
        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_file(&record);
    }

    fn spawn_mock_driver(record: &str) -> crate::driver::EngineDriver {
        crate::driver::EngineDriver::spawn_with_env(
            &mock_engine_path(),
            16,
            &[("MOCK_ENGINE_RECORD_PATH", record)],
        )
        .expect("spawn mock driver")
    }

    #[test]
    fn d2_driver_search_returns_bestmove() {
        let record = temp_record_file();
        let mut driver = spawn_mock_driver(&record);
        driver.new_game().expect("new_game");
        let uci = driver.search(Position::STARTING_FEN, 50).expect("search");
        assert_eq!(uci, "0000");
        driver.quit();
        let _ = std::fs::remove_file(&record);
    }

    #[test]
    fn d3_driver_timeout_on_no_bestmove() {
        // Real-driver timeout test: spawn `/bin/cat`, which reads stdin forever
        // and never writes anything UCI-shaped. `EngineDriver::spawn_with_env`
        // would normally block waiting for `uciok` during the handshake — bypass
        // that by constructing the driver state by hand and calling `search`.
        // The 10× movetime ceiling fires; assert TimedOut.
        //
        // Mirrors the `/bin/cat` fixture pattern from `elo-iterate.rs`'s driver
        // tests (search elo-iterate.rs for "/bin/cat").
        use std::io::{BufRead, BufReader};
        use std::process::{Command, Stdio};

        let mut child = Command::new("/bin/cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn /bin/cat");
        let stdin = child.stdin.take().expect("stdin piped");
        let stdout = child.stdout.take().expect("stdout piped");

        let (tx, rx) = std::sync::mpsc::sync_channel::<String>(1024);
        let reader = std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });

        let mut driver = crate::driver::EngineDriver {
            child,
            stdin: Some(stdin),
            rx,
            reader: Some(reader),
        };

        let result = driver.search(clawfish::Position::STARTING_FEN, 50);
        assert!(
            matches!(result, Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut),
            "expected TimedOut, got {result:?}"
        );

        driver.quit();
    }

    // -----------------------------------------------------------------------
    // Corpus invariants (CV1 + CV2)
    // -----------------------------------------------------------------------

    #[test]
    fn cv1_wac_corpus_invariants() {
        let text = include_str!("../../bench/data/wac.epd");
        let entries: Vec<_> = crate::epd::parse_epd_file(text)
            .into_iter()
            .filter_map(|r| r.ok())
            .collect();
        assert_eq!(
            entries.len(),
            300,
            "WAC should have 300 positions, got {}",
            entries.len()
        );
        for entry in &entries {
            assert!(!entry.bm.is_empty(), "every WAC entry must have bm");
            // WAC may have free-text c0 comments (not weighted); if c0 is present
            // it should be empty (no san=weight pairs) since they're comment strings.
            if let Some(c0) = &entry.c0 {
                assert!(
                    c0.is_empty(),
                    "WAC c0 must be a plain comment (no san=weight entries), got {:?} in {:?}",
                    c0,
                    entry.id
                );
            }
        }
    }

    #[test]
    fn cv2_sts_corpus_presence() {
        // Every STS entry's canonicalized bm[0] must be present in c0.
        let text = include_str!("../../bench/data/sts.epd");
        let entries: Vec<_> = crate::epd::parse_epd_file(text)
            .into_iter()
            .filter_map(|r| r.ok())
            .collect();
        assert_eq!(
            entries.len(),
            1500,
            "STS should have 1500 positions, got {}",
            entries.len()
        );

        for entry in &entries {
            assert_eq!(entry.bm.len(), 1, "STS bm must have exactly one move");
            assert!(entry.c0.is_some(), "STS entry must have c0");

            let id = entry.id.as_deref().unwrap_or("");
            let _ = crate::epd::parse_sts_id(id)
                .unwrap_or_else(|| panic!("parse_sts_id failed for: {id:?}"));

            // bm[0] normalized through the position must appear somewhere in c0.
            // We normalize through the position to handle over-qualified SAN in c0
            // (e.g., `Bg7f8` in c0 matching `Bf8` in bm).
            if let Some(c0) = &entry.c0 {
                let bm0_norm = normalize_through_pos(&entry.position, &entry.bm[0]);
                let found = c0
                    .iter()
                    .any(|(s, _)| normalize_through_pos(&entry.position, s) == bm0_norm);
                assert!(
                    found,
                    "bm[0] {:?} not found in c0 for {:?}",
                    entry.bm[0], entry.id
                );
            }
        }
    }

    /// Normalize a SAN string through the position (find legal move, re-render).
    fn normalize_through_pos(pos: &clawfish::Position, san: &str) -> String {
        let stripped = crate::san::canonicalize_san(san);
        if let Some(mv) = crate::san::legal_move_from_san(pos, &stripped) {
            crate::san::canonicalize_san(&crate::san::san_of_legal_move(pos, mv))
        } else {
            stripped
        }
    }

    #[test]
    fn cv2b_sts_bm_is_max_weight_in_c0() {
        // bm[0] should be at max weight; allow up to 5 errata in 1500.
        let text = include_str!("../../bench/data/sts.epd");
        let entries: Vec<_> = crate::epd::parse_epd_file(text)
            .into_iter()
            .filter_map(|r| r.ok())
            .collect();

        let mut not_at_max = 0usize;
        for entry in &entries {
            if let (Some(bm0), Some(c0)) = (entry.bm.first(), &entry.c0) {
                let bm0_norm = normalize_through_pos(&entry.position, bm0);
                let max_w = c0.iter().map(|(_, w)| *w).max().unwrap_or(0);
                let bm0_w = c0
                    .iter()
                    .find(|(s, _)| normalize_through_pos(&entry.position, s) == bm0_norm)
                    .map(|(_, w)| *w)
                    .unwrap_or(0);
                if bm0_w < max_w {
                    not_at_max += 1;
                }
            }
        }
        assert!(
            not_at_max <= 5,
            "too many STS entries where bm[0] is not at max c0 weight: {not_at_max}"
        );
    }

    // -----------------------------------------------------------------------
    // mod summary tests
    // -----------------------------------------------------------------------

    fn make_result(
        index: usize,
        credit: u32,
        max: u32,
        theme: Option<(u32, &str)>,
    ) -> crate::runner::PositionResult {
        crate::runner::PositionResult {
            index,
            id: None,
            theme: theme.map(|(n, s)| (n, s.to_string())),
            credit,
            max_credit: max,
            engine_uci: "e2e4".to_string(),
            engine_san: "e4".to_string(),
            elapsed_ms: 100,
        }
    }

    #[test]
    fn su1_summarize_wac() {
        let results = vec![
            make_result(0, 1, 1, None),
            make_result(1, 0, 1, None),
            make_result(2, 1, 1, None),
        ];
        let s = crate::summary::summarize_wac(&results);
        assert_eq!(s.total, 3);
        assert_eq!(s.solved, 2);
    }

    #[test]
    fn su2_summarize_sts_themes() {
        let results = vec![
            make_result(0, 10, 10, Some((1, "Undermine"))),
            make_result(1, 2, 10, Some((1, "Undermine"))),
            make_result(2, 0, 10, Some((2, "Open Files"))),
        ];
        let s = crate::summary::summarize_sts(&results);
        assert_eq!(s.total_credit, 12);
        assert_eq!(s.max_credit, 30);
        assert_eq!(s.per_theme.len(), 2);
        let t1 = s.per_theme.iter().find(|t| t.theme_num == 1).unwrap();
        assert_eq!(t1.credit, 12);
        assert_eq!(t1.max, 20);
        assert_eq!(t1.positions, 2);
    }

    #[test]
    fn su3_elo_estimate() {
        // Swaminathan: 44.523 * score_pct - 242.85.
        // At score_pct = 80.0 (i.e., 80%): 44.523 * 80 - 242.85 = 3318.99.
        let elo = crate::summary::sts_elo_estimate(80.0);
        let expected = 44.523_f64.mul_add(80.0, -242.85);
        assert!(
            (elo - expected).abs() < 0.01,
            "got {elo}, expected {expected}"
        );
        // Sanity: the M5.E end-state percentage (~58.9%) maps to ~2380 STS-Elo,
        // which is in the right ballpark vs the actual ~2622 mixed-TC rating
        // estimate (STS systematically underestimates game-playing strength).
        let elo_m5e = crate::summary::sts_elo_estimate(58.9);
        assert!(
            (2370.0..2400.0).contains(&elo_m5e),
            "M5.E mapping out of expected band: {elo_m5e}"
        );
    }

    // -----------------------------------------------------------------------
    // mod runner smoke test (R1)
    // -----------------------------------------------------------------------

    #[test]
    fn r1_runner_mock_engine_two_positions() {
        let record = temp_record_file();

        let fen = Position::STARTING_FEN;
        let entries = vec![
            crate::epd::EpdEntry {
                fen: fen.to_string(),
                position: Position::from_fen(fen).unwrap(),
                bm: vec!["e4".to_string()],
                c0: None,
                id: Some("test.001".to_string()),
            },
            crate::epd::EpdEntry {
                fen: fen.to_string(),
                position: Position::from_fen(fen).unwrap(),
                bm: vec!["d4".to_string()],
                c0: None,
                id: Some("test.002".to_string()),
            },
        ];

        let cfg = crate::runner::RunConfig {
            engine_path: mock_engine_path(),
            epd_path: "dummy.epd".to_string(),
            movetime_ms: 50,
            hash_mib: 16,
            concurrency: 1,
            suite: crate::cli::Suite::Wac,
            limit: None,
            extra_env: vec![("MOCK_ENGINE_RECORD_PATH".to_string(), record.clone())],
        };

        let results = crate::runner::run(&cfg, &entries).expect("run");
        let _ = std::fs::remove_file(&record);

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].engine_uci, "0000");
        assert_eq!(results[0].credit, 0);
        assert_eq!(results[1].engine_uci, "0000");
        assert_eq!(results[1].credit, 0);
        assert_eq!(results[0].index, 0);
        assert_eq!(results[1].index, 1);
    }
}
