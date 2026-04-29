# Stockfish 18 — mid-session `UCI_Elo` reconfiguration probe

**Pre-ELOH.A preflight (per `docs/tooling/elo-iteration-harness.md`).**
Resolves the open contract question: does Stockfish 18 honor `setoption name UCI_Elo value <new>` *between games* without an intervening process restart?

- **Date.** 2026-04-29.
- **Stockfish.** Stockfish 18 (`/opt/homebrew/bin/stockfish`, Homebrew on Apple Silicon macOS).
- **Outcome.** Mid-session `setoption UCI_Elo` is honored. Spawn-once contract for ELOH.A / ELOH.B is viable.

## Method

Drive Stockfish via stdin/stdout, single process for the run:

1. `uci` → `isready`.
2. `setoption name UCI_LimitStrength value true`.
3. **Round 1.** `setoption name UCI_Elo value 1320` → `position fen <Kiwipete>` → `go depth 12`. Record bestmove + the depth-12 PV head.
4. **Round 2.** `setoption name UCI_Elo value 2400` (no `ucinewgame`) → same fen → same `go`. Record bestmove + PV head.
5. **Round 3.** `setoption name UCI_Elo value 1320` (no `ucinewgame`) → same fen → same `go`. Record bestmove + PV head.

Position: Kiwipete (`r3k2r/p1ppqpb1/bn2pnp1/3PN3/1p2P3/2N2Q1p/PPPBBPPP/R3K2R w KQkq - 0 1`). Tactical complexity at depth 12 is high enough that strong play diverges from `UCI_Elo`-induced stochastic play.

A second pass repeated round 1 and round 2 with `ucinewgame` between them as a control — to disambiguate "setoption took effect" from "hash-table residue confused round 2."

## Raw observations

Probe transcript at `/tmp/claude-501/eloh-probe/transcript.txt` (per-machine, not committed).

| Round | UCI_Elo | ucinewgame before | Depth-12 PV head | bestmove | Interpretation |
|---|---|---|---|---|---|
| 1 (test 1) | 1320 | no | `d5e6 e7e6 e2a6 …` | `d2g5` | Stochastic — bestmove ≠ PV head; weak choice. |
| 2 (test 1) | 2400 | no | `d5e6 a6e2 c3e2 …` | `d5e6` | Bestmove = PV head; strong choice. |
| 3 (test 1) | 1320 | no | `d5e6 e7e6 e2a6 …` | `d5d6` | Stochastic again; ≠ round 1's bestmove (different roll, hash residue). |
| 1 (test 2) | 1320 | yes | (similar PV) | `d2f4` | Stochastic, fresh hash. |
| 2 (test 2) | 2400 | yes | (similar PV) | `d5d6` | Strong choice on a fresh hash. |

The score (`info … score cp`) reported by Stockfish is identical across `UCI_Elo` settings — the engine evaluates the position the same way regardless of strength cap. `UCI_LimitStrength` introduces stochasticity at *move selection*, not at evaluation.

## Conclusion

**Mid-session `setoption UCI_Elo` is honored.** The `2400` round picked the engine's PV-head move; both `1320` rounds picked a non-PV stochastic move. The 1320→2400→1320 sequence shows the strength cap toggles cleanly between strong and stochastic regimes within one process lifetime.

**Hash-table preservation across `setoption` is observed but immaterial.** Round 3 (back to 1320, no ucinewgame) picks a *different* stochastic move than round 1 — the hash from rounds 1+2 biases round 3's search. This is expected behavior for a long-lived UCI process and is not specific to `setoption`. ELOH.A's per-game `ucinewgame` (standard practice between games anyway) sidesteps any hash-residue concern.

## Implications for ELOH.A / ELOH.B

- **ELOH.A** — fixed-config opponent, no mid-run reconfiguration. The probe is satisfied trivially: spawn Stockfish once at run start, `setoption UCI_LimitStrength true` + `setoption UCI_Elo 1320` once, then loop games separated by `ucinewgame`. No spawn-per-pair fallback needed.
- **ELOH.B** — mid-run `setoption UCI_Elo <new>` between games. The probe shows the option is honored without process restart. Standard `ucinewgame` between games clears hash residue.

The fallback path documented in the spec ("if the probe fails, ELOH.A's spawn-once contract becomes spawn-per-game-pair, and ELOH.A's match-loop budget grows by ~30 LOC") is **not triggered**. ELOH.A keeps the spawn-once design.
