# M7.C research — SEE capture futility / delta pruning at frontier nodes (main search)

Prior-art survey for M7.C (negamax-side capture futility / delta pruning, the
ADR-0026 §10 + ADR-0040 §Consequences deferral). Honors ADR-0003 (no engine
source-repo reading; CPW / papers / TalkChess / blogs only). Compiled 2026-06-17.

## 1. Two distinct, complementary mechanisms

**1a. Alpha-relative capture futility (Heinz lineage / Stockfish).** Extend the
Heinz (1998) frontier-futility test to captures with an **MVV gain term**:

```
futility_base  = static_eval + positional_margin            // ~0..150 cp
futility_value = futility_base + value(captured_piece)       // MVV (piece value), NOT see()
if futility_value <= alpha:  skip the capture                // can't reach alpha even with the material
```

Gain term is the **captured piece value** (a fast O(1) upper bound on material
gain), not SEE. If even that upper bound falls short of alpha the move is hopeless.
This is textbook **delta pruning** carried into the main search.

**1b. SEE gate layered on top (the Stockfish pattern).** When 1a does *not* prune
(futility_value just above alpha), a second **alpha-relative SEE** check fires:

```
if !see_ge(mv, alpha - futility_base):  skip the capture     // exchange doesn't recover enough
```

i.e. prune iff `see(mv) < alpha - futility_base`  ⟺  `static_eval + see(mv) + margin < alpha`.
This catches captures whose victim is big enough to pass the MVV test but whose
realistic recapture-aware outcome (SEE) still can't reach alpha (e.g. an even
RxR-defended trade in a lost position). **This is where `see()` is consumed.**

Engines run **MVV first, SEE second** for efficiency: MVV is O(1) and filters the
obvious dead-loss positions, so `see()` rarely fires.

**1c. Pure absolute SEE-by-depth (a DIFFERENT, alpha-independent mechanism).**
`if see(mv) < -k·depth²  → skip` (captures; quiets use `-k·depth`), regardless of
alpha, for `depth ≤ ~8`. Lynx: noisy const ~50–80, quiet ~−50..−80. This is
node-reducing but **alpha-independent** — the same class as M7.B's flat
`QS_SEE_PRUNE_THRESHOLD = 0`, hence the same slow-TC regression risk. Research
recommends the alpha-relative form (1a/1b) **first**; this is a separate later lever.

## 2. Depth gating & margins

- Frontier (d1): margin ≈ minor (~125–150). MadChess `{150, 250, 400, 600}` at
  distances 1–4; Heinz d1 ≈ 125, d2 ≈ rook (~500). TalkChess: 200–300 cp "safe" at d1.
- Modern engines extend futility to d ≤ 7–9 with a linear margin `base + k·depth`.
- The margin in 1a/1b sits **on top of** the material term, so it is positional
  slack only and can be tighter than a pure-MVV-only margin.

## 3. Exemptions (consensus across all sources)

In check (skip all pruning) · alpha near mate (`|alpha| > MATE − MAX_PLY`) ·
move gives check (commonly exempt — discovered-check risk) · promotions (exempt) ·
TT/hash move (always search) · PV nodes (pruning off) · first legal move searched.

## 4. Slow-TC character — the key finding for this project

**Alpha-relative capture futility (1a/1b) is TC-safe by construction.** It only
fires when the position is already far below alpha; at slow TC deeper search finds
more refutations that *raise* alpha from below, making the condition *harder* to
satisfy — so it fires **less** at depth, not more. Alpha is the natural regulator.

This is the **opposite** character to M7.B's absolute threshold (alpha-independent,
fired more at the depths slow TC reaches → the 60+0.6 regression). ⇒ M7.C v1 needs
**no root-depth ramp**; the M7.B.2 mechanism is unnecessary here. (Kaufman/lucasart
on TalkChess t=46503: depth-based, not TC-based, conditioning is the right lever;
longer TC can tolerate *more* pruning because deeper search compensates.)

Other failure modes: pruning bad captures that give check (discovered checks SEE
misses); over-pruning in K+P / sparse endgames where small material decides (Delta
Pruning is conventionally disabled in late endgame). MVV-ordered capture lists allow
an early loop-exit (first failing capture ⇒ all later ones fail).

## 5. Reported Elo (not cleanly isolated in public prose)

MadChess 2.0 full futility package incl. captures (d1–4): **+54** (bullet). Frontier
futility (quiet-only, d1–2): **+25**/2000g (TalkChess t=74403). Bad-SEE capture
*reductions* in main search: **+4..+8**. Capture-futility-only at frontier is a
subset, estimated ~+10–30 but not published standalone. Net is SPRT-determined.

## 6. Recommendation adopted by M7.C v1 (see `docs/plans/m7.c.md`)

Mechanism 1 (alpha-relative), frontier only (d ≤ FFP_MAX_DEPTH = 1), two layers:
(a) MVV delta-prune `static_eval + victim + margin ≤ alpha` (sound optimistic
ceiling, no see()); (b) SEE refinement on materially-losing captures
(`victim < attacker`) `static_eval + see(mv) + margin ≤ alpha`. `CFP_MARGIN_D1 = 150`.
No ramp (alpha regulates). Promotions excluded; gives-check exemption deferred
(FFP-v1 precedent); the absolute SEE-by-depth lever (1c) deferred as a separate
future mechanism. SPRT vs `M7.B.2`, 2-seed, per-TC read (slow-TC watched but
expected safe per §4).

## Sources

CPW: Futility Pruning · Static Exchange Evaluation · Delta Pruning · Search
Progression. Heinz CSAIL dt/node23 (frontier) + node26 (extended). MadChess 2.0
build-37 futility blog. Lynx DeepWiki §3.5 pruning. Viri wiki (asteri.sm 2023-02-20).
TalkChess: t=37514 (SEE in main search), t=74403 (futility issues), t=59315
(futility), t=46503 (pruning by TC), t=41217 (qsearch SEE / check captures),
t=76750 (SEE pruning nodes). Mediocre Chess futility guide. Beowulf theory. hgm
deepfut. Stockfish PR #6266 title ("Simplify Capture Futility Pruning") — title/PR
metadata only, no source read.
