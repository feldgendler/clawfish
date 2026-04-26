# 0005 — Configurable strength reduction is a planned milestone

**Status:** Accepted, 2026-04-27

## Context

The user wants to play against the engine. At ~1000 Elo, a fully-strength version of even an early-milestone engine will be unbeatable. We need a way to dial the engine's strength down to a specified level — ideally a target Elo, possibly with human-like move selection.

Like NNUE, this is a planned milestone (M7), not an optional "maybe." Treating it as committed lets us be sure the architecture doesn't foreclose it.

## Decision

We commit to building a configurable strength dial. We do **not** build it now, and we **do not** add any abstraction for it now — but we ensure the architecture preserves the necessary insertion points.

The dial has two flavors, treated as separate milestones:

1. **Basic strength reduction (M7):** Mechanisms standard in the engine literature — depth/node-count caps, eval noise injection, top-N randomized move selection (pick from top *k* moves with probability proportional to score gap). Each mechanism is a deterministic function and unit-testable. The composition's actual Elo at each setting is calibrated empirically via self-play matches at fixed time controls.

2. **Human-like play (M12, optional):** A separate model (Maia-style — trained to predict human moves at a target rating band) plugged in via the same eval/policy hook used by NNUE. Distinct from M7: M7 is an engine playing weakly; M12 is an engine playing *like a human*.

UCI's standard options `UCI_LimitStrength` (boolean) and `UCI_Elo` (integer) are the canonical interface. We expose both, plus a finer-grained "skill level" if it's useful.

## Consequences

**Cost paid now:** zero. The required hooks are exactly the same ones ADR-0004 already mandates for NNUE-readiness:
- Eval is a discrete function call, replaceable / wrappable.
- Move selection is a discrete function call (rather than open-coded "pick the best score" inside search), so top-N randomization can intercept.

Both are good engineering anyway. Nothing extra to build until M7.

**Scheduling flexibility:** M7 is positioned after M6 (eval improvements) so eval-noise has a meaningful eval to perturb, but it can be pulled forward if the user wants to play against the engine sooner. Even a minimal version (depth cap only) shipped after M3 would be useful for casual play.

**Calibration is its own sub-project.** Self-play matches at various knob settings, fit a curve from "knob value" to "self-play Elo." Note: self-play Elo is *not* the same as Elo against humans — the engine's strengths and weaknesses differ from a human's. M12 (human-like play) is the only path to truly human-calibrated weakness; M7 is "weaker by self-play measurement."

## Rationale

This is the same pattern as NNUE: a future feature whose only architectural cost is structural discipline that's already justified for other reasons. Recording the commitment now ensures we don't accidentally violate it during search-side optimization (e.g. by inlining "pick best move" into the search loop and losing the move-selection hook).
