# M6.B research — Pawn structure eval + pawn hash + pawn-Zobrist substream

Prior-art synthesis for M6.B. Sources are Chess Programming Wiki, TalkChess
threads, and public blog posts — no engine source repos (ADR-0003). The plan
(`docs/plans/m6.b.md`) and ADR-0032 are written separately by the orchestrator;
this note is the design-space record the plan references.

## 1. Term definitions

### 1.1 Isolated pawn

- **Predicate.** A pawn with no friendly pawn on either adjacent file (any
  rank). Set-wise: `isolanis = wpawns & ~fileFill(adjacent-file pawns)`
  ([CPW — Isolated Pawns (Bitboards)](https://www.chessprogramming.org/Isolated_Pawns_(Bitboards))).
- **Granularity.** Per pawn — each isolated pawn scored individually.
- **Half-isolani** (isolated on only one adjacent file) — out of scope for M6.B.
- **Weights (literature center).** MG −10, EG −20. CPW: isolated penalties are
  larger than backward penalties. Spread across surveyed engines is wide
  (−5…−20); M6.F Texel tune resolves.

### 1.2 Doubled pawn

- **Predicate.** Pawns that are not the front-most friendly pawn on their file:
  `doubled = wpawns & wRearSpans(wpawns)`
  ([CPW — Doubled Pawn](https://www.chessprogramming.org/Doubled_Pawn)).
- **Granularity.** Per *extra* pawn on the file (`popcount_on_file − 1`):
  doubled→1 penalty, tripled→2. Per-file-once under-counts tripled pawns.
- **Weights (literature center).** MG −10, EG −15. Post-NNUE consensus is mild
  (a few…~20 cp), down from historical half-pawn valuations.

### 1.3 Backward pawn — the definitional divergence

Four competing definitions in the literature:

| Variant | Condition | Verdict |
|---|---|---|
| Kmoch / Straggler | rank 2–3, stop sq undefended, attacked by enemy pawn, half-open file | rejected — over-restrictive |
| **CPW-simple** | stop sq not in own attack-front-spans AND attacked by an enemy pawn; no rank/half-open restriction | **chosen** |
| Stop-square-only | stop sq lacks friendly support, regardless of enemy presence | rejected — flags too many, needs tiny penalty |
| SEE-based | stop sq has negative SEE incl. pieces | rejected — needs piece info, pawn-hash-incompatible |

**Chosen — CPW-simple bitboard** ([CPW — Backward Pawns (Bitboards)](https://www.chessprogramming.org/Backward_Pawns_(Bitboards))):

```
stops        = wpawns << 8
wAttackSpans = wEastAttackFrontSpans(wpawns) | wWestAttackFrontSpans(wpawns)
bAttacks     = bPawnEastAttacks(bpawns) | bPawnWestAttacks(bpawns)
wBackward    = (stops & bAttacks & ~wAttackSpans) >> 8
```

- **Granularity.** Per pawn.
- **Rejected refinements** (noisy, deferred to tuning): half-open-file
  amplification; mutual-backwardness cancellation; rank restriction.
- **Weights (literature center).** MG −8, EG −12 (smaller than isolated).

### 1.4 Connected / phalanx / defended

| Term | Predicate |
|---|---|
| Phalanx | ≥2 same-color pawns on the same rank, adjacent files |
| Defended | pawn with a friendly pawn on file±1, rank−1 (white) |
| **Connected** | phalanx-member **or** defended — the M6.B umbrella term |

- **Rank-scaled bonus** (CPW cites the Stockfish-era shape): small at rank 2,
  large at rank 7 — reflects the increasing promotion threat.
- **Granularity.** Per connected pawn, indexed by the pawn's rank.
- **Phalanx vs chain-defended treated under one rank-scaled "connected"
  bonus** — standard practice; avoids over-complication.

| Rank (white) | MG | EG |
|---|---|---|
| 2 | +3 | +5 |
| 3 | +7 | +10 |
| 4 | +13 | +18 |
| 5 | +22 | +30 |
| 6 | +40 | +55 |
| 7 | +70 | +95 |

(approximate literature-center; M6.F tunes)

**Shipped indexing reconciliation.** The implemented `CONN_MG`/`CONN_EG` in
`src/eval/data.rs` are indexed by **0-based LERF rank index**, not chess rank:
`CONN_MG = [0, 0, 3, 7, 13, 22, 40, 70]`. So the `+3/+5` lands at LERF index 2
(chess rank 3), and a 2nd-chess-rank phalanx earns **0** (indices 0–1 are 0 —
a home-rank pawn formation is not yet a structural asset; the bonus accrues as
the phalanx advances). This shifts the table above one chess rank deeper than
its "Rank (white)" labels suggest. Not a correctness concern: ADR-0032 §6 sets
no normative values (Texel-tuned in M6.F) and the tests pin the shipped
constants directly. The labels above are the literature-center magnitudes; the
shipped table is the LERF-indexed embedding of them.

### 1.5 Passed-pawn detection (bonus is M6.C)

- **True passer.** No enemy pawn on the pawn's file or either adjacent file on
  any square *strictly ahead* (ranks above its own rank for white).
- **Bitboard** ([CPW — Passed Pawns (Bitboards)](https://www.chessprogramming.org/Passed_Pawns_(Bitboards))):

```
enemyFront = bFrontSpans(bpawns)
enemyFront |= eastOne(enemyFront) | westOne(enemyFront)
whitePassers = wpawns & ~enemyFront            // symmetric for black
```

- A doubled rear pawn is automatically *not* a passer (its own front pawn
  occupies its front-span). No special-case needed.
- **Candidate passer** notion — explicitly out of M6.B scope.
- **M6.B output.** `passed_pawns[White]`, `passed_pawns[Black]` bitboards
  cached in the pawn-hash entry; the rank/king-distance/path bonus is M6.C.

## 2. Pawn hash table

| Aspect | Decision basis |
|---|---|
| Hit rate | 95–99%+ for settled positions (CPW; TalkChess t=19582). |
| Size | **4 MiB** (roadmap default) — must fit L3 (TalkChess t=72195: a >L3 pawn hash is counter-productive). Apple Silicon shared L3 8–16 MB → 4 MiB safe. |
| Entry | key + MG i16 + EG i16 + `passed_pawns[2]` u64. ~24–32 B. |
| Key verify | 32-bit empirically collision-free (TalkChess t=19582); 64-bit for paranoia, simpler. |
| Replacement | **Always-replace** — universal recommendation for pawn hash. |
| Clearing | `ucinewgame` + per-bench-position, mirroring TT (ADR-0010). |

**Forward-compat for M6.E.** Pawn-shield file masks are a function of pawn
positions only → validly keyed by the pawn-Zobrist key, so M6.E can extend the
entry then. King-distance-to-passer is **not** pawn-only (king moves invalidate
it) → must be computed live outside the cache (M6.C concern).

## 3. Pawn-Zobrist substream

| Option | Mechanism | Trade-off |
|---|---|---|
| **(i) reuse Polyglot pawn keys** | accumulate `piece_key(pawn,sq)` over all pawns into a dedicated `pawn_zobrist` field | zero new vendored constants; reuses ADR-0009's audited key path + from-scratch/round-trip discipline; better cache locality (key row already loaded for the main zobrist on pawn moves) |
| (ii) separate 128-key set | new PRNG-generated `PAWN_KEYS[color][sq]` | literature default (auditability); +1 KB constants; independent collision stream |

- CPW recommends a dedicated incremental key "similar to the main TT,
  initialized from pawn squares only" — (ii) in spirit, but the
  reuse-Polyglot-subrange variant is an accepted alternative
  ([WBForum t=6644](http://www.open-aurec.com/wbforum/viewtopic.php?t=6644)).
- **Side-to-move excluded** — pawn structure is STM-independent.
- Collision independence (ii's main argument) is a weak concern here: a
  pawn-hash false hit perturbs the pawn score by tens of cp, bounded — it does
  not corrupt a TT-stored bestmove. Our always-on debug round-trip assert is a
  stronger correctness guarantee than auditability-by-separation.

### 3.1 Twelve flag-arm enumeration (roadmap-confirmed)

| Arm | Pawn-key delta |
|---|---|
| `Quiet` (pawn mover) | out@from, in@to |
| `DoublePush` | out@from, in@to |
| `Capture`, pawn mover | out@from(mover), in@to(mover) |
| `Capture`, pawn victim | out@to(victim) |
| `Capture`, pawn×pawn | both of the above |
| `EnPassant` | out@from(mover), in@to(mover), **out@capture_sq(victim), capture_sq ≠ to** |
| `*Promo` ×4 | out@from(pawn); no pawn in@to |
| `*PromoCapture` ×4, non-pawn victim | out@from(pawn); no pawn in@to |
| ~~`*PromoCapture` ×4, pawn victim~~ | **geometrically impossible** — a promo-capture victim is on the mover's back rank, where a pawn can never stand. The structural form's `if victim.kind == Pawn` is statically false here; **EP is the only two-pawn-removal pawn-key case** (M6.B test-author finding; plan §5 corrects this). |

12 = distinct `MoveFlag` discriminants that can touch a pawn key.

### 3.2 Silent-corruption failure modes (mandatory named tests)

- **EP-naive XOR.** Treating the EP victim as on `to` (not `capture_sq`)
  desyncs the key; the make/unmake round-trip does *not* restore it → the
  cache later returns an eval from a one-extra-pawn position. Wrong-answer,
  not a panic. Named EP round-trip test required.
- **Promo-capture, pawn victim — vacuous.** Originally listed as a failure
  mode; the M6.B test-author established it is geometrically impossible (the
  victim is on the mover's promotion/back rank, never a pawn). The structural
  §5 form is trivially correct for it. Coverage of the two-pawn-removal class
  is the EP arm only.

## 4. Tapering

| Term | Phase character | Why |
|---|---|---|
| Isolated | slightly EG-heavier | piece activity offsets in MG; pure target in EG |
| Doubled | ~symmetric | mobility hit both phases; small range anyway |
| Backward | slightly EG-heavier | same as isolated |
| Connected | EG-heavier at advanced ranks | rank-6/7 phalanx ≈ promotion threat in EG |

No normative CPW MG/EG-split table; the principle is "features whose value
changes with piece density get an MG/EG split" — pawn weaknesses are canonical.

## 5. Pitfalls / bug taxonomy

| Pitfall | Mitigation |
|---|---|
| doubled per-file vs per-extra-pawn | use rear-spans / `popcnt−1` per file |
| isolated+doubled stacking vs if-else | decide one convention, test it |
| backward base ignores half-open file | accept flat pre-tune; amplification is tuning |
| passed-pawn off-by-one (own rank in span) | shift-before-fill; e5 pawn span = e6/e7/e8 only |
| color/perspective sign | `score_for_side` helper: +white, −black into mg/eg_white |
| pawn-hash key vs TT key correlation | (ii) avoids it; (i) accepted with bounded-error rationale |
| forget per-bench-position pawn-hash clear | clear in the same `Search::reset` that clears nothing TT-side — pawn hash is search-owned |
| EP pawn-key at `to` not `capture_sq` | named EP round-trip test |
| ~~promo-capture pawn victim missed~~ | N/A — geometrically impossible (see §3.1/§3.4) |
| king-tropism cached in pawn hash | keep it live (M6.C); pawn hash holds pawn-only data |

## 6. Recommended literature-default weights (pre-Texel)

| Term | MG | EG | Granularity |
|---|---|---|---|
| Isolated | −10 | −20 | per pawn |
| Doubled | −10 | −15 | per extra pawn on file |
| Backward | −8 | −12 | per pawn (CPW-simple) |
| Connected | rank-scaled +3/+5 … +70/+95 | (see §1.4 table) | per pawn, by rank |
| Passed | — | — | detection only (bonus M6.C) |

Center-of-literature values; M6.F Texel tune is the primary calibration.

**Provenance caveat (added post-M6.B, from the subset-screen finding).**
These four values are **not from one co-calibrated source** — they are a
methodologically incommensurable pastiche: ISO/DBL are hand-picked midpoints
of wide cross-engine ranges (CPW qualitative + post-NNUE folklore); BWD is a
deliberately un-amplified placeholder; **CONN is one component of the
Stockfish-lineage `Connected[rank]` expression — the bare rank table stripped
of the phalanx/supported/opposed modulators it was co-designed and
jointly-tuned to be used with**. Empirically (ADR-0032 §7,
`docs/milestones/m6.b.md`): each term is individually SPRT-positive vs `M6.A`
(ISO +82.7, DBL +31.4, CONN +103.1) but **ISO+CONN together = −197.94** — a
catastrophic double-count, because ISO and CONN measure the *same
connectivity axis* with opposite sign and mismatched shape. An `(ISO+CONN)/2`
co-scale probe recovers ~+204 of that (pure over-magnitude) but **plateaus
at ≈0** — a global multiplier cannot fix a wrong-*shape* double-count.
**M6.B ships CONN-only**; M6.F must **reshape** (re-attach CONN's modulation
context + decorrelate the axis via joint Texel), not merely rescale. The
literature's standing position (Texel's method; CPW Automated Tuning) is that
eval terms are jointly fit *because* they overlap — never independently
sourced as done here pre-tune.

## Sources

CPW: Pawn Hash Table, Pawn Structure, Isolated Pawn(s), Doubled Pawn, Backward
Pawn(s), Passed Pawn(s), Connected Pawns, Pawn Spans, Tapered Eval, Incremental
Updates, CPW-Engine eval. TalkChess: t=29689, t=52300, t=19582, t=72195,
p=925582. WBForum t=6644. Little Chess Evaluation Compendium (Tsvetkov).
manuelfedele.github.io eval tutorial.
