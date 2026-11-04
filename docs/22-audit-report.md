# Audit report — escape-agent harness

Date: 2026-11-04. Scope: every crate in the workspace, plus wire compatibility
against `../opencode/packages/client/src/promise/generated/types.ts`.

HEAD at report time: `03a90ea`. Working tree clean.

## Method

Read-only audits per subsystem with `file:line` evidence, then one fix per
confirmed defect with a regression test that fails before and passes after.
Every claim below was verified against the code, not against a worker's
self-report.

## Confirmed defects and fixes

| # | Defect | Evidence | Fix |
|---|---|---|---|
| F1 | An `Err` in the actor drain loop killed the actor permanently; `ensure_actor` only notified the dead handle, so the session stayed busy forever (no `session.idle`, no terminal event). | `runtime/actor.rs:99,106,138` | `a2d59fe` reaps a finished actor and respawns. Test `66b7f98` (`tool_panic_does_not_wedge_the_session`). |
| F2 | Recovery could publish two terminal events for one turn and report a fake abort. | `runtime/turn/recovery` | `6a3ded3`, fails-before proven (`EXIT_BEFORE=101`, 152 tests). |
| A3 | A failed subagent child was reported as `completed`, because the durable `idle_outcome` was defaulted. | `spawn.rs`, `finish_foreground` | `27a6378`; test `subagent_reports_a_deleted_child_as_failed`. |
| A4 | Compaction lifecycle was not observable; `delta` was documented but unimplemented. | `turn/compaction.rs` | `28ace94` + `055da6a` (docs corrected, `delta` deliberately dropped). |
| S2 | `GET`/`DELETE /api/shell/{id}` served shells owned by another session. | `data/shells/http.rs:99,143` | `2637449`; 404 via `shell_not_found`, `EXIT_WITHOUT_GUARD=101`. |
| S3 | `session.viewed` was written non-atomically with its event (two writers, one monotonic seq). | `oc-store/src/session.rs:352-369` | `99ada67` atomic write; `8889f94` removed the dead duplicate. |
| S4 | The spawn budget silently uncapped itself when the active child count could not be read. | `oc-store/src/session.rs` | `726f791` now fails loudly. |
| C2 | An agent whose frontmatter could not be parsed silently inherited `Allow`. | `oc-config/src/markdown_agents.rs:27,116,131`; `agents.rs:355` | `9b9505e` fails closed; test `289ac0f` (`left: Allow` / `right: Deny`, `EXIT_BEFORE=101`, 36 tests). |
| T1 | The edit tool read an arbitrarily large file into memory. | `oc-tools/src/edit.rs:20,85` | `b6e0e43`; test `edit_size_cap.rs`, `EXIT_WITHOUT_CAP=101`. |
| M1 | A rewrite of a soft-deleted memory silently undeleted it, and `touch_memories` lacked a `deleted` filter. | `oc-store/src/memory.rs:155-175,305-320` | `31620d9`; test `scribe_write_cannot_resurrect_a_forgotten_memory` (`tests/memory.rs:290`), `EXIT_BEFORE=101`. |
| G1 | A session that discovered zero skills fell back to the server's load-time snapshot, leaking the server project's skills into the prompt index, and disagreeing with the loader. | `oc-runtime/src/prompt/sources.rs:131-134` | `03a90ea`; removes the fallback so the index and the loader agree. |

## Findings judged not to be bugs

| Finding | Verdict | Why |
|---|---|---|
| `session.viewed` published after a terminal event | by design | The event records a read, not a turn transition. |
| Gaps in `seq` | by design | One per-session counter is shared by `event`, `session_message` and `session_inbox`. |
| SSE `Lagged` ends the stream with `None` | by design | Pinned by `oc-server/tests/http.rs:294-327` (`sse_overflow_ends_the_stream`). |
| "Zero user messages" in a session | not reproducible | Artifact of the newest-50 page; `GET /message` with no limit returns the newest 50, `limit>200` clamps to 200. |
| Exit status masked for lane / `oc_tools` shells | refuted | Both run under `sh -o pipefail -c`. |
| Foreground subagent cancelled by a restart | design gap, not fixed | `spawn.rs:205` persists the child durably in both modes; only who waits differs (`wait.rs:414-421` vs `:172`). |

## Known open items (not fixed)

| Item | Location | Severity |
|---|---|---|
| A truncated stream is reported `succeeded`. | `turn/stream.rs:157-159,397` | high |
| A stored compaction reason `"overflow"` is not a valid `CompactionReason`. | `turn/compaction.rs:120,311,439` | medium |
| The `wait` budget never expires; `wait.rs:31` clamps a NaN. | `oc-server/src/data/session/wait.rs:31` | medium |
| A deterministic summarizer failure wedges every following turn. | `turn/compaction.rs:192-239` | high |
| Path containment lowercases on Linux, so a case-differing sibling escapes. | `oc-tools` containment helper | high |
| The `patch` tool has no input size cap (the edit tool does). | `oc-tools/src/patch.rs:287-301` | medium |
| `diff.rs` builds a quadratic matrix with no cell budget. | `oc-tools/src/diff.rs:21-32` | low |
| LSP and plugin sidecars kill only the direct child. | `oc-lsp/src/client/spawn.rs:43-52`; `oc-plugin/src/sidecar/process.rs:148-160` | medium (documented limitation) |
| A dead SSE GET is never respawned while MCP status stays `Connected`. | `oc-mcp/src/transport/http.rs:604-616` | medium |
| LSP diagnostics send empty text as valid 1:1. | `oc-lsp/src/diagnostics.rs:38` | low |
| `config_patch` removes then renames, leaving a window with no file. | `oc-server/src/data/config_patch.rs:151-156` | low |
| Skill id collisions resolve global-over-project, opposite of the documented order. | `oc-config/src/skills.rs:126-135` | medium |
| `.claude`/`.agents` skill roots are scanned up every ancestor to `/` with no containment check. | `oc-config/src/skills.rs:137-146` | low (documented) |
| The `03a90ea` skill-leak fix has no regression test. The test-writing attempts died at harness restarts, so only the source fix landed; nothing yet pins that a session with no skills of its own does not inherit the server's snapshot. | `oc-runtime/src/prompt/sources.rs:131` | test gap |
| In this frontmatter subset a list-valued `permissions:` can never parse; such agents are now refused rather than silently permitted. | `oc-config/src/markdown.rs:190-192` | by design after `9b9505e` |

## Agent grouping

`docs/21-grouping.md` specifies grouping as an optional non-empty string
`metadata.group` on the existing open `SessionMetadata`
(`types.ts:65,2238`). The wire is not extended: `session.list` keeps exactly
nine parameters (`openapi.json:921-1068`), so no query parameter was added.
The convention is documentation-only; no crate filters on it yet. The
limitation is recorded in the doc: `PATCH` replaces metadata wholesale
(`data/session/crud.rs:176-186`), so a client must send the full metadata
object.

## Gates

`cargo fmt --all --check`: pass (`EXIT_FMT_CHECK=0`, twice).
`cargo clippy` per crate with `-D warnings`: pass for oc-types, oc-store,
oc-config, oc-tools, oc-runtime and oc-server (all `EXIT_CLIPPY_*=0`).
`cargo test` per crate: pass for oc-types, oc-store, oc-config, oc-tools, and
all five oc-runtime test binaries (`steer_live` 11, `nested_spawn` 5,
`observer_repairs` 7, `manual_compact` 7, `runtime` 74 — all `EXIT_*=0`).
oc-server's test binaries could not be executed in this environment: linking
the 300 MB test binary exceeds the roughly ten-minute restart window, and every
attempt was killed mid-link. clippy for oc-server passed, including
`--all-targets`, which type-checks those same test targets. Re-run
`cargo test -p oc-server -- --test-threads=1` on a machine without that
restart cadence before release.

## Environment note

The session harness restarts every ~10 minutes and kills child processes,
including `setsid` process groups. That is a defect in the harness, not in this
repository: a `spawn`ed child that is persisted durably can still be cancelled
while its parent waits for it.

## Disposition of the two re-dispatched audit areas

Two delegation attempts failed at a harness restart before reporting. Both areas
were re-dispatched and are covered; this section records the disposition so the
gap is not mistaken for an unexamined area.

| Failed session | Area it owned | Replacement session | Outcome |
|---|---|---|---|
| `ses_e5da0cf18ada47Y5Nlbrts0JAc` (explore) | Map the supplement-memory scope defect | `ses_e59566dc687eSlBDvc6W2CqUla` | Covered. Verdict **by design, already correct and pinned**; no code change. |
| `ses_e5da3cfbbb84ZFnjDUW054ScRv` (general) | Finish or cleanly revert the skill-scope fix | `ses_e59566dc687fwRJN2R8HV4VC3B` | Covered. Found two real defects; one fixed in `03a90ea`, the other is in the open-items table above. |

**Supplement memory.** The scope list is built per session from `project_id` and
`agent_id`, and the `session` scope is added only for subagents, keyed to the
current session id (`oc-runtime/src/prompt/sources.rs:188-195`). Row filtering is
`(m.scope = ? AND m.scope_id = ?)` (`oc-store/src/memory.rs:440`), the
comparison baseline is per actor (`oc-runtime/src/runtime/actor.rs:24`), and
deleted rows are excluded by SQL (`oc-store/src/memory.rs:273,388,418`).
Independently of the SQL, a stale pin is dropped and the supplement is suppressed
(`oc-runtime/src/turn/session.rs:218-224`, `instructions.rs:186`). Pinned by
`forgetting_a_pinned_memory_drops_it_from_the_epoch_prefix`
(`crates/oc-runtime/tests/runtime/memory.rs:198-201`) and
`memory_from_another_project_is_not_injected` (`:135-136`). The residual gaps are
test coverage, not defects: there is no cross-*session* case, and no positive
case that a newly added memory shows up in the supplement.

**Skill scope.** The `.opencode` sibling case is pinned by
`find_skill_scopes_to_the_session_directory`
(`crates/oc-runtime/src/skill/tests.rs:118`, assertion "a project skill outside
the session tree must not be loadable"). The re-dispatched audit found two real
defects beyond that: the server-snapshot fallback, fixed in `03a90ea`, and the
id-collision ordering, which is in the open-items table above. Nothing pinned the
`.claude`/`.agents` ancestor loop, `skills[]` visibility, or the fallback, so
`03a90ea` landed without a regression test — recorded in the table above.
