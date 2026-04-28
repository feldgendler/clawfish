# M2 Research: Tournament Harness on Apple Silicon

*Prepared for the chess engine project. M2 UCI random-mover milestone and the SPRT plumbing it leaves behind for M3+.*

---

## Match Runner: `cutechess-cli` vs `fastchess`

### The shape of the choice

Two general-purpose UCI tournament runners are in current community use: **`cutechess-cli`** (the original; ships alongside the Cute Chess GUI) and **`fastchess`** (a from-scratch C++17 reimplementation by Disservin, started 2023). Older runners (`xboard -tourney`, etc.) are not in the conversation for new work in 2025–2026.

The decisive datapoint: **Stockfish's own Fishtest framework migrated from `cutechess-cli` to `fastchess` in 2024**. The migration tracking issue [official-stockfish/fishtest#2106](https://github.com/official-stockfish/fishtest/issues/2106) is closed, and the official Fishtest documentation now [documents `fastchess` as the worker-side runner](https://official-stockfish.github.io/docs/fishtest-wiki/Running-Fastchess.html). Fishtest is the largest distributed chess testing operation in the world; its choice is the strongest signal about which runner is actually maintained at scale.

### Apple Silicon install — verified

I ran `brew search` directly for both tools. Findings:

- `brew search cutechess` — no formula. Only `cutesdr` (an unrelated SDR app) appears.
- `brew search fastchess` — no formula. Only `fastrace` and `fastscripts`.
- `brew info --cask cute-chess` and `brew info --cask cutechess` — both return `Error: Cask ... is unavailable: No Cask with this name exists.`

So **neither tool is in homebrew core, and neither has a homebrew cask**. The single chess GUI cask is `chessx`; chess engines that *are* in core include `stockfish`, `lc0`, `fairy-stockfish`, `gnu-chess`, `xboard`, `cheops`, and `fairymax` ([brew search output](https://formulae.brew.sh/)). This forces the install path to be either a release-page download or a from-source build.

**`cutechess-cli` install on Apple Silicon.** Latest release **1.4.0, 2025-06-05** ([release page](https://github.com/cutechess/cutechess/releases)). Querying the GitHub Releases API directly, the v1.4.0 asset list is exactly three files: `cutechess-1.4.0-win64.exe`, `cutechess-1.4.0-win64.zip`, and `Cute_Chess-1.4.0-x86_64.AppImage`. **There is no macOS asset of any kind.** Every macOS user must build from source. Official instructions ([build wiki](https://github.com/cutechess/cutechess/wiki/Building-from-source)) call for `brew install cmake qt6 qt5compat` followed by `cmake -S . -B build && cmake --build build`. Qt6 alone is a ~1 GB dependency, and homebrew-Qt build issues have been reported as recently as [issue #689](https://github.com/cutechess/cutechess/issues/689). The CLI compiles fine; the GUI side is reportedly broken on macOS per the [chessprogramming wiki](https://www.chessprogramming.org/Cutechess-cli) ("Cutechess GUI does not run on macOS, but cutechess-cli does"). GUI-on-Mac is a separate problem treated in §6.

**`fastchess` install on Apple Silicon.** Latest release **`v1.8.0-alpha`, 2026-01-28** ([release page](https://github.com/Disservin/fastchess/releases)). Querying the Releases API, the asset list contains pre-built binaries for `linux-x86-64`, `linux-arm64`, `linux-risc-v`, `windows-x86-64`, `windows-arm64`, **`mac-x86-64`**, and **`mac-arm64`**. Releases happen quarterly (1.4 2025-04, 1.5/1.6 2025-09, 1.7 2025-11, 1.8 2026-01). The `fastchess-mac-arm64.tar` extracts to a single static binary — no Qt, no shared-library hunt. Build-from-source is also straightforward (`make -j CXX=clang++`, [README](https://github.com/Disservin/fastchess#readme)) but unnecessary. Despite the `-alpha` tag, this line is what Fishtest runs in production.

This asymmetry — fastchess ships a ready-to-run Apple Silicon binary, cutechess-cli does not ship a macOS binary at all — is by itself decisive for our use.

### Performance and scale

`fastchess` is engineered for SPRT from day one: the [README](https://github.com/Disservin/fastchess#readme) advertises tested operation up to 250 concurrent threads. `cutechess-cli` has historically had concurrency-related stalls under heavy load, which was one of the explicit motivations for the Fishtest migration ([issue #2106](https://github.com/official-stockfish/fishtest/issues/2106)). At M2 scale (a handful of games) and M3+ scale (SPRT in the thousands) both tools would handle the throughput; what matters is that fastchess is *the* tool the SPRT-at-scale crowd uses.

**Single-game overhead is judgment call** — no published head-to-head per-game benchmark. Informed intuition: process spawn + UCI handshake is microseconds; engine thinking dominates. Tool overhead is only visible at very short time controls under high concurrency, which is exactly where fastchess wins.

### CLI ergonomics, JSON config, PGN, SPRT — feature comparison

I read the [`fastchess` man.md](https://github.com/Disservin/fastchess/blob/master/man.md) and the [`cutechess-cli` help.txt](https://github.com/cutechess/cutechess/blob/master/projects/cli/res/doc/help.txt) directly. Both share most of their CLI surface — fastchess explicitly aims to be "Cute-Chess output compatible." Coverage:

| Capability | `cutechess-cli` 1.4.0 | `fastchess` 1.8.0-alpha |
|---|---|---|
| `-engine` inline config (`cmd`, `name`, `dir`, `arg`, `proto`, `option.X`, `tc`, `st`, `nodes`, `depth`, `restart`, `ponder`) | Yes ([help.txt](https://github.com/cutechess/cutechess/blob/master/projects/cli/res/doc/help.txt)) | Yes, same syntax ([man.md](https://github.com/Disservin/fastchess/blob/master/man.md)) |
| `engines.json` — *named engines pre-registered, referenced by `conf=NAME`* | Yes — `~/.config/cutechess/engines.json` on Linux / `~/Library/Application Support/Cute Chess/` style on macOS (judgment: not authoritatively documented for macOS) | **No.** Only inline `-engine` plus `-config` for *resuming a session*; no JSON registry of engines |
| `-sprt elo0=N elo1=N alpha=A beta=B` | Yes | Yes, plus `model=(normalized\|logistic\|bayesian)` |
| Cute-Chess time-control format `moves/min:sec+inc` | Yes | Yes (verbatim format) |
| PGN output, append, notation choice | Yes | Yes, with extended telemetry (`nodes`, `seldepth`, `nps`, `hashfull`, `tbhits`) |
| `-output format=cutechess` for compatibility | n/a | Yes — drop-in for tools expecting the older format |
| Resign/draw adjudication thresholds | Yes | Yes, identical syntax |
| Termination annotations (FIDE end states) | PGN draw labels include "Draw by 3-fold repetition", "Draw by fifty moves rule", "Draw by insufficient mating material", "Draw by stalemate"; PGN `[Termination]` tag emitted for adjudication / timeout / disconnection / illegal move ([cutechess#778](https://github.com/cutechess/cutechess/issues/778), [TalkChess discussion](https://talkchess.com/viewtopic.php?t=63888&start=2)) | Same scheme (claims Cute-Chess output compat); `-check-mate-pvs` extra-validates checkmate lines |
| UCI compliance checker | No | Yes (built-in, judgment: useful for shaking out our own engine's protocol bugs in M2) |
| Maintenance status late 2025 / early 2026 | Active but slow — 1.4.0 was the first release in **23 months** (1.3.1 was 2023-07-30). Bug-fix oriented. | Active — quarterly releases, 1.8.0-alpha shipped 2026-01-28 |

The notable structural difference is that **`cutechess-cli` has the `engines.json` registry and `fastchess` does not**. In practice this is small — a shell wrapper around `fastchess` gives us the same "named engine" affordance, with engine config under git rather than under `~/Library`. Fishtest uses fastchess inline, not via JSON ([Running Fastchess](https://official-stockfish.github.io/docs/fishtest-wiki/Running-Fastchess.html)).

### Compatibility with Cute Chess GUI for interactive use

The GUI is independent of which CLI runner you use — picking `fastchess` for scripted matches does not preclude installing Cute Chess GUI for watching games interactively. The two are orthogonal. GUI install on macOS is its own problem, treated in §6.

### Recommendation

**Use `fastchess` 1.8.0-alpha as the primary match runner, downloaded as the pre-built `mac-arm64` binary from the GitHub release.** Rationale: ships a ready-to-run Apple Silicon binary (cutechess does not ship a macOS binary at all), is what Fishtest currently runs, has a built-in UCI compliance checker that is genuinely useful for our M2 protocol shakedown, and has visibly more development velocity. **Fallback if fastchess proves unavailable:** build `cutechess-cli` from source via `brew install cmake qt6 qt5compat` + `cmake --build` per the [official build wiki](https://github.com/cutechess/cutechess/wiki/Building-from-source). The CLI surface is similar enough that any scripts we write will port across with minor edits.

---

## Engine Config Structure (and Its `engines.json` Equivalent for `fastchess`)

Since `fastchess` has no `engines.json` registry, the canonical pattern is to encode each engine's invocation as a shell-script fragment or a Make recipe and concatenate `-engine` arguments at the top level. Verbatim from the [Fishtest production invocation](https://official-stockfish.github.io/docs/fishtest-wiki/Running-Fastchess.html) and the [SPRT guide on OpenChess](https://open-chess.org/viewtopic.php?t=4360):

**Engine vs. itself (M2 self-play smoke):**

```bash
fastchess \
  -engine cmd=./target/release/clawfish name=clawfish-W \
  -engine cmd=./target/release/clawfish name=clawfish-B \
  -each proto=uci tc=10+0.1 \
  -rounds 2 -repeat \
  -pgnout file=bench/matches/m2-smoke.pgn notation=san \
  -log file=bench/matches/m2-smoke.log level=info engine=true
```

`-repeat` makes each "round" play both colors with the same opening, so `-rounds 2` produces 4 games. `-each` applies the same options to all engines (DRY). `engine=true` in `-log` records the raw UCI traffic — essential for debugging M2 protocol issues.

**Engine vs. Stockfish 18:**

```bash
fastchess \
  -engine cmd=./target/release/clawfish name=clawfish option.Hash=64 \
  -engine cmd=stockfish name=sf18 option.Hash=64 option.Threads=1 \
  -each proto=uci tc=10+0.1 \
  -openings file=resources/openings/8moves_v3.pgn format=pgn order=random \
  -rounds 50 -repeat \
  -draw movenumber=34 movecount=8 score=20 \
  -resign movecount=3 score=600 \
  -pgnout file=bench/matches/vs-sf18.pgn notation=san \
  -log file=bench/matches/vs-sf18.log
```

Required fields per engine: **`cmd`** is the only one that's strictly mandatory. `name` is needed if you don't want auto-generated names colliding when two instances of the same binary face each other. `proto=uci` defaults appropriately for our case (only UCI is supported by fastchess per the [man.md](https://github.com/Disservin/fastchess/blob/master/man.md)). Time control via `tc=` or `st=` or `nodes=` is required at the match level via `-each` if not per-engine.

For comparison, here's the equivalent **cutechess-cli `engines.json` entry**, which we'd need if we end up on the cutechess fallback ([sample format gathered from cutechess docs and TalkChess](https://www.chessprogramming.org/Cutechess-cli)):

```json
[
  {
    "name": "clawfish",
    "command": "./target/release/clawfish",
    "protocol": "uci",
    "workingDirectory": "/path/to/clawfish",
    "options": [{"name": "Hash", "value": "64"}]
  },
  {
    "name": "sf18",
    "command": "stockfish",
    "protocol": "uci",
    "options": [{"name": "Hash", "value": "64"}, {"name": "Threads", "value": "1"}]
  }
]
```

and the corresponding cutechess-cli invocation referencing them: `cutechess-cli -engine conf=chess -engine conf=sf18 -each tc=10+0.1 -rounds 4 -repeat -pgnout games.pgn`.

**Recommendation:** In our repo, do not commit any `engines.json`. Commit the `fastchess` invocation as a shell wrapper at `scripts/match.sh` and have it source named-engine recipes from `scripts/engines/` (one shell function per engine). That puts engine configs under git, avoids the `~/Library/Application Support/Cute Chess/engines.json` dance, and adapts to either runner with a one-line edit.

---

## Where Match Outputs Land in the Repo

The repo currently uses `bench/` for **committed**, milestone-stamped benchmark *summaries* (`bench/m1.g.md`, codified by [ADR-0010](../decisions/0010-benchmark-baseline-format.md)). Raw `criterion` artifacts go under `target/criterion/` and are gitignored. So `bench/` already has a clear "committed summary, not raw output" semantic.

Match outputs don't fit that semantic. PGN files are large, frequently regenerated, and are an *artifact-of-runs*, not a *summary-of-results*. Treating them like criterion raw output (gitignored, under `target/`) is the cleaner mental model.

**Proposed layout:**

```
bench/                          # committed summaries (current convention, ADR-0010)
  m1.g.md                       # existing
  m2.md                         # M2 exit-criterion smoke summary (committed)
  sprt/                         # M3+ SPRT result summaries (committed; dated)
    2026-XX-mX-Y-feature.md     # per-test result + bounds + LLR + games-played
target/matches/                 # gitignored; matches the target/criterion/ pattern
  smoke/                        # M2 self-play smoke runs
    m2-smoke-YYYY-MM-DD.pgn
    m2-smoke-YYYY-MM-DD.log
  sprt/                         # raw SPRT runs
    feature-name/               # one subdir per SPRT campaign
      games.pgn
      run.log
```

`target/matches/` piggybacks on the existing `/target` gitignore line, so no new gitignore entry is needed. **Wrinkle:** `cargo clean` wipes `target/`, including matches. Judgment call — match logs are regenerable, `cargo clean` is rare; acceptable. Alternative: a top-level `.matches/` directory, gitignored explicitly.

`bench/m2.md` (and `bench/sprt/*.md` later) follows ADR-0010's shape: human-readable Markdown, minimal numbers, hardware/version stamping, one file per milestone or SPRT campaign. The PGN is the raw evidence; the .md is the headline.

**Recommendation:** PGN/log output to `target/matches/{smoke,sprt}/...` (gitignored via the existing `/target` rule). Committed milestone summaries to `bench/m2.md` and `bench/sprt/*.md`, mirroring ADR-0010's pattern for criterion. No conflict with existing benchmark layout.

---

## Self-Play Smoke-Test Contract for M2

There is **no canonical practitioner standard** for a UCI-mover smoke — this is judgment-driven. Drawing on Fishtest's small-end conventions and common sense:

**Time control: `tc=10+0.1` (10 seconds base, 0.1 second increment).** The de-facto Fishtest ultra-fast preset ([OpenChess SPRT guide](https://open-chess.org/viewtopic.php?t=4360)). Fast enough that 4 games finish in under a minute; slow enough not to stress latency. For a random mover `nodes=1` would also work, but `tc=` keeps the same harness shape as later builds.

**Game count: 4 games via `-rounds 2 -repeat`.** `-repeat` plays each opening twice (one per color), so 4 games covers both sides. Sufficient to catch deterministic protocol bugs that only manifest as Black, or on the second game (state-leakage between games).

**Exit criteria, as a checklist:**

1. All 4 games terminate with a legal FIDE result: 1-0, 0-1, or 1/2-1/2 — never `*` (unterminated). Cute-Chess-style PGN encodes the cause as a draw label or `[Termination]` tag.
2. **No `Termination "illegal move"` and no `Termination "stalled connection"`** in any game's PGN ([cutechess docs on these tags](https://talkchess.com/viewtopic.php?t=63888&start=2)). These are the two failure modes that imply our engine misbehaved.
3. **No process crash or non-zero engine exit code.** fastchess reports both in the run log when `engine=true`.
4. **`fastchess`'s built-in UCI compliance checker emits zero warnings** for the engine binary across the 4 games (this is a fastchess-specific feature per the [README](https://github.com/Disservin/fastchess#readme); judgment call: definitely worth using for M2).
5. Logs contain no `info` lines that mention `unknown command` or `protocol error` from either side.

**Repeat the smoke at least twice** — once self-play, once against `stockfish` at depth 1 — before declaring M2 done. The Stockfish run validates that we speak UCI to a real opponent, not just to ourselves with matching bugs.

**Recommendation:** M2 exit criterion = "4-game self-play at `tc=10+0.1` plus 4-game vs. `stockfish` at `option.UCI_LimitStrength=true option.UCI_Elo=1320` (Stockfish's minimum), all 8 games legally terminated, no protocol errors, fastchess UCI-compliance checker silent." Captured as `bench/m2.md`.

---

## In-Tree Rust Integration Test (No External Runner)

A "spawn the release binary, drive UCI by hand" test. CI gate that the binary actually starts, completes handshake, and answers `go`. Not a substitute for the fastchess matches — a smaller smoke that runs in `cargo test` to catch gross regressions before any tournament runner is invoked.

### Pitfalls (real, not theoretical)

The [`std::process::Child` docs](https://doc.rust-lang.org/std/process/struct.Child.html) and [rust-lang/rust#45572](https://github.com/rust-lang/rust/issues/45572) document the canonical hazards:

1. **Pipe-fill deadlock.** If you write lots to stdin without draining stdout, the child can block writing to stdout (kernel pipe buffer ~64 KB on macOS) while we're blocked writing to stdin. *Mitigation:* read stdout on a dedicated thread. For UCI this is usually a non-issue per turn, but chatty `info` streams during long searches can trip it.
2. **Line buffering.** Rust's stdout, when connected to a pipe, switches to **block buffering by default**. Our engine *must* `stdout().flush()` after every UCI response line, or output stalls until ~4 KB accumulates. *Judgment:* confirm `src/uci/*` flushes per line. This is the single most common cause of "test hangs and I don't know why."
3. **macOS signal handling.** `Child::kill` sends SIGKILL, fine. Don't rely on EOF-on-stdin to exit the child — send `quit` and then `wait`. `wait()` closes stdin first per the docs, which is what we want.
4. **Race between reading `bestmove` and writing the next command.** If the driver writes the next `go` before draining the previous turn's `info` lines, they accumulate in the pipe. Not a deadlock for low-volume engines but causes subtle non-determinism. *Mitigation:* a reader thread owns stdout and pushes events to a channel; the main thread only writes stdin and pulls from the channel ([community pattern](https://www.nikbrendler.com/rust-process-communication/)).

### Canonical pattern, Rust skeleton

```rust
// tests/uci_self_play.rs
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;

fn spawn_engine() -> (std::process::Child, std::process::ChildStdin, Receiver<String>) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_clawfish"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn engine");
    let stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");

    // Reader thread owns stdout, pushes lines to the channel.
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines().flatten() {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    (child, stdin, rx)
}

fn recv_until<F: Fn(&str) -> bool>(rx: &Receiver<String>, pred: F) -> String {
    loop {
        let line = rx.recv_timeout(Duration::from_secs(5)).expect("engine timeout");
        if pred(&line) { return line; }
    }
}

#[test]
fn uci_handshake_and_one_move() {
    let (mut child, mut stdin, rx) = spawn_engine();
    writeln!(stdin, "uci").unwrap();
    recv_until(&rx, |l| l == "uciok");
    writeln!(stdin, "isready").unwrap();
    recv_until(&rx, |l| l == "readyok");
    writeln!(stdin, "position startpos").unwrap();
    writeln!(stdin, "go movetime 100").unwrap();
    let bm = recv_until(&rx, |l| l.starts_with("bestmove "));
    assert!(bm.split_whitespace().nth(1).is_some());
    writeln!(stdin, "quit").unwrap();
    let status = child.wait().expect("wait");
    assert!(status.success());
}
```

Key features of this skeleton:

- `env!("CARGO_BIN_EXE_clawfish")` resolves to the path of the release binary as built by Cargo. This is the [documented integration-test convention](https://doc.rust-lang.org/cargo/reference/environment-variables.html#environment-variables-cargo-sets-for-crates) and is preferred over hardcoding `target/release/clawfish`.
- `stderr(Stdio::inherit())` lets `eprintln!` from the engine surface in `cargo test` output for debugging. If the engine is verbose, switch to `Stdio::null()`.
- The reader thread + `Receiver<String>` pattern avoids both the pipe-fill deadlock (always reading) and the `bestmove` race (each command's response can be drained explicitly via `recv_until`).
- `recv_timeout(5s)` turns "engine hangs" into a test failure rather than a CI hang.
- `child.wait()` after `quit` confirms clean exit and reaps the process.

A full single-game self-play would add a loop: read `bestmove`, accumulate moves into `position startpos moves <list>`, send to the same engine again, repeat until `bestmove 0000` or until a hardcoded turn limit. Game termination detection without a full rules layer in the test driver is awkward — for a complete game test, **prefer the fastchess-driven smoke test from §4**. The in-tree integration test should be a handshake + N-move smoke, not a game-state-aware harness.

**Recommendation:** Ship one `tests/uci_smoke.rs` with the skeleton above; M2 exit criterion includes it passing in `cargo test`. Game-completion testing belongs to fastchess, not to the in-tree test.

---

## Cute Chess GUI Install on Apple Silicon

The user's chess strength is ~1000 and they won't be reading code; visual feedback during development is genuinely valuable. The question is real.

**The bad news, verified.** Cute Chess 1.4.0 has no macOS binary on its release page (verified via GitHub Releases API, §1). The README and [chessprogramming wiki](https://www.chessprogramming.org/Cutechess-cli) state "Cutechess GUI does not run on macOS." Unofficial builds exist — [chessengeria.eu hosts `CuteChess_1.3.0-beta4_Mac_Apple_Silicon.7z`](https://www.chessengeria.eu/post/cute-chess-for-mac) — but unsigned, blog-hosted, outdated.

The "officially supported" path: `brew install cmake qt6 qt5compat` plus `git clone … && cmake -S . -B build && cmake --build build` ([build wiki](https://github.com/cutechess/cutechess/wiki/Building-from-source)). Pulls ~1 GB of Qt and produces a `cutechess.app` reportedly unstable on macOS (TalkChess threads of crashes and missing widgets — judgment: usable for short demos, not long live-watch).

**Alternatives:**

- **`chessx`** is in homebrew as a cask (`brew install --cask chessx`) and runs natively. Primarily a database/PGN viewer with engine integration. *Judgment: probably the lowest-friction "see something move" path on macOS today.*
- **`xboard`** is a homebrew formula. Old-school X11 GUI; works, ugly.
- **Lichess external engine.** [Lichess external engine API](https://lichess.org/api#tag/External-engine-(draft)). More setup than is justified for our use.
- **PGN replay after the fact.** fastchess writes PGN as games complete; open in any board viewer (lichess.org/paste, ChessX, Mac App Store Chess.app). Asynchronous but zero-friction. *Judgment: probably the actual answer for our use case.*

**Recommendation:** Skip Cute Chess GUI install entirely. Use `chessx` from homebrew (`brew install --cask chessx`) for live engine sessions if needed, and rely on PGN replay through Lichess paste import or any standard board viewer for post-match review. Revisit Cute Chess GUI only if the user develops a specific need to watch live tournament games as they happen and the existing tools fall short.

---

## Recommended Harness Setup

Putting it all together.

### Chosen runner

**`fastchess` 1.8.0-alpha** (or whatever is current at install time on the project's quarterly cycle), as a pre-built `mac-arm64` binary downloaded from the [GitHub release page](https://github.com/Disservin/fastchess/releases/latest).

**Install on Apple Silicon (no `brew install` available):**

```bash
mkdir -p ~/.local/bin
curl -L https://github.com/Disservin/fastchess/releases/download/v1.8.0-alpha/fastchess-mac-arm64.tar -o /tmp/fastchess.tar
tar -xf /tmp/fastchess.tar -C ~/.local/bin
chmod +x ~/.local/bin/fastchess
fastchess --version    # confirm
```

(The user runs the actual install themselves; this is the recipe.)

**Fallback:** Build `cutechess-cli` from source via `brew install cmake qt6 qt5compat && git clone https://github.com/cutechess/cutechess && cd cutechess && cmake -S . -B build && cmake --build build`.

### Sample fastchess invocation (commit as `scripts/match.sh`)

No `engines.json` since fastchess does not consume one. The shell wrapper is the registry:

```bash
#!/usr/bin/env bash
# scripts/match.sh — M2 self-play smoke
set -euo pipefail
cargo build --release
mkdir -p target/matches/smoke
fastchess \
  -engine cmd="$PWD/target/release/clawfish" name=clawfish-W \
  -engine cmd="$PWD/target/release/clawfish" name=clawfish-B \
  -each proto=uci tc=10+0.1 \
  -rounds 2 -repeat \
  -pgnout file=target/matches/smoke/m2-smoke.pgn notation=san \
  -log file=target/matches/smoke/m2-smoke.log level=info engine=true
```

### Where match outputs land

- `target/matches/smoke/*.pgn`, `target/matches/smoke/*.log` — gitignored via existing `/target` rule. M2 self-play smoke runs.
- `target/matches/sprt/<feature>/*` — gitignored. M3+ SPRT campaigns.
- `bench/m2.md` — committed milestone summary (mirrors ADR-0010's pattern).
- `bench/sprt/<dated>.md` — committed SPRT result summaries (M3+).

### Smoke-test contract for M2 exit criteria

1. `scripts/match.sh` produces a PGN with **4 games**, all terminated with a legal FIDE result (1-0 / 0-1 / 1/2-1/2; not `*`).
2. **Zero `[Termination "illegal move"]` and zero `[Termination "stalled connection"]`** tags.
3. **Zero non-zero engine exit codes** in the match log.
4. `fastchess`'s built-in UCI compliance checker reports no warnings.
5. Repeat the same smoke with one engine swapped to `stockfish` (`option.UCI_LimitStrength=true option.UCI_Elo=1320`); same exit criteria. (Stockfish 18 is already installed at `/opt/homebrew/bin/stockfish` per the perft fixture work.)
6. The committed summary `bench/m2.md` records: hardware, fastchess version, Stockfish version, the two run logs' PGN paths, headline counts (W/L/D each side), and a one-paragraph "no anomalies observed" or list of caveats.

### Integration-test pattern

In-tree, in `tests/uci_smoke.rs`, the ~30-line skeleton from §5: spawn release binary via `env!("CARGO_BIN_EXE_clawfish")`, drive `uci → uciok → isready → readyok → position startpos → go movetime 100 → bestmove`, then `quit` and `wait`. Reader thread owns stdout to avoid pipe-fill deadlock; `recv_timeout` to convert hangs into test failures. Run as part of `cargo test`. Game-completion testing stays in fastchess, not in-tree.

### Cute Chess GUI install

**Skip.** The Cute Chess GUI does not have a working macOS binary and the from-source build is fragile. Use `brew install --cask chessx` if a live GUI is wanted, or replay fastchess's PGN output via Lichess paste import or any board viewer. Revisit only if the need becomes concrete.

---

## Sources

- [`cutechess/cutechess` GitHub repository](https://github.com/cutechess/cutechess)
- [Cute Chess 1.4.0 release notes](https://github.com/cutechess/cutechess/releases)
- [Cute Chess Building from source wiki](https://github.com/cutechess/cutechess/wiki/Building-from-source)
- [`cutechess-cli` help.txt](https://github.com/cutechess/cutechess/blob/master/projects/cli/res/doc/help.txt)
- [Ubuntu manpage for `cutechess-cli`](https://manpages.ubuntu.com/manpages/trusty/man6/cutechess-cli.6.html)
- [Cutechess-cli — Chessprogramming Wiki](https://www.chessprogramming.org/Cutechess-cli)
- [`Disservin/fastchess` GitHub repository](https://github.com/Disservin/fastchess)
- [`fastchess` man.md](https://github.com/Disservin/fastchess/blob/master/man.md)
- [`fastchess` releases page](https://github.com/Disservin/fastchess/releases)
- [Fishtest: Running Fastchess](https://official-stockfish.github.io/docs/fishtest-wiki/Running-Fastchess.html)
- [Fishtest issue #2106 — Replace cutechess-cli by fast-chess](https://github.com/official-stockfish/fishtest/issues/2106)
- [Fastchess SPRT guide on OpenChess](https://open-chess.org/viewtopic.php?t=4360)
- [SPRT — Chessprogramming Wiki](https://www.chessprogramming.org/Sequential_Probability_Ratio_Test)
- [dogeystamp: Elo and rigorous SPRT testing](https://www.dogeystamp.com/chess3/)
- [`std::process::Child` Rust docs](https://doc.rust-lang.org/std/process/struct.Child.html)
- [rust-lang/rust#45572 — `Command` deadlock when stdout pipe fills](https://github.com/rust-lang/rust/issues/45572)
- [Nik Brendler: Long-lived child processes in Rust](https://www.nikbrendler.com/rust-process-communication/)
- [Cargo environment variables (`CARGO_BIN_EXE_<name>`)](https://doc.rust-lang.org/cargo/reference/environment-variables.html)
- [TalkChess: PGN Termination tag values](https://talkchess.com/viewtopic.php?t=63888&start=2)
- [chessengeria.eu — unofficial Cute Chess Mac builds](https://www.chessengeria.eu/post/cute-chess-for-mac)
- [ADR-0010 — benchmark baseline format (in-tree)](../decisions/0010-benchmark-baseline-format.md)
