---
name: my-explain
description: Walk through a structured chunk of project material (a plan section, an "in scope" / "out of scope" list, a doc bullet list, or the most recent assistant response) line-by-line in a specific Q&A discipline — the user reads each explanation and either asks follow-up questions or says "continue" / "next line" to advance. Triggered when the user runs `/my-explain` with or without arguments.
---

# my-explain

A guided line-by-line walkthrough discipline. The user is a casual chess player but a serious engineer who does not read Rust by design (per `CLAUDE.md`); chat explanations are the primary signal for catching bugs and validating decisions. This skill exists to make that signal high-fidelity for any structured chunk of project material.

## Invocation

- `/my-explain <target>` — walk through the target. Examples:
  - `/my-explain M3.C` → "In scope" of `docs/plans/m3.c.md`.
  - `/my-explain M3.C out-of-scope` → "Out of scope" (§1) of the same plan.
  - `/my-explain ADR-0016` → settled-commitments list of `docs/decisions/0016-search-structure.md`.
  - `/my-explain docs/decisions/0014-static-eval-white.md §3` → that specific section.
- `/my-explain` (no argument) — walk through the **most recent assistant response in this conversation**, line-by-line. This is the "you just explained X; now I want to dig into your own bullets one at a time" mode.

If the target is ambiguous, ask which list/section to walk via `AskUserQuestion`. Don't guess.

## The discipline

For each discrete line / bullet / item in the target, in document order:

1. **Repeat the line verbatim** as a blockquote at the top of your message — so the user sees exactly which line is being unpacked, with no interpretation drift.
2. **Explain the chess-programming-specific terms.** Be unusually rigorous: name each term, define it, give the alternative or contrast that makes the meaning clear. Use worked numerical examples where applicable.
   - **Skip general programming / Rust language terms** (e.g. `i32`, `Vec`, `Arc<Mutex<...>>`, `pub(crate)`, `#[derive]`) unless the user explicitly opts in. The user is a serious engineer — those are noise. Chess-engine terminology, UCI protocol, search algorithms, eval semantics, and tradeoff reasoning are signal.
3. **Explain the design choices and tradeoffs.** Why was this approach chosen over alternatives? What does it cost? What does it enable? What's deferred and where does the deferred fix land?
4. **Stop and wait for input.** End the message with an explicit prompt — `Continue?`, or `Stop. Ask anything; say 'continue' for the next line.`, or equivalent. The prompt is mandatory; without it the user has no clear handoff. **Do not proceed to the next line until the user signals.**

**This discipline overrides project auto-mode behavior.** Auto-mode normally says "minimize interruptions, prefer action over planning." `/my-explain` is the explicit opposite — the user opted into a stop-per-line cadence by invoking the skill. Honor the stops even when auto-mode is active. Course corrections from the user remain normal input; "continue" / "next line" remain the advance signals.

The user will then either:

- **Ask a follow-up question** about the current line — drill into a specific term, challenge a claim, ask for a worked example, ask about an alternative design. Answer the question fully, then stop again at the same line. The user may ask multiple follow-ups before advancing.
- **Say "continue" / "next line" / "next" / "go on"** — advance to the next line and repeat the four steps above.
- **Skip ahead** ("skip to line N", "jump to the part about X") — honor the request.
- **Stop the walkthrough** ("stop", "done", "I'm good") — end gracefully.
- **Reply ambiguously** ("huh?", "wait", a question that doesn't clearly belong to this line or any other) — clarify rather than assume. Ask whether they want a follow-up on the current line, want to advance, or want something else. Don't auto-advance on unclear input.

If a user question would be better answered by a future line in the walkthrough, give a brief preview (one or two sentences) but don't go deep — note that the next-line discussion will cover it, and stay focused on the current line. This preserves the line-by-line cadence without leaving the user's question unaddressed.

When the list is exhausted, give a short summary (one line per item) recapping what was covered, and offer to walk a related section ("we covered 'In scope'; want to do 'Out of scope' next?").

## Tone and depth calibration

- Match the depth to the user's questions. If they ask sharp questions, go deep; if they say "continue" quickly, keep subsequent items at similar depth without padding.
- **Don't oversimplify chess-programming concepts.** The user is ~1000 Elo at chess but a serious engineer who reads chess literature. Use the proper terms (alpha-beta, fail-soft, MVV-LVA, MDP, PV-node, transposition table, Zobrist) without dumbing them down — define on first use within the walkthrough, then use freely.
- **Use worked numerical examples whenever a formula or constant appears.** PeSTO values, MATE = 30000, mate-in-N conversions, multipliers, cadences — show the actual numbers in tables.
- **Acknowledge errors immediately.** If the user catches a flaw in your explanation, concede explicitly, give the corrected version, and continue. Don't paper over.
- **Surface plan inconsistencies.** If during the walkthrough you notice a contradiction between the plan and an ADR, between two sections of the same doc, or between the plan and observable code, flag it as a should-fix concern for plan-review or final-review. The skill exists partly to make plan-review through chat possible.
- **Distinguish "current scope" from "deferred."** When a line in the in-scope section depends on something deferred to a later phase, say so — name the future phase and what it adds. The user uses this to build the right mental model of what "M3.C complete" actually means vs what "M3 complete" will mean.

## What NOT to do

- Don't batch multiple lines into one message ("here's the whole list at once"). The walkthrough's value is in the user being able to interject after each. Batching defeats the discipline.
- Don't repeat the line as a paraphrase or summary — quote it verbatim. The user reads the quote to lock onto the exact phrasing they're about to dig into.
- Don't explain general-programming terms unprompted. (Exception: Rust-specific concurrency primitives if the line's *behavior* hinges on them, e.g. `AtomicBool` ordering for cancellation. Then it's chess-engine-relevant, not language trivia.)
- Don't ask for permission to start. Once invoked, immediately go to line 1 of the target. The user already opted in by typing `/my-explain`.
- Don't truncate. If a line genuinely needs 800 words to unpack (e.g. mate-distance pruning across multiple cases), spend them. The user prefers depth over brevity for unfamiliar material.
- Don't leave open questions hanging. If you reference a future feature ("M3.D's qsearch will fix this"), give enough context that the reference is meaningful — don't assume the user has the future plan loaded.

## Why this discipline exists

Per `CLAUDE.md` "User profile":

> Does not know Rust, by design. The language choice is a self-imposed gatekeeper to keep the work vibe-coded. The user will not be inspecting code line-by-line; chat explanations are the primary signal. Be unusually rigorous in chat about explaining decisions and surfacing risks, since bugs cannot be caught by reading.

A line-by-line walkthrough with stop-points is the highest-fidelity way to surface those risks. Each stop is an opportunity for the user to challenge an assumption, catch an inconsistency, or course-correct a design. Bullet-list summaries don't have stop-points; chat-style narration runs ahead. The `/my-explain` discipline forces the explanation to slow down to the user's reading and questioning cadence.

## Default target resolution

When `/my-explain` is invoked with no argument, walk the **most recent assistant response** in this conversation, treating each top-level bullet, numbered item, or section heading as a "line." If the previous response was prose without discrete items, identify a natural list within it (e.g. a comparison table) and walk those rows. If no list-like structure exists, tell the user the previous response isn't suitable and ask what they'd like to walk instead.
