---
name: Rust Build & Test
description: Build and test Rust without locking the workspace. Use when running cargo check, clippy, nextest, test, or when several agents share a target directory.
metadata:
  opencode/autoinvoke: true
---

# Rust Build & Test — big brother protocol

Cargo locks the target directory, not the command. Two agents running
`cargo test` in one workspace serialize silently, burn minutes, and stall their
turns. Never run cargo directly when more than one agent may be working.

## 1. Get a ticket

- If the `lane` tool exists: `lane.queue` with
  `{ "lane": "cargo:<workspace>", "command": "<exact command>", "cwd": "<dir>",
  "priority": "high|normal|low", "owner": "<your session id>" }`.
- If it does not: serialize with a lock directory next to the workspace:
  `mkdir .cargo-lane.lock` in a retry loop with 2 s sleep and a 20 min deadline;
  remove it in a `finally` block. Never delete a lock you do not own.
- One ticket per command. Do not hold a ticket across unrelated work.

## 2. Order of operations

Fastest failure first, most expensive last:

1. `cargo fmt --check` (seconds, no target lock contention if formatted).
2. `cargo check --workspace --all-targets` — add `-p <crate>` when possible.
3. `cargo clippy --workspace --all-targets -- -D warnings` (reuse check artifacts).
4. `cargo nextest run --workspace --no-fail-fast` if installed, else
   `cargo test --workspace --no-fail-fast`.
5. Only the failing test: `cargo nextest run -p <crate> <test-name>` or
   `cargo test -p <crate> <test-name> -- --exact`.

Skip steps that cannot be affected by the diff. State which steps you skipped
and why.

## 3. Shared workspace rules

- Prefer `cargo check`/`clippy` over `build` unless a binary is needed.
- Set `CARGO_BUILD_JOBS` once (default `min(cores, 8)`); do not let each agent
  set a different value.
- Parallel isolation uses a distinct target dir, not a second lock:
  `CARGO_TARGET_DIR=target/<agent-or-worktree>` for simultaneous work in
  worktrees. Copying a warm target dir is faster than a cold build.
- Never `cargo clean`, `rm -rf target`, or change `RUSTFLAGS` without an
  exclusive lane ticket; both invalidate everyone's cache.
- `cargo update` is a workspace-wide mutation: announce it, run it alone, and
  commit the lockfile change separately.

## 4. Reporting

The ticket result is what siblings need. Report exactly:

```
lane: cargo:... | ticket: ... | status: ok|fail | took: 42s
cmd: cargo nextest run -p oc-store
failures: <test ids, one per line> | none
first error: <file:line: message, ≤ 2 lines>
```

Send it to the coordinator (or the requesting sibling) with `talk`. Do not paste
full compiler output; attach the command and the first error.

## 5. When you are the coordinator

You are the big brother. You do not write code while holding the lane.

1. Collect requests as tickets; dedupe identical commands.
2. Order: format → check (affected crates) → clippy → tests. Run the smallest
   scope that can prove the change.
3. Batch independent test binaries into one `nextest` run with
   `--no-fail-fast`; do not start one lane ticket per test.
4. On failure, send the failing test ids and first error to the owning sibling,
   not the whole log.
5. Keep the lane busy but never run two cargo commands in one target dir. If a
   sibling must build in parallel, give it a separate `CARGO_TARGET_DIR`.

## 6. Hard stops

- A command that has produced no output for 10 minutes: cancel it, report
  `stalled`, and check whether another process owns the target lock.
- Do not retry a failing compiler error unchanged. Fix or ask.
- Do not run tests in a loop hoping for a pass; flaky tests get a dedicated
  ticket and are reported as flaky.
