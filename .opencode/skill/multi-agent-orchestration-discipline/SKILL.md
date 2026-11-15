---
name: "Multi-Agent Orchestration Discipline"
description: "Use when orchestrating subagent waves on a large repo. Prevents step-limit loss, duplicate writers, disk exhaustion, and false \"done\" reports."
metadata:
  opencode/agent: true
---
# Multi-Agent Orchestration Discipline

## When to use

You are a root/coordinator agent delegating file edits and build runs to
subagents on a repo with long compile times (e.g. `/root/escape-agent`). Use this
before spawning the first worker of a wave, and again before declaring the wave
done.

## Rule 1 — Slice by file ownership, never by topic

Each worker gets an explicit, exclusive file/directory list. Two workers editing
the same file will silently clobber each other (observed: duplicate
`model_health` field in one struct causing E0124 mid-wave).

- Write the ownership list in the prompt, verbatim.
- Before spawning, check `roster` for a still-`busy` worker on those paths.
- Never let a worker "help" outside its list; steer it with `session_write`
  instead.

## Rule 2 — Never let a worker finish without writing its handoff

A worker that hits its step limit mid-edit leaves a half-applied tree. Observed
damage: a deleted `const PROMPT_EXPLORE: &str =` declaration line, 2 of 10
agents missing from the return vector, JSX referencing deleted signals.

- Require an incremental `box_write`/append after **each** completed file, not
  one final summary.
- Prompt literally: "if you are near your step budget, stop editing and write
  the handoff first."
- Treat every "done" claim as unverified until the gate command output is pasted.

## Rule 3 — Make the worker paste command output, never a claim

Ask for `... 2>&1 | tail -N` verbatim. A report without a tail is a hypothesis.
Several workers reported success while their last check was skipped or previews
the pre-edit tree.

## Rule 4 — One long command per step, long timeouts

Long cargo/vite builds exceed default tool timeouts. Instruct workers to run one
build per step with a long timeout and let it finish, instead of re-running short
attempts (which re-link from scratch and burn the budget).

## Rule 5 — Disk hygiene is a first-class constraint

Parallel workers each picking their own `CARGO_TARGET_DIR` filled `/` to 100%,
producing `rustc-LLVM ERROR: IO failure on output stream` and `mold: Disk full?`.
These read as code errors but are not.

- `df -h /root` at the start of every wave.
- Reuse a small, fixed set of target dirs (`target/debug` shared, at most one
  scratch dir), and delete scratch dirs between waves.
- Never delete `target/debug` or `target/release`; only wave-scratch dirs, and
  only after confirming `pgrep -a cargo` is empty.
- Run `env -u OC_WEB_DIST` for oc-server tests (see project memory).

## Rule 6 — Separate "implement" from "verify" into different agents

The implementer is biased toward declaring victory. Give the final gate run to a
fresh agent whose only job is: run fmt/clippy/test, paste tails, fix only clear
breakage, list defects it found but did not fix.

## Rule 7 — Box keys and box hygiene

- Box keys allow only `[a-z0-9._-]`. A key like `contract/model-resolution`
  fails validation; use `contract-resolution`.
- Handoff boxes are append-only in practice; after 3+ appends they become 40-50k
  characters of stale interim text that misleads the next session. **Rewrite**
  (not append) a handoff box into a compact final record as soon as the wave is
  green.

## Rule 8 — Freeze contracts before spawning

Put the decision (order of resolution, wire field names, agent ids, UI layout)
into a stable box and point every worker at it. Workers that re-derive a design
from the code diverge from each other, and the reconciliation costs more than the
implementation.

## Definition of done for a wave

1. `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D
   warnings`, `cargo test --workspace` tails pasted and green.
2. UI source typechecked with a before/after error diff against the known
   pre-existing baseline; build output rebuilt and the new strings grepped out of
   the bundle.
3. Handoff box rewritten compact; every claim traceable to a pasted command.
4. Docs updated to describe shipped behavior, not the intended contract.
