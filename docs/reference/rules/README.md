# FIDE Rules Reference — Engine-Relevant Slices

All rule wording is verbatim from the FIDE Laws of Chess (approved by the FIDE General Assembly on 7 August 2022, in force from 1 January 2023). See [`../README.md`](../README.md) for the upstream source URL and re-fetch instructions.

---

## Files

| File | Contents |
|---|---|
| [board-and-pieces.md](board-and-pieces.md) | Articles 1–2: game objective, board layout, initial position, piece set and symbols, file/rank/diagonal definitions. |
| [piece-movement.md](piece-movement.md) | Articles 3.1–3.6: general movement rules (no sharing a square, capture semantics, attack definition), bishop, rook, queen, slider blocking, knight. |
| [pawn.md](pawn.md) | Article 3.7 complete: single advance, double advance, diagonal capture, en passant, promotion. |
| [castling.md](castling.md) | Article 3.8: both forms of castling, king and rook final squares, all conditions for permanent loss of castling rights, all conditions for temporary prevention. |
| [check-and-legal-moves.md](check-and-legal-moves.md) | Articles 3.9–3.10: definition of check, prohibition on leaving/exposing king in check, definition of legal/illegal moves and illegal positions. |
| [winning-and-losing.md](winning-and-losing.md) | Article 5.1: checkmate ends the game; resignation and the dead-position exception to resignation. |
| [draws.md](draws.md) | Articles 5.2 and 9 consolidated: stalemate, dead position, draw by agreement, threefold repetition (claim), 50-move rule (claim), fivefold repetition (automatic), 75-move rule (automatic). |
| [algebraic-notation.md](algebraic-notation.md) | Appendix C: Standard Algebraic Notation — piece abbreviations, file/rank labelling, square coordinates, move notation, captures, disambiguation, promotion, castling symbols, check/checkmate symbols, sample game. |
| [glossary.md](glossary.md) | FIDE Glossary pruned to terms relevant to the rules above; clock/scoresheet/arbiter/conduct/variant terms omitted. |

---

## What Was Dropped and Why

| Dropped material | Reason |
|---|---|
| Article 4 (The Act of Moving the Pieces) | Touched-piece and hand-release rules — competitive over-the-board procedure only; irrelevant to engine move generation or legality. |
| Article 6 (The Chessclock) | Clock management, time controls, flag-fall — engine does not implement a clock at the rules layer. |
| Article 7 (Irregularities) | Illegal-move penalties, board-reset procedures, arbiter interventions — over-the-board administration only. |
| Article 8 (The Recording of the Moves) | Scoresheet obligations — physical record-keeping; engine uses notation only for SAN/PGN parsing (covered by Appendix C). |
| Article 9.1 (Draw offer procedure) | Draw offer timing and scoresheet marking — competitive procedure; the substantive draw conditions are retained in `draws.md`. |
| Article 9.4, 9.5 (Claim procedure) | Touched-piece and clock-pausing steps for claiming draws — arbiter procedure; the draw rules themselves are retained. |
| Article 10 (Points) | Match scoring (1, ½, 0) — scoring system for competitions, not rules of the game. |
| Article 11 (Conduct of the Players) | Player conduct, prohibited devices, distraction rules — over-the-board competition only. |
| Article 12 (Role of the Arbiter) | Arbiter powers and penalties — over-the-board administration only. |
| Appendix A (Rapid Chess) | Time control variant — clock-related, irrelevant to engine rules layer. |
| Appendix B (Blitz) | Time control variant — clock-related, irrelevant to engine rules layer. |
| Appendix C, rule C.3 (Figurine notation in other languages) | Engine speaks English SAN/long-algebraic only; foreign-language piece abbreviations and figurines are out of scope. |
| Appendix C, rule C.12 (Draw offer mark) | Scoresheet symbol (=) for draw offers — scoresheet procedure, not engine-relevant. |
| Appendix D (Rules for Blind/Visually Disabled Players) | Physical accommodation for over-the-board play; no bearing on engine implementation. |
| Guidelines I (Adjourned Games) | Sealed-move and adjournment procedures — over-the-board competition only; engines do not adjourn. |
| Guidelines II (Chess960 Rules) | Variant chess — explicitly out of scope per ADR-0001. |
| Guidelines III (Games without Increment / Quickplay Finishes) | Clock-based draw-claim procedures — clock-related, irrelevant to engine rules layer. |
| Glossary entries for dropped articles | Terms such as `adjourn`, `arbiter`, `chessclock`, `flag`, `flag-fall`, `forfeit`, `j'adoube`, `scoresheet`, `sealed move`, `touch move`, `Chess960`, `quickplay finish`, `blitz`, `rapid chess`, `default time`, `increment`, etc. are omitted because they only appear in dropped articles. |
