/// Maximum search depth in plies. PV table is sized to this constant.
pub(crate) const MAX_PLY: usize = 64;

/// Mate score: returned when a side is delivering mate. Ply-adjusted so faster
/// mates compare higher.
pub(crate) const MATE: i32 = 30_000;

/// Sentinel infinity value: wider than any MATE score; used for the initial
/// alpha/beta window.
pub(crate) const INF: i32 = 30_001;

/// Minimum mate score magnitude; used to distinguish mate scores from
/// centipawn scores in `score_to_uci`. A score with `|score| >= MATE_IN_MAX_PLY`
/// is a mate score.
pub(crate) const MATE_IN_MAX_PLY: i32 = MATE - MAX_PLY as i32; // 29_936

/// Minimum depth at which aspiration narrows the window. Below this, the
/// outer loop passes `(-INF, INF)` to negamax — same as M3.E behavior.
/// Depths 1–5 have prior-iteration scores that are too volatile to seed a
/// tight window (research §4 cites threshold values from 4 through 6 as
/// common; this project uses 6 because empirical tc=10+0.1 SPRT showed
/// threshold=4 regressed by ~22 Elo while threshold=6 gained ~+66 Elo
/// against the same baseline — fast-TC-only games reach ~depth 7, so
/// threshold=4 exposed too many shallow iterations to aspiration's
/// re-search overhead).
pub(crate) const ASPIRATION_MIN_DEPTH: u32 = 6;

/// First-try aspiration half-width in centipawns. Window is
/// `(prior - HALF_WIDTH, prior + HALF_WIDTH)`. CPW workhorse default;
/// roadmap §M4.D pins ±50 with a documented post-merge width-tune campaign
/// over ±25 / ±75 / ±100. Also the OFF-path fallback returned by
/// `aspiration_half_width` when `adaptive == false`.
pub(crate) const ASPIRATION_HALF_WIDTH: i32 = 50;

/// Default centi-K multiplier for the adaptive aspiration half-width formula
/// `half = clamp((k_centi * |d1 - d2| + 50) / 100, min, max)`. K=2.00 centers
/// the window at the proven fixed ±50 for the median ID score-delta (~25 cp).
pub(crate) const ASPIRATION_K_CENTI_DEFAULT: i32 = 200;

/// Default minimum adaptive aspiration half-width in centipawns. Prevents the
/// window from narrowing so tightly on stable positions that a single-cp
/// fluctuation causes a fail. Mirrors the item-5 hand-pick `MIN=25`.
pub(crate) const ASPIRATION_MIN_DEFAULT: i32 = 25;

/// Default maximum adaptive aspiration half-width in centipawns. Caps the
/// window on volatile positions, preventing a first-try window wider than ±50
/// on quiet positions (limit is ±250 ≈ 5 pawns). Mirrors the item-5
/// hand-pick `MAX=250`.
pub(crate) const ASPIRATION_MAX_DEFAULT: i32 = 250;

/// Default for the `Aspiration_Adaptive` UCI option. `true` enables the
/// SPRT-confirmed (+13.03 Elo) adaptive half-width path. Set to `false`
/// explicitly (via `setoption name Aspiration_Adaptive value false`) to
/// restore the fixed-±50 OFF path for comparison or diagnostics.
pub(crate) const ASPIRATION_ADAPTIVE_DEFAULT: bool = true;

/// Default lower bound of the adaptive-aspiration depth band. The adaptive
/// half-width formula is applied only when
/// `adaptive_min_depth ≤ depth ≤ adaptive_max_depth`. Defaults to
/// `ASPIRATION_MIN_DEPTH` (6) — the shallowest depth at which aspiration is
/// active at all — so the default band covers the entire aspiration domain.
/// UCI `Aspiration_AdaptiveMinDepth default 6`.
pub(crate) const ASPIRATION_ADAPTIVE_MIN_DEPTH_DEFAULT: u32 = ASPIRATION_MIN_DEPTH;

/// Default upper bound of the adaptive-aspiration depth band. Set to
/// `MAX_PLY as u32` (64) — tied to the symbol so a future `MAX_PLY` bump keeps
/// the default band a no-gate by construction. The ID loop hard-clamps to
/// `MAX_PLY − 1 = 63` on every path, so `[6, 64]` covers the entire reachable
/// depth domain and is a structural no-op gate. UCI
/// `Aspiration_AdaptiveMaxDepth default 64`.
pub(crate) const ASPIRATION_ADAPTIVE_MAX_DEPTH_DEFAULT: u32 = MAX_PLY as u32;

/// Parameters for the adaptive aspiration half-width feature. Stored in
/// `AlphaBetaMover` and set by `Engine::handle_setoption` via the
/// `set_aspiration_params` method (same worker-join discipline as `set_seed`).
///
/// **Production default: `adaptive == true`** (SPRT-confirmed +13.03 Elo).
/// When `adaptive == false` (explicit `setoption name Aspiration_Adaptive
/// value false`), `aspiration_half_width` returns the fixed
/// `ASPIRATION_HALF_WIDTH` constant for every input — byte-identical to the
/// pre-Unit-1 baseline.
///
/// The `adaptive_min_depth`/`adaptive_max_depth` band gate (Unit 2) is only
/// consulted when `adaptive == true` and `score_d2` is `Some` — the `!adaptive`
/// and `score_d2.is_none()` early-returns precede the band check, preserving
/// the OFF-path byte-identity invariant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AspirationParams {
    /// When `false`, `aspiration_half_width` returns the fixed fallback for
    /// every input, preserving byte-identical bench behavior.
    pub adaptive: bool,
    /// Centi-K multiplier (K × 100). UCI option `Aspiration_K default 200`.
    pub k_centi: i32,
    /// Minimum adaptive half-width in centipawns. UCI `Aspiration_Min default 25`.
    pub min: i32,
    /// Maximum adaptive half-width in centipawns. UCI `Aspiration_Max default 250`.
    pub max: i32,
    /// Lower bound of the adaptive-width depth band (inclusive). The formula is
    /// only applied at `depth ≥ adaptive_min_depth`. UCI
    /// `Aspiration_AdaptiveMinDepth default 6`.
    pub adaptive_min_depth: u32,
    /// Upper bound of the adaptive-width depth band (inclusive). The formula is
    /// only applied at `depth ≤ adaptive_max_depth`. UCI
    /// `Aspiration_AdaptiveMaxDepth default 64`.
    ///
    /// When `adaptive_min_depth > adaptive_max_depth` (inverted band), the
    /// band check is always true → fixed-50 everywhere. Accepted degenerate;
    /// falls back to baseline behavior.
    pub adaptive_max_depth: u32,
}

impl Default for AspirationParams {
    fn default() -> Self {
        Self {
            adaptive: ASPIRATION_ADAPTIVE_DEFAULT,
            k_centi: ASPIRATION_K_CENTI_DEFAULT,
            min: ASPIRATION_MIN_DEFAULT,
            max: ASPIRATION_MAX_DEFAULT,
            adaptive_min_depth: ASPIRATION_ADAPTIVE_MIN_DEPTH_DEFAULT,
            adaptive_max_depth: ASPIRATION_ADAPTIVE_MAX_DEPTH_DEFAULT,
        }
    }
}

/// Minimum depth at which NMP is attempted. Below this, the null-search's
/// `depth - 1 - R` would be ≤ 0, dispatching to qsearch — defeating the
/// cost/benefit calculation. ADR-0023 §2.
pub(crate) const NMP_MIN_DEPTH: u32 = 3;

/// NMP base reduction: `R = NMP_BASE_R + depth / NMP_DEPTH_DIVISOR`.
/// CPW workhorse default. ADR-0023 §1.
pub(crate) const NMP_BASE_R: u32 = 2;

/// NMP depth divisor in the reduction formula. ADR-0023 §1.
pub(crate) const NMP_DEPTH_DIVISOR: u32 = 6;

/// Upper depth bound for reverse-futility pruning. At depths above this,
/// `static_eval - margin*depth >= beta` is rarely true (the margin grows
/// faster than realistic eval surplus), and the tactical-blindness risk
/// from a depth-7+ refutation grows. Stockfish DD historical: `depth < 7`
/// (i.e., depth ≤ 6); ADR-0024 §1.
pub(crate) const RFP_MAX_DEPTH: u32 = 6;

/// Linear coefficient for reverse-futility pruning's depth-scaled margin.
/// `margin = RFP_MARGIN_PER_DEPTH * depth`. At depth=1, 100 cp (≈ one
/// pawn); at depth=6, 600 cp (≈ a rook). Conservative v1 starting value;
/// CPW workhorse alternative is 150 (post-landing SPRT-tune candidate).
/// ADR-0024 §1.
pub(crate) const RFP_MARGIN_PER_DEPTH: i32 = 100;

/// Minimum depth at which LMR is considered. Below this, reduced searches are
/// too shallow to justify the complexity and tend to degenerate into qsearch.
/// M5.C plan / forthcoming ADR-0025.
pub(crate) const LMR_MIN_DEPTH: u32 = 3;

/// Quiets are indexed 1-based within the node's quiet-only ordering. The
/// first quiet searched at a node (index 1) is never reduced; reductions may
/// begin at the second quiet (index 2).
pub(crate) const LMR_MIN_QUIET_INDEX: u32 = 2;

/// Additive base term for the M5.C log-log LMR formula.
pub(crate) const LMR_BASE_OFFSET: f64 = 0.99;

/// Divisor in the M5.C log-log LMR formula.
#[allow(clippy::approx_constant)] // conservative-band placeholder; SPRT-tunable
pub(crate) const LMR_LOG_DIVISOR: f64 = 3.14;

/// Quiets with history scores at or above this threshold are trusted and are
/// exempt from LMR in M5.C v1.
pub(crate) const LMR_HIGH_HISTORY_THRESHOLD: i16 = 4_096;

/// FFP node-level depth ceiling (M5.D — ADR-0026 §4). At depth > FFP_MAX_DEPTH,
/// FFP does not fire.
///
/// **v2: 1** (frontier-only, Heinz 1998 original formulation). The v1 setting
/// (`FFP_MAX_DEPTH = 2`) layered "extended futility" at depth 2 on top, but
/// the v1 mixed-TC SPRT (M5.D landing) showed strong slow-TC regression
/// implicating depth-2 FFP as the cause: per-TC bimodal pattern with positive
/// fast TC (10+0.1: 56.5%, 20+0.2: 68.3%) and negative slow TC (40+0.4:
/// 30.2%, 60+0.6: 40.6%). Restricting to depth 1 (Heinz's classical scope)
/// keeps the cheap frontier prune but drops the deeper-search tactical-
/// blindness risk. See [`bench/sprt/2026-05-06-m5.d-vs-m5c-mixed-tc.md`] and
/// the M5.D retrospective for the v1 → v2 reasoning.
///
/// `FFP_MARGIN_D2 = 150` and `FFP_MARGIN_D3 = 250` are kept as named
/// constants (forward compat) but inactive at v2.
pub(crate) const FFP_MAX_DEPTH: u32 = 1;

/// FFP margin at depth 1 (frontier nodes). 100 cp ≈ one pawn. Conservative v1
/// per ADR-0026 §4. TalkChess t=74403 successful at 100 cp / d=1 in `{100, 150}`.
pub(crate) const FFP_MARGIN_D1: i32 = 100;

/// FFP margin at depth 2 (pre-frontier / Heinz "extended futility"). 150 cp.
/// **Inactive at v2** (FFP_MAX_DEPTH = 1 keeps depth 2 from firing). Defined
/// here so a post-tune `FFP_MAX_DEPTH = 2` revival can re-activate it without
/// churn. ADR-0026 §4.
pub(crate) const FFP_MARGIN_D2: i32 = 150;

/// FFP margin at depth 3 (pre-pre-frontier / "limited razoring"). **Inactive
/// at v2** (FFP_MAX_DEPTH = 1 keeps depth 3 from firing). Defined here so a
/// post-tune `FFP_MAX_DEPTH = 3` SPRT can activate it without churn.
/// ADR-0026 §4.
pub(crate) const FFP_MARGIN_D3: i32 = 250;

/// Compile-time invariant: at v1, FFP fires at `depth ≤ 2` and LMR fires at
/// `depth ≥ 3`, so the two pruning paths cannot co-fire at any node. This
/// assertion is load-bearing as a tripwire — a future tuning that raises
/// `FFP_MAX_DEPTH` to overlap with `LMR_MIN_DEPTH` MUST update ADR-0026 §6
/// (the per-quiet ordinal semantics question) and remove this assertion in
/// the same patch. Mirrors ADR-0025 §3's `LMR_HIGH_HISTORY_THRESHOLD <=
/// MAX_HISTORY` invariant pattern.
const _: () = assert!(FFP_MAX_DEPTH < LMR_MIN_DEPTH);

/// M5.G singular-extension minimum remaining depth. Below this, SE is too
/// shallow to justify the verification-search cost, and the literature
/// majority sets the threshold here. ADR-0029 §1.
pub(crate) const SE_MIN_DEPTH: u32 = 6;

/// M5.G singular-extension margin per ply. `singular_beta = tt_score - depth *
/// SE_MARGIN_PER_DEPTH`. Xiphos / Ethereal defaults; conservative starting
/// value. Post-landing SPRT-tune candidate (tuning backlog: try 2). ADR-0029 §2.
pub(crate) const SE_MARGIN_PER_DEPTH: i32 = 1;

/// M5.G singular-extension TT-entry depth tolerance. SE fires only when the
/// TT entry's stored depth is at least `depth - SE_TT_DEPTH_DELTA`, i.e. the
/// TT score is "fresh enough" to be evidence at the current depth. Xiphos
/// default. ADR-0029 §3.
pub(crate) const SE_TT_DEPTH_DELTA: u32 = 3;

// Depth-disjointness invariants. These are load-bearing tripwires: a future
// tuning that moves FFP_MAX_DEPTH or SE_MIN_DEPTH into overlap MUST update
// the corresponding ADRs and remove the violated assertion.
//
// FFP ≤ LMR boundary (ADR-0026 §6 + ADR-0025 §3):
const _: () = assert!(FFP_MAX_DEPTH < SE_MIN_DEPTH);

/// Volatility-responsive first-try aspiration half-width (Unit 1 + Unit 2).
///
/// Early-return order is load-bearing — each guard preserves a byte-identical
/// sub-path from prior milestones:
/// 1. `score_d2.is_none()` → fixed-50 (no completed prior-prior iteration to
///    delta against; first adaptive-eligible ID iteration always falls here).
/// 2. `!params.adaptive` → fixed-50 (OFF-path byte-identical to pre-Unit-1;
///    this guard precedes the band check so an explicit `adaptive=false` bench
///    never touches the band fields).
/// 3. Band gate (`depth < adaptive_min_depth || depth > adaptive_max_depth`) →
///    fixed-50 (Unit 2: restrict the adaptive formula to the `[min_depth,
///    max_depth]` closed interval). An inverted band (`min > max`) makes this
///    predicate permanently true, falling back to fixed-50 everywhere — the
///    accepted degenerate documented in plan §2.3.
/// 4. Adaptive formula:
///    `clamp((k_centi * |score_d1 - d2| + 50) / 100, min, max)`.
///    The `+ 50` rounds half-away-from-zero before the integer division by 100,
///    giving deterministic platform-independent arithmetic.
pub(crate) fn aspiration_half_width(
    score_d1: i32,
    score_d2: Option<i32>,
    params: &AspirationParams,
    depth: u32,
) -> i32 {
    let Some(d2) = score_d2 else {
        return ASPIRATION_HALF_WIDTH;
    };
    if !params.adaptive {
        return ASPIRATION_HALF_WIDTH;
    }
    if depth < params.adaptive_min_depth || depth > params.adaptive_max_depth {
        return ASPIRATION_HALF_WIDTH;
    }
    ((params.k_centi * (score_d1 - d2).abs() + 50) / 100).clamp(params.min, params.max)
}

/// First-try aspiration window. Returns `(-INF, INF)` (equivalent to no
/// aspiration) when:
///
/// 1. `depth < ASPIRATION_MIN_DEPTH` — too-shallow iteration; prior
///    score is unstable.
/// 2. `prior_score == None` — no prior iteration to seed from (the first
///    ID iteration of the current `go`).
///
/// Otherwise uses `aspiration_half_width` to compute the first-try half-width
/// from `params` and the two most recent completed ID scores, then returns
/// `(prior - half, prior + half)`. Mate-score `prior_score` values produce a
/// window straddling the mate boundary; `widen_after_fail` handles the
/// resulting first-try fail via the asymmetric full-window re-search
/// (research §7.2).
///
/// When `params.adaptive == false` (explicit OFF), `aspiration_half_width`
/// returns exactly `ASPIRATION_HALF_WIDTH` for any input, so this function is
/// byte-identical to the pre-Unit-1 signature for every (prior, depth) pair.
///
/// Pure function. Pinned by AS1–AS5b.
pub(crate) fn aspiration_window(
    prior_score: Option<i32>,
    prior_prior_score: Option<i32>,
    params: &AspirationParams,
    depth: u32,
) -> (i32, i32) {
    if depth < ASPIRATION_MIN_DEPTH {
        return (-INF, INF);
    }
    let Some(prior) = prior_score else {
        return (-INF, INF);
    };
    let half = aspiration_half_width(prior, prior_prior_score, params, depth);
    (prior - half, prior + half)
}

/// Two-tier asymmetric widening on aspiration failure. Computes the
/// re-search window from the failed-try's returned score and the failed
/// try's `(prev_alpha, prev_beta)` window.
///
/// **Fail-high** (`returned >= prev_beta`): re-search `(returned, +INF)` —
/// keep the proved lower bound as the new alpha; widen the upper side.
///
/// **Fail-low** (`returned <= prev_alpha`): re-search `(-INF, returned)` —
/// keep the proved upper bound as the new beta; widen the lower side.
///
/// **Caller contract**: only called when `(returned >= prev_beta) ||
/// (returned <= prev_alpha)`. The window-contained case is short-circuited
/// by the caller. Pinned by AS9b (debug panic if invariant violated).
///
/// Pure function. Pinned by AS6–AS9b.
pub(crate) fn widen_after_fail(returned: i32, prev_alpha: i32, prev_beta: i32) -> (i32, i32) {
    debug_assert!(
        returned >= prev_beta || returned <= prev_alpha,
        "widen_after_fail called with window-contained score: \
         returned={returned} prev_alpha={prev_alpha} prev_beta={prev_beta}"
    );
    if returned >= prev_beta {
        (returned, INF)
    } else {
        // returned <= prev_alpha by the debug_assert
        (-INF, returned)
    }
}

/// NMP depth reduction. Returns `NMP_BASE_R + depth / NMP_DEPTH_DIVISOR`
/// (= `2 + depth/6`). Pure function. Extracted as a named helper so
/// mutations on the formula constants are directly unit-testable
/// (M3.D `negate_window` precedent).
pub(crate) fn null_move_reduction(depth: u32) -> u32 {
    NMP_BASE_R + depth / NMP_DEPTH_DIVISOR
}

/// RFP depth-scaled margin. Returns `RFP_MARGIN_PER_DEPTH * depth as i32`.
/// Pure function. Extracted as a named helper so mutations on the formula
/// constants are directly unit-testable (M3.D `negate_window` /
/// M3.E `aborted_fallback_result` / M5.A `null_move_reduction` precedent).
///
/// `RFP_MARGIN_PER_DEPTH * depth as i32` cannot overflow `i32` at any depth
/// below ~21M; the `depth <= RFP_MAX_DEPTH = 6` gate makes this trivially safe.
pub(crate) fn reverse_futility_margin(depth: u32) -> i32 {
    RFP_MARGIN_PER_DEPTH * depth as i32
}

/// LMR base reduction. Inputs are `(depth, quiet_index)` where `quiet_index`
/// is 1-based within the quiet-only ordering at the current node. Returns
/// `0` for `depth < LMR_MIN_DEPTH` or `quiet_index < LMR_MIN_QUIET_INDEX`
/// (in-domain guard pinned by tests; do not rely on caller-side gates
/// alone). Otherwise computes `floor(LMR_BASE_OFFSET + ln(depth) *
/// ln(quiet_index) / LMR_LOG_DIVISOR)` and clamps to `0..=(depth - 2)` so
/// the reduced child is always at least depth 1. Extracted as a named
/// helper so formula mutations are directly unit-testable (M3.D
/// `negate_window` precedent). ADR-0025 §4.
pub(crate) fn late_move_reduction(depth: u32, quiet_index: u32) -> u32 {
    if depth < LMR_MIN_DEPTH || quiet_index < LMR_MIN_QUIET_INDEX {
        return 0;
    }

    let raw = LMR_BASE_OFFSET + (depth as f64).ln() * (quiet_index as f64).ln() / LMR_LOG_DIVISOR;
    let reduction = raw.floor() as u32;
    reduction.clamp(0, depth.saturating_sub(2))
}

/// Per-depth FFP margin (M5.D — ADR-0026 §4 + §5). Returns 0 outside
/// `[1, FFP_MAX_DEPTH]`.
///
/// Defining the depth-3 entry here (even though `FFP_MAX_DEPTH = 2` disables
/// it at v1) keeps the constant inventory consistent with the roadmap's CPW
/// reference and lets a future SPRT raise `FFP_MAX_DEPTH` without revisiting
/// the table.
pub(crate) fn frontier_futility_margin(depth: u32) -> i32 {
    match depth {
        1 => FFP_MARGIN_D1,
        2 => FFP_MARGIN_D2,
        3 => FFP_MARGIN_D3, // inactive until FFP_MAX_DEPTH raised
        _ => 0,
    }
}

/// FFP gate test combined with proved fail-soft upper bound (M5.D — ADR-0026
/// §5). `static_eval` and `alpha` are STM-relative centipawn scores at the
/// **parent** node (pre-move).
///
/// Returns `Some(static_eval + margin)` iff `depth ∈ [1, FFP_MAX_DEPTH]` AND
/// `static_eval + margin <= alpha` (saturating addition); `None` otherwise.
/// The `Some` payload is the FFP-proved fail-soft upper bound on the move's
/// true score, guaranteed `<= alpha` by the gate. The call site uses it to
/// floor `best` (ADR-0026 §7) without a separate recompute.
///
/// **One helper, two responsibilities by design.** Splitting into a bool
/// gate + a separate i32 bound calc would create a saturation-overflow
/// asymmetry: the gate must use `saturating_add` (else `+`-overflow could
/// let the bound test pass when arithmetic says no), and the call-site
/// contribution must reuse the same saturated value (else the `+`-overflow
/// could re-emerge at the call site). The single helper closes the
/// asymmetry by computing the bound once.
///
/// **Domain guard.** The depth-range check is helper-level defense-in-depth
/// (M5.C `late_move_reduction` precedent): the helper returns `None` at
/// `d == 0` or `d > FFP_MAX_DEPTH` even with mathematically passing
/// `margin`/`alpha` — guards against a refactor that drops the call-site
/// `depth <= FFP_MAX_DEPTH` gate.
///
/// **Overflow defense.** `saturating_add` defends against `i32` overflow
/// when `static_eval` is near `MATE`. The node-level gate's
/// `alpha.abs() < MATE_IN_MAX_PLY` makes that case unreachable in
/// production, but the helper is `pub(crate)` and unit-tested independently
/// — overflow on a unit-test edge would be a confusing failure.
///
/// **Inequality.** `<=` not `<`: a move whose true score could exactly
/// equal alpha does not improve alpha (fail-soft requires strict
/// improvement); pruning at equality is the standard CPW form.
pub(crate) fn ffp_pruned_bound(static_eval: i32, depth: u32, alpha: i32) -> Option<i32> {
    if depth == 0 || depth > FFP_MAX_DEPTH {
        return None;
    }
    let bound = static_eval.saturating_add(frontier_futility_margin(depth));
    if bound <= alpha { Some(bound) } else { None }
}

/// Singular-extension β (M5.G — ADR-0029 §2). `tt_score - depth * SE_MARGIN_PER_DEPTH`,
/// floored at `-(MATE - 1)` to avoid mate-score wrap when `tt_score` is near `-MATE`.
/// Pure function. The caller-side `tt_score.abs() < MATE_IN_MAX_PLY` gate
/// (§5 of `singular_extension_eligible`) makes the floor a defense-in-depth
/// invariant rather than a hot-path concern, but the floor is unit-tested
/// independently.
pub(crate) fn singular_beta(tt_score: i32, depth: u32) -> i32 {
    let raw = tt_score.saturating_sub(SE_MARGIN_PER_DEPTH * depth as i32);
    raw.max(-(MATE - 1))
}

/// Singular-extension verification-search remaining depth (M5.G — ADR-0029 §3).
/// `(depth - 1) / 2` (integer division), with a debug-assert that
/// `depth >= SE_MIN_DEPTH`. At SE_MIN_DEPTH = 8 this yields verif_depth = 3.
/// Pure function.
///
/// **Caller obligation.** `depth >= SE_MIN_DEPTH` is the contract.
/// `(0 - 1)` underflows in `u32` (panicking in debug, wrapping in release —
/// `(0_u32.wrapping_sub(1)) / 2 = u32::MAX / 2`, which would then propagate
/// into a deep verification recursion). The eligibility predicate's clause 5
/// (`depth >= SE_MIN_DEPTH`) is the only call-site protection; the helper's
/// `debug_assert!` pins this in debug builds.
pub(crate) fn verification_depth(depth: u32) -> u32 {
    debug_assert!(
        depth >= SE_MIN_DEPTH,
        "verification_depth: caller must have passed the SE_MIN_DEPTH gate; depth={depth}"
    );
    (depth - 1) / 2
}
