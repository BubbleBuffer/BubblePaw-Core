# LCM episode prompt v3 comparison

Newest round first. Round 3 measures the final v3.1 text (PR #24 head `4cc156d`) against v2. Rounds 2 and 1 follow unchanged.

## Round 3: v2 and final v3.1 (2026-10-28)

Same 9 closed episodes as round 2, same bench and Flash model, arms `v2,v3` only, cap $0.50. 19 calls, $0.209. All live runs together: $0.736. The v3 text now also limits change counts to the +N/−M form shown in the source, and asks for full message UUIDs and call IDs in prose too. Metrics follow the current parser, including the tolerant `parse_gist`. "Gist words" is the length before the 25-word cap.

| Ep | Chat | Chars v2 | Chars v3.1 | Rows parsed | Mutating covered | Gist words | Gist is the episode | Invented counts | Short UUIDs | v2 IDs kept | v2 paths kept |
|---|---|---|---|---|---|---|---|---|---|---|---|
| 1 | 791b9ca4 | 7908 | 8518 | 3/3 | 0/0 | 13 | yes | 0 | 0 | 4/4 | 2/4 |
| 2 | 791b9ca4 | 7120 | 7775 | 8/8 | 0/0 | 34 | yes | 0 | 0 | 7/7 | 4/5 |
| 3 | 791b9ca4 | 7817 | 7824 | 7/7 | 0/0 | 30 | yes | 0 | 0 | 7/8 | 3/28 |
| 4 | 94af8a96 | 7229 | 7585 | 5/7 | 0/0 | 16 | yes | 0 | 0 | 8/8 | 3/3 |
| 5 | 94af8a96 | 9167 | 7155 | 9/9 | 0/0 | 23 | yes | 0 | 0 | 8/8 | 6/8 |
| 6 | 94af8a96 | 8619 | 8500 | 11/11 | 7/7 | 22 | yes | 0 | 0 | 11/11 | 6/8 |
| 7 | 7a533468 | 6951 | 6965 | 5/5 | 0/0 | 14 | yes | 0 | 0 | 3/3 | 5/7 |
| 8 | 7a533468 | 8452 | 9869 | 10/10 | 0/0 | 33 | yes | 0 | 0 | 7/10 | 7/8 |
| 9 | 7a533468 | 6299 | 8106 | 13/13 | 1/1 | 24 | yes | 0 | 0 | 13/14 | 7/7 |
| **All** | | 69562 | 72297 (+4%) | 71/73 | 8/8 | 5/9 within 25 | 9/9 | 0 | 0 | 68/73 | 43/78 |

The spot check read all nine gists. Each one says what that episode found or changed, for example episode 9's "Sub-items 1–3a are implemented and compile; 3b (`worktrees.rs`) was left untouched ...". None restates the task the way round 2's episodes 8 and 9 did. No gist sits directly on line 2: all nine leave a blank line after the title, and seven still use backticks. `parse_gist` handles both.

Both malformed rows in episode 4 use the verb `Listed`, which is neither in the verb set nor a mapped synonym:

````text
- Listed crates/bubblepaw-core/tests and searched for ScriptedProvider [call_00_vm70ebsunxyaurqzqs43phst]
- Listed crates/bubblepaw-core/tests/support and read crates/bubblepaw-core/tests/structured_output.rs [call_00_86pwupwlhxxbvqam4z5o2wq0]
````

Path retention is low only in episode 3. There v2 lists 28 test and fixture paths, while v3.1 summarizes them in a few rows and in prose; in the other episodes v3.1 keeps most of v2's paths. Chapter prompt v1 closed the three v3.1 episodes of `791b9ca4` with 3,769 output tokens and 12,428 characters. It stayed prose, with 0 Action-form rows.

| Target | Round 3 | Met |
|---|---|---|
| Parse 100% | 71 of 73 (97%) | no: 2 `Listed` rows |
| Coverage of all edits | 8 of 8 | yes |
| Characters at most +15% over v2 | +4% | yes |
| Gist found and within 25 words | found 9/9; 5/9 within 25 words before the cap; 9/9 summarize the episode | partly: found and on topic, too long in 4 |
| No invented counts | 0 | yes |
| No shortened UUIDs | 0 | yes |

**Verdict: v2 stays the default.** The final v3.1 removes round 2's invented count, shortened UUIDs and task-restating gists, and stays within +4% of v2's length. Two gaps remain:
- the gist is too long in 4 of 9 episodes and never sits directly on line 2;
- one episode used an unlisted verb.

Both are cheap to absorb on the host side without another paid run: `parse_gist` already caps the gist at 25 words, and `Listed` now maps to `Read` (added after this run, so both rows would now parse: 73 of 73). One run of 9 episodes still carries the run-to-run noise noted in round 2.


## Round 2: v2, v3-draft and v3.1 (2026-10-28)

v3.1 revises `summary_prompt_v3.txt` after round 1:
- The gist sits directly on line 2: no blank line, no backticks or Markdown, at most 25 words.
- Targets are repo-relative paths, and IDs are copied in full.
- Each row has one outcome; when it needs several, they are separated by ` · `.
- Only the listed verbs are used (a rewrite is `Edited`).
- Prose no longer restates what the Actions rows say.

`episode_prompt = "v3"` now selects v3.1. The round-1 text is kept as `summary_prompt_v3_draft.txt`, which only the bench can select (`v3-draft`, behind the `bench` feature). The parser now:
- keeps several ` · ` outcomes joined;
- maps Rewrote, Modified and Updated to Edited, Wrote to Created and Executed to Ran;
- skips other unknown verbs.

`parse_gist` takes the first non-blank prose line after the title, wherever it sits. It skips headings, list items, quotes, tables and fences, and stops at the `Actions:` or `Open:` section. It strips backticks and keeps at most 25 words.

Bench: `--arms v2,v3-draft,v3 --per-session 3 --no-tail` on three recorded coding chats: `791b9ca4` (SQLite migrations), `94af8a96` and `7a533468` (host attach work). The run took the first three closed episodes of each chat, 9 in all; unclosed tails were left out. Each arm chains its own previous summaries. Flash only, 30 calls, $0.330. Both live runs together cost $0.527.

| Ep | Chat | Source tok | Chars v2 | Chars draft | Chars v3.1 | Parsed draft | Parsed v3.1 | Covered draft | Covered v3.1 | Invented v3.1 | Gist draft | Gist v3.1 | Bare targets draft/v3.1 | Short UUIDs draft/v3.1 |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 1 | 791b9ca4 | 24206 | 7899 | 8932 | 5941 | 4/4 | 4/4 | 0/0 | 0/0 | 0 | 19 | 18 | 0/0 | 0/0 |
| 2 | 791b9ca4 | 26818 | 7340 | 10691 | 8112 | 7/7 | 11/11 | 0/0 | 0/0 | 0 | 22 | 22 line 2 | 0/0 | 0/0 |
| 3 | 791b9ca4 | 26222 | 6469 | 9786 | 7111 | 7/7 | 7/7 | 0/0 | 0/0 | 0 | 23 | 18 | 0/0 | 0/0 |
| 4 | 94af8a96 | 25151 | 7560 | 6861 | 6947 | 5/7 | 2/2 | 0/0 | 0/0 | 0 | 20 | 15 line 2 | 0/0 | 0/4 |
| 5 | 94af8a96 | 24223 | 8687 | 8245 | 9184 | 9/9 | 4/4 | 0/0 | 0/0 | 0 | 28 | 16 | 0/0 | 0/0 |
| 6 | 94af8a96 | 24775 | 9220 | 8514 | 8204 | 11/12 | 14/14 | 7/7 | 7/7 | 0 | 23 | 34 | 4/1 | 0/0 |
| 7 | 7a533468 | 26006 | 7610 | 7744 | 6937 | 4/4 | 4/4 | 0/0 | 0/0 | 0 | 16 | 20 | 0/0 | 0/0 |
| 8 | 7a533468 | 24100 | 9402 | 9038 | 8072 | 5/5 | 6/6 | 0/0 | 0/0 | 0 | 16 | - | 3/0 | 0/0 |
| 9 | 7a533468 | 24229 | 8018 | 10840 | 8339 | 13/13 | 12/12 | 1/1 | 1/1 | 1 | 26 | - | 3/0 | 0/0 |
| **All** | | | 72205 | 80651 (+12%) | 68847 (-5%) | 51/54 | 64/64 | 8/8 | 8/8 | 1 | | | 10/1 | 0/4 |

The gist columns give the word count of the first non-empty line after the title; "line 2" means it sits directly under the title. A dash means no gist line, because the summary went straight into a heading.

Verdict against the v3.1 targets:

| Target | v3.1 | Met |
|---|---|---|
| Parse 100% | 64 of 64 rows | yes |
| Coverage of all edits | 8 of 8 mutating calls | yes |
| Characters at most +15% over v2 | -5% | yes |
| Gist on every episode (9/9) | Directly on line 2: 2 of 9. With the tolerant `parse_gist`, recomputed offline: 9 of 9 found, 7 of 9 within 25 words before the cap | partly: the rule is not, the parse is |

Other findings:
- **Invented count.** One row was invented: episode 9's `Edited docs/host-attach.md · +4 −2`. The source showed only a `4 +-` diff stat.
- **Shortened UUIDs.** Episode 4 shortened 4 UUIDs, despite the full-ID rule.
- **Retention against v2.** Call IDs kept: draft 82 of 86, v3.1 80 of 86. Full paths kept: draft 38 of 63, v3.1 39 of 63.
- **Run-to-run noise is large.** The same draft prompt was +45% in characters over v2 in round 1 and +12% here. One run of 9 episodes cannot settle a few percent either way.
- **Chapter.** Chapter prompt v1 closed the three v3.1 episodes of `791b9ca4` with 3,482 output tokens and 11,666 characters. It stayed prose, with 0 Action-form rows.

**v2 stays the default.** v3.1 fixes the parse, verb and length problems, and the parser now tolerates the common deviations. It still misses the gist-placement target: most summaries leave a blank line after the title, and two put a heading there instead. For that reason v3 is not proposed as the default. The tolerant `parse_gist` was rerun on the saved outputs, with no new model calls:
- v3.1 yields a gist in 9 of 9 episodes.
- In episodes 8 and 9 the gist is the first prose line under "Goal and constraints". It is a task statement, not an episode gist; episode 9's runs to 121 words before the cap.
- The round-1 draft yields 8 of 8, but only 3 of those are within 25 words.

A follow-up prompt edit, not measured, does two things:
- It allows a change count only when the source shows it as +N/−M, so a bare diff stat such as "4 +-" gives none.
- It requires message UUIDs and call IDs in full, in prose too.

### Episode 9, v3.1 text

````markdown
# Runner async-offload fixes: all three sub-items edited, checks green except one test-exit quirk

## Goal and constraints

Human task (source 6c42ae3d-e445-4d2e-b873-86ec5ab35b89, delegated Typed request): three runner fixes, each to be verified in code first and skipped if the finding no longer holds — (1) one pre-submission registered Ctrl-C listener in `host_attach::attach` plus a deterministic pre-SIGINT wait in its test and one sentence in `docs/host-attach.md`; (2) `net/cache.rs` SQLite `lookup`/`insert` moved into `tokio::task::spawn_blocking` via async wrappers called from `net.rs`; (3) sync git off async workers — (a) `delegate/diff.rs` + `repo_name` async, (b) `worktrees.rs` reached from async `agents.rs:1231`, where a cross-file `start_worker` API change must STOP and be reported as call chain + proposal instead. Scope limited to `host_attach.rs`, `net.rs`, `net/*`, `delegate.rs`, `delegate/diff.rs`, `worktrees.rs`, `agents.rs` (sub-item 3 only, not `complete_effect`), `child_workspaces.rs`, runner tests, `docs/host-attach.md`. Final reply must be the delegation_report JSON.

## Progress and current state

- Sub-item 1 (prior episode, now tidied): `host_attach.rs` has module-level `type Interrupt` (`tokio::signal::unix::Signal` / `tokio::signal::windows::CtrlC`), `interrupt_listener()`, one listener created before the `attach.submit` write, and select arm `_ = interrupt.recv(), if !cancelled`. This episode collapsed the stray double blank line before the helper's doc comment, so the fmt risk is gone (`cargo fmt --all -- --check` exit 0).
- Test/comment: `tests/host_attach.rs` now carries a three-line comment above `interrupted.until("the hang step", …)` stating that the listener is registered before `attach.submit`, so the host producing the step proves the listener exists; readiness condition unchanged, no sleeps.
- `docs/host-attach.md` §Control protocol item 4 now states the client arms its interrupt listener before writing `attach.submit`, so an early or between-poll Ctrl-C is buffered. Diff stat: 4 changed lines.
- Sub-item 2 implemented: `net/cache.rs` gained `lookup_async` and `insert_async` (`spawn_blocking` over a cloned `SqliteCache`/`Arc<Inner>`, `.context("web cache lookup/insert task stopped")`), placed before `fn reclaim`; the original sync `lookup`/`insert` remain so the module's sync `#[test]`s still compile. `net.rs` private `lookup`/`insert` helpers are now `async fn` (insert takes `T: Serialize + Send + 'static` by value) delegating to the wrappers, and all six call sites in `search_chain`/`fetch_chain` `.await` them (insert sites pass `served.clone()` / `page.clone()`).
- Sub-item 3a implemented: `delegate/diff.rs` rewritten (3854 bytes) around `tokio::process::Command`; `git`, `snapshot`, `between` are async, unit test is `#[tokio::test]` with awaited calls. `delegate.rs` `repo_name` is `async` and uses `tokio::process::Command`; `repo_name(&workspace).await`, `diff::snapshot(&workspace).await` (both sites) and `diff::between(...).await` are awaited inside `run_controlled`.
- Sub-item 3b: `worktrees.rs` and `agents.rs` untouched — still to be reported as STOP with the chain recorded in the prior episode (sync `start_worker` → `start_profile_owned` → `create_in_admission` at agents.rs:1231 → `worktrees.rs` `run`/`git`/`git_test`) and the proposal to wrap the `create_in_admission` call in `spawn_blocking` inside that sync closure.
- Acceptance greps are satisfied: no `tokio::signal::ctrl_c(), if` arm remains, no `std::process::Command` in `delegate/diff.rs`, `spawn_blocking` present in both `net.rs` and `net/cache.rs`.

## Evidence and verification

- `cargo fmt --all` exit 0 (call_241d9163c28f4789aabc16c5); `cargo fmt --all -- --check` exit 0 (call_430ef45b2f0e4cf198c913a4).
- `cargo check -p bubblepaw-runner --all-targets --features web` exit 0 in 2m42s (call_992d6d77dbd54b498a9f1ccd).
- `cargo clippy --workspace --all-targets -- -D warnings -A deprecated` exit 0 (call_430ef45b2f0e4cf198c913a4).
- `cargo clippy --target x86_64-pc-windows-gnu -p bubblepaw-runner --lib --bins -- -D warnings -A deprecated`: first attempt exit 1, E0463 "can't find crate for `core`" — target not installed (call_37f1ac1298fc44a488fbbeff); after `rustup target add x86_64-pc-windows-gnu` exit 0 (call_19ff3b4475d44011b966daf9, call_4a8b245090764ee38163ae2e).
- Nine test targets: 59 passed, 0 failed — child_sessions 21, delegate 8, net 4, process_workspaces 2, research 8, web_fallback 10, worktree_children 6; `host_attach` and `host_attach_mcp` ran 0 tests because `tests/host_attach.rs` starts with `#![cfg(unix)]` and this host is Windows (call_4143b32c6b064dcfa32c37e8, call_8ed5f7bc8e9d40cda47e2c67, call_b1aa2028d3f14496b73bde56). The wrapping PowerShell command reported exit 1 despite "test result: ok" lines, from PowerShell turning cargo's stderr incremental-dir notes ("Zugriff verweigert (os error 5)") into NativeCommandError (call_44f7f8973e9c4dc89b6ed78b) — the direct acceptance invocation does not use `Out-File`/`2>&1`, so this needs one clean re-run to confirm exit 0.
- `cargo test -p bubblepaw-runner --lib` was started in background as `process:2c6edce2-68d6-46fd-8614-731f579e7f31` writing `lib-run.txt`; its outcome was never observed by the end of this episode (call_78ef6411fedc47f1b6f0d264).
- Environment note: builds lack incremental-session finalization (`os error 5`) on this volume but still finish; harmless.

## Actions

- Edited crates/bubblepaw-runner/src/host_attach.rs · collapsed the duplicate blank line before the Ctrl-C helper [call_4dcb805e8dd84a4eaa2bf956]
- Edited crates/bubblepaw-runner/src/net/cache.rs · +29 · added async `lookup_async`/`insert_async` over `spawn_blocking`, sync methods kept [call_4dcb805e8dd84a4eaa2bf956] [call_ee941d6f2ce84d2f823b67d6]
- Edited crates/bubblepaw-runner/src/net.rs · 8 replacements · `lookup`/`insert` helpers made async and all six call sites awaited [call_ed66b2c37f5c4c8389006036]
- Created crates/bubblepaw-runner/src/delegate/diff.rs · rewritten via `tokio::process::Command`, async `snapshot`/`between`, `#[tokio::test]` [call_7b1723a69a6d4d83bff47791]
- Edited crates/bubblepaw-runner/src/delegate.rs · 5 replacements · `repo_name` async with tokio process, awaited at ~245/248/366/368 [call_bb09c62736dd40df87c9bcb0]
- Edited crates/bubblepaw-runner/tests/host_attach.rs · +3 · readiness comment for SIGINT before the hang-step wait [call_241d9163c28f4789aabc16c5]
- Edited docs/host-attach.md · +4 −2 · sentence on arming Ctrl-C before `attach.submit` [call_217ef5f27706460a9055481d]
- Ran `cargo fmt --all` then `cargo fmt --all -- --check` · exit 0 [call_241d9163c28f4789aabc16c5] [call_430ef45b2f0e4cf198c913a4]
- Ran `cargo check -p bubblepaw-runner --all-targets --features web` · exit 0 [call_992d6d77dbd54b498a9f1ccd]
- Ran `cargo clippy --workspace --all-targets -- -D warnings -A deprecated` · exit 0 [call_430ef45b2f0e4cf198c913a4]
- Ran windows-gnu clippy · exit 1 (target missing) · installed target · rerun exit 0 [call_37f1ac1298fc44a488fbbeff] [call_19ff3b4475d44011b966daf9] [call_4a8b245090764ee38163ae2e]
- Ran nine test targets · 59 passed, 0 failed, host_attach/host_attach_mcp 0 tests under `#![cfg(unix)]`; wrapper reported exit 1 from PowerShell stderr noise [call_8ed5f7bc8e9d40cda47e2c67] [call_b1aa2028d3f14496b73bde56] ! risk: acceptance test step needs a clean exit 0

## Open

- Re-run the nine target tests (and `--lib`) directly without `Out-File`/`2>&1` piping to confirm exit code 0; `process:2c6edce2-68d6-46fd-8614-731f579e7f31` lib run result is still unknown.
- Produce the sub-item 3b STOP text (call chain `probes`/`parallel.rs:290` → sync `start_worker` agents.rs:762 → `start_profile_owned` 1080 → `create_in_admission` agents.rs:1231 → `worktrees.rs` `run`/`git`/`git_test` ~948-990; proposal: `spawn_blocking` around the `create_in_admission` call in that sync closure, or making `start_worker`/`start_profile_owned` async and updating out-of-scope `probes.rs`/`parallel.rs` callers).
- Emit the final delegation_report JSON: sub-item 1 done (with comment + doc sentence), 2 done, 3a done, 3b stopped; checks = fmt 0, workspace clippy 0, windows-gnu clippy 0 after target install, web-feature check 0, nine test targets 59 passed (host_attach tests not run on Windows due to `#![cfg(unix)]`), cargo fmt --check 0.
````

## Round 1: v2 against the v3 draft (2026-10-28)

Measured 2026-10-28 with `bubblepaw-episode-prompt-bench` (bench feature, dev-only) on `opencode/deepseek-v4.1-flash`. The bench copies a store and plans episodes the way the live worker does (default `EpisodeConfig`, 24k-token source target). It summarizes the same episode-only input once with `episode_prompt = "v2"` and once with `"v3"`; each arm chains its own previous summary. It then closes one chapter with chapter prompt v1 over the v3 summaries. Spend for all 17 calls: $0.197.

Source: the recorded BubblePaw-base coding chat `791b9ca4`, which converted five per-open SQLite migrations to numbered `user_version` steps. Its episodes 1-8 are used in order; episode 8 is the chat's unclosed tail. The other five candidate chats were never reached because this one alone yielded eight episodes. Chats with credential-like text were left out before the run. The bench also skips any episode that matches its credential pattern; none did.

### Metrics

- **Output tokens** are the provider's billed output and include reasoning, so they are noisy. Characters measure the summary text itself.
- **Parse rate** comes from `episode_actions::parse_actions`, which the analysis script mirrors.
- **Coverage** counts every top-level `js` call whose code runs `tools.write` or `tools.edit` (or `rm`/`mv`). A call counts as covered when an Actions row cites its call ID or names its path.
- **Retention** compares the call IDs and full `a/b/c.ext` paths in v2 with those in v3.

| Ep | Source tok | Out tok v2 | Out tok v3 | Chars v2 | Chars v3 | Rows | Parsed | Mutating calls | Covered | Invented counts | Gist words | v2 IDs kept | v2 paths kept |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 1 | 24206 | 8035 | 3668 | 6564 | 8828 | 4 | 4 | 0 | 0 | 0 | 20 | 4/5 | 4/5 |
| 2 | 26818 | 3392 | 3198 | 6833 | 10849 | 7 | 7 | 0 | 0 | 0 | 27 | 6/7 | 3/5 |
| 3 | 26222 | 3630 | 3371 | 8186 | 8830 | 7 | 7 | 0 | 0 | 0 | 22 | 7/8 | 8/12 |
| 4 | 24868 | 3331 | 3391 | 6172 | 8366 | 7 | 7 | 0 | 0 | 0 | 18 | 8/10 | 13/16 |
| 5 | 25065 | 3865 | 3866 | 7515 | 12572 | 12 | 12 | 10 | 10 | 0 | 28 | 12/13 | 5/6 |
| 6 | 24393 | 1672 | 3915 | 5539 | 9739 | 10 | 9 | 4 | 2 | 0 | 35 | 9/10 | 5/6 |
| 7 | 24348 | 3148 | 3820 | 6548 | 10780 | 12 | 11 | 9 | 9 | 0 | 38 | 11/12 | 4/5 |
| 8 | 15481 | 3213 | 8088 | 6551 | 8366 | 7 | 7 | 1 | 1 | 0 | 37 | 7/8 | 6/11 |
| **All** | 191401 | 30286 | 33317 | 53908 | 78330 | 66 | 64 | 24 | 22 | 0 | | 64/73 | 48/66 |

What the numbers mean:

- **Parse rate: 64 of 66 rows (97%).** That is below the 100% target. The two malformed rows were skipped and the parse still succeeded:

````text
episode 6: - Edited 5 files (`request_usage.rs`×2, `storage_cold.rs`, `store_storage.rs`, `src/tests.rs`) · user_version 9 → 11 · all `replacements: 1` [call_00_ad7z5pn2o8m2nhz256sbnore]
episode 7: - Rewrote the whole core test file · helpers `version`/`count`/`text`/`column`, 219 lines [call_00_b7ewqruf0ephl8lwdu4rwyfn]
````

  The first row has two outcome parts. The second starts with "Rewrote", which is not in the verb set.

- **Coverage: 22 of 24 mutating calls.** There are two misses:
  - Episode 6, edit `…56sbnore`: it has its own row, but that row is the malformed one above, so a strict parse loses it.
  - A throwaway validation script written to a temp path outside the repo.
- **Invented change counts: 0.** No row used `+N −M`. The edits went through `tools.edit` with old and new strings, so the source never showed line counts, and Flash correctly left the change field empty.
- **Call IDs:** 56 of 64 parsed rows cite a source call ID. Every v2 call ID also appears in v3. The one ID that looks dropped in each episode is the task anchor's UUID, which v3 shortens to its 8-character prefix (`254e72ca`).
- **Paths:** v3 keeps 65 of the 66 v2 full paths, at least by file name. Its Actions targets often use a bare file name (`store.rs`) where v2 wrote the repo path, which lowers the full-path ratio. One full path (`context_episodes/legacy_metadata.rs`) is missing from episode 5 altogether.
- **Errors:** in seven episodes, v3 mentions errors and exit codes at least as often as v2. In episode 8, v3 drops v2's note that the earlier truncated counts were invalid, but keeps the corrected count call with exit 0.
- **Length:** v3 is 45% longer in characters (average 9791 vs 6738) against the same 2,000-token target. All 16 summaries passed the unchanged validation. Billed output tokens rose 10%.
- **Title and gist:** all 8 v3 summaries start with `# <title>`, and all put `Actions:` immediately before `Open:`. The gist rule is only partly followed:
  - 0 of 8 put the gist on line 2; every one leaves a blank line after the title.
  - 3 of 8 stay within 25 words.
  - 3 of 8 are free of Markdown; the rest use backticks.

### Chapter over v3 summaries

Chapter prompt v1 closed all 8 v3 episodes of `791b9ca4` as one chapter, using 5431 billed output tokens and 12151 characters. The chapter stayed prose. It has 0 lines in Action-row form, no `Actions:` section and no ` · ` separators. Rather than copying the action lists, it restates the goal, the settled design and the outcome. Its description:

> Chat/Serving/Catalog/Ledger migration style conversion: numbered `user_version` steps implemented, tests+fixtures written, `_migcheck.py` still needs deletion

The chapter text opens:

````text
## Goal and constraints

Typed request (source `254e72ca`): make every schema change in the SQLite stores a numbered `PRAGMA user_version` step, eliminating the five "add the column if missing on every open" sites, while every store the release at base `e67bd80` can produce still opens with data intact:

1. `store_quota.rs::migrate_catalog` balance ALTER → catalog v1→v2 (was called from `store_layout.rs::projection` and `store_quota.rs::record`)
2. `store_prompt_log.rs::setup`'s 7 `ADDED_COLUMNS` on serving projection → serving v2
3. `store_serving.rs::initialize_tool_call_index` kind rebuild → chat chain
4. `store.rs::ensure_checkpoint_schema` preimage_hash rebuild → chat chain
5. runner `delegate/ledger.rs::migrate`'s 6 `ADDED_COLUMNS` → new ledger chain (first step brings old ledgers current)

Settled design from the request: each DB keeps its own chain; a converted step may inspect the column/constraint **once** (old code may already have applied it), and bumps the version in the **same transaction**; later opens do no schema inspection; no extra transaction on an already-current open; fresh stores still get the full current schema; the serving `supported` version in `projection` rises accordingly.

Tests: fixture-based `old_style_*` tests (core `crates/bubblepaw-core/tests/fixtures/`, runner `crates/bubblepaw-runner/tests/fixtures/`, SQL only, no `.db`), each covering both "already applied at old version" and "column absent", asserting new version, new column/constraint, surviving row; existing tests stay green.

House rules: **never run cargo**, no `git stash`, no commit/push (harness commits and runs CI: fmt, strict clippy incl. `--features web,bench`, all core/mcp/runner tests). `file-size-allowlist.txt` is an exact line-count ratchet. Clippy `-D warning
…
````

### Round 1 verdict

v3 does what it was added for:
- The Actions index almost always parses.
- It cites source call IDs and covers the repo edits.
- It never invents change counts.
- The chapter prompt does not copy it.

Two weaknesses keep it opt-in:

- It costs about 45% more summary text.
- Flash ignores the gist format: a blank line follows the title, five of eight gists exceed 25 words, and five use backticks. Some Actions targets also shrink to bare file names, and 2 of 66 rows break the grammar.

Keep `v2` as the default. Before v3 is a default candidate, tighten the prompt: the gist goes directly under the title with no backticks, and targets use repo-relative paths. Then measure again on more than one chat.

### Side by side

Episodes 1 (exploration only) and 7 (many edits) are shown as full v2 and v3 texts. The other episodes show only the v3 gist and Actions section. All full texts are in the bench JSON, which is not committed.

#### Episode 1

Source `791b9ca4` messages `254e72ca`..`7d0851b2`: 24206 source tokens, 4 tool calls, 0 mutating.

**v2**

````text
# Numbered SQLite migrations for five stores

## Goal and constraints
Typed request `254e72ca-b1b6-43a9-8907-ebb84f286eab` requires replacing five unversioned per-open schema mutations with numbered `PRAGMA user_version` steps, while preserving every database producible by release `e67bd80`. Scope: core `store*.rs` as needed, `bubblepaw-runner/src/delegate/ledger.rs`, tests/fixtures, `file-size-allowlist.txt`, and the migration paragraphs of `docs/storage-model.md` / `docs/delegation-bridge.md`.

House rules: never run cargo, never use `git stash`, do not commit/push. Do not touch `Store::scoped` in `store_layout.rs` ~770-900. Add `old_style_*` SQL-fixture tests under core/runner `tests/fixtures/` (no binary `.db`), covering both “old code already applied the change” and “change absent.” Keep existing tests passing. Final acceptance runs a guard restricted to allowed paths and `ci-check.ps1 -Branch s5/check-migrations -MinPassed 2045 -Require 'old_style_'`.

## Progress and current state
No source edits have been made yet. Exploration only.

Verified current implementation at `e67bd80`:

- **Chat store** (`store.rs` `initialize`, ~405-530; read call `call_00_gze0j513xk5mdyab824cbvsa`): max supported version is **9** (`version <= 9` and `(1..=9).contains`). Fresh DB sets `application_id=1112556080` and `user_version=1`, then applies v2-v7, `migrate_request_usage` (v8/v9), approvals/event-log/host-state schemas, reminders, `ensure_checkpoint_schema`, cold init, then `initialize_tool_call_index`. Both item 4 and item 3 therefore touch the same chat chain.
- **Split chat-v1 path** (`store_layout.rs` ~860-890): requires application `0x42504230` and version `(7..=9)`; calls `migrate_request_usage`, reminders, `ensure_checkpoint_schema`, cold init, then `initialize_tool_call_index`. This path bypasses `Store::initialize`, so new chat migrations must run here too.
- **Catalog** (`store_layout.rs` `projection` ~120-150): application `0x42504231`; supported version is 2. Every catalog open starts a transaction, executes `CATALOG_SCHEMA`, `QUOTA_SCHEMA`, then `quota::migrate_catalog(&tx)`, and bumps to 2 only if old version <2.
- **Serving projection** (`store_layout.rs`): application `0x42504232`; `SERVING_SCHEMA` currently includes `PRAGMA user_version=1` and is executed on every serving open. `prompt_log::setup` then runs every open: drops legacy `usage`, sets pragmas, creates `model_requests`/`prompt_tiers`, and inspects `pragma_table_info` to add 7 nullable columns.
- **Tool-call index** (`store_serving.rs` ~216-255): checks whether `message_tool_calls.kind` exists; if absent, drops/rebuilds the table and backfills from `messages` in one transaction, then calls `initialize_message_headers` (still unversioned, not one of the five listed items).
- **Checkpoint schema** (`store.rs` ~3540-3578): creates checkpoint tables every open and rebuilds `checkpoint_records` if `preimage_hash` is `NOT NULL`, preserving rows.
- **Ledger** (`ledger.rs` ~60-140): six `ADDED_COLUMNS` for `delegation_attempts`/`delegation_calls`, checked per-column via `PRAGMA table_info` in `migrate()`. Current ledger open flow/versioning has not yet been inspected; need to read the surrounding ledger setup before editing.

Material discovery: task item 1 says `migrate_catalog` is called from `store_layout.rs` ~147 and `store_quota.rs` ~116. The only observed call is `store_layout.rs` ~147; `store_quota.rs` ~116 is the read-only `has_column` fallback in `readings`, which emits `NULL` for `balance` when a v1 catalog is opened read-only. Preserve that fallback or verify whether removing the per-open migration affects read-only CLI access.

Critical pitfall: `SERVING_SCHEMA` contains `PRAGMA user_version=1`. Bumping serving to v2 while still executing this batch would reset the version to 1 on every open. The version pragma must be moved to fresh initialization only. `SERVING_SETUP` also contains connection-level pragmas (`synchronous=NORMAL`, `cache_spill=OFF`) that cannot safely be moved into the versioned transaction; split them from the schema step.

## Planned version mapping (not implemented)
- Item 1 catalog: keep **v2**; make the `balance` ALTER part of the v1→v2 step only, with a one-time column check for catalogs where old code already added it. Avoid a transaction on already-v2 opens.
- Item 2 serving projection: **v2**; move the 7 added columns into the v1→v2 step; raise `supported` to 2. Fresh serving DB gets full current schema and final v2.
- Item 4 chat checkpoint: **v10**; create current checkpoint schema and conditionally relax `preimage_hash` from NOT NULL, then bump.
- Item 3 chat tool-call index: **v11**; conditionally rebuild `message_tool_calls` if `kind` is absent, backfill, then bump.
- Chat validation ranges must rise from 9 to 11 (`store.rs` initialize checks, split chat-v1 range in `store_layout.rs`, and `schema_compatibility.rs` assertion currently expects 9).
- Item 5 ledger: first numbered step **v1** (or next appropriate number after inspecting existing ledger metadata), bringing older ledgers to current shape exactly once.

## Evidence
- Task and constraints: typed request `254e72ca-b1b6-43a9-8907-ebb84f286eab`.
- Initial listing and clean-looking `git log` at `e67bd80`: `call_00_bf116fi2xcmvdf1peq3jg0hb`.
- Reads of `store_layout.rs`, `store_quota.rs`, `store_prompt_log.rs`, `store_serving.rs`: `call_00_gqkookuk86yomh50s6gqux0b`.
- Search for checkpoint/tool-index/helper call sites: `call_00_ca483kuy5vsbcfamv4pbf7qs`.
- Detailed reads of `store.rs` ~490-530, split chat path, and `store_serving` functions: `call_00_gze0j513xk5mdyab824cbvsa`.
- Existing fixture pattern observed in `crates/bubblepaw-core/tests/schema_compatibility.rs`: `include_str!("fixtures/main-v6-schema.sql")`, `agent-v4-schema.sql`, helper inserts a session/message/input, and `assert_schema` currently asserts chat `user_version == 9`.

## Open
Implementation has not started. Next steps: inspect full ledger open/initialization path and all hardcoded chat version references; implement versioned migrations in `store.rs`, `store_layout.rs`, `store_quota.rs`/quota step, `store_prompt_log.rs`, `store_serving.rs`, and `ledger.rs`; add SQL fixtures and `old_style_*` tests for catalog, serving, chat checkpoint, chat tool calls, and ledger, including both column-present and column-absent cases; update `schema_compatibility` version assertions and `file-size-allowlist.txt` exact line counts; update docs. Then rely on the unrun CI acceptance scripts; no cargo commands may be executed directly.
````

**v3**

````text
# Migrating five unversioned SQLite migrations to numbered steps

Episode only explored the code; no files were changed, so the whole conversion and its tests remain to be written.

## Goal and constraints
Delegated task (source `254e72ca`): remove the five per-open, unversioned migration styles and make each a numbered `PRAGMA user_version` step in the chain of the database it touches. Items: (1) `store_quota.rs::migrate_catalog` balance ALTER, folded into catalog v1→v2; (2) `store_prompt_log.rs::setup`'s 7 `ADDED_COLUMNS` on the serving projection; (3) `store_serving.rs::initialize_tool_call_index` kind-column rebuild; (4) `store.rs::ensure_checkpoint_schema` preimage_hash rebuild; (5) runner `ledger.rs::migrate` 6 `ADDED_COLUMNS` (ledger currently has no chain, so its first step brings old ledgers to current shape).

Settled design from the request: a converted step may inspect once (old code may already have applied it) but must bump the version in the same transaction, so later opens do no schema inspection; fresh stores still get the full schema; no extra transaction on an already-current open. Tests: fixture-based `old_style_*` tests (SQL fixtures under `crates/bubblepaw-core/tests/fixtures/`, runner under `crates/bubblepaw-runner/tests/fixtures/`, no .db binaries), each covering both "column already present with old version" and "column absent", asserting new version, new column/constraint, and surviving row. Keep existing tests green.

House rules: never run cargo, no `git stash`, no commit/push (harness does that plus CI). `file-size-allowlist.txt` is an exact ratchet — changed listed files need exact new line counts, files ≤500 lines must be delisted, new files >500 lines added; prefer new small files. Clippy `-D warnings`; no AI attribution. Scope: core `store*.rs` and other core src as needed, `bubblepaw-runner/src/delegate/ledger.rs`, both crates' `tests/` + fixtures, the allowlist, migration paragraph of `docs/storage-model.md` and `docs/delegation-bridge.md`. Do not touch `Store::scoped` in `store_layout.rs` ~770–900 (another worker).

## Progress and findings (verified in code)
- Base is `e67bd80`; worktree clean on entry (`git log`/`git status` in `call_00_bf116fi2xcmvdf1peq3jg0hb`).
- Chat chain (`crates/bubblepaw-core/src/store.rs`): `initialize` guards `version <= 9`, fresh DB sets `application_id=1112556080`/`user_version=1` with the v1 tables, then v2…v7 blocks in `store.rs`, then `migrate_request_usage(&mut connection, version)` (v8, v9), then `approvals::SCHEMA`/`event_log::SCHEMA`/`host_state::SCHEMA`, `initialize_reminders`, `ensure_checkpoint_schema(&connection)` at line 511, `cold::initialize`, `initialize_tool_call_index(&mut connection)` at line 514. Note the chain reuses the *pre-migration* `version` variable for later steps, which only works because steps are cumulative.
- Split (chat-v1) records take a different open path (`store_layout.rs` ~840–885, read in `call_00_gze0j513xk5mdyab824cbvsa`): requires app `0x42504230` and version `7..=9`, then calls `migrate_request_usage`, `initialize_reminders`, `ensure_checkpoint_schema` (line 876), `cold::initialize`, `initialize_tool_call_index` (line 879) — i.e. these chat-v1 records never run `initialize()`. Any new chat step (v10) must therefore also be reachable/executed from this path, or those stores would stall at a version their next open can't advance.
- Catalog/serving (`store_layout.rs::projection`): `supported` is 2 for app `0x42504231` (catalog) else 1. Catalog branch runs one transaction: `schema`, `QUOTA_SCHEMA`, `quota::migrate_catalog(&tx)`, and `PRAGMA user_version=2` when `version < 2`. Serving branch (app `0x42504232`) runs `prompt_log::setup(&conn)`, `Store::search_schema(&conn)`, then ATTACHes `catalog.db`. `SERVING_SCHEMA` itself embeds `PRAGMA user_version=1`, so raising serving to v2 needs that literal moved/guarded.
- `store_prompt_log.rs::setup` = `SERVING_SETUP` (DROP `usage`, pragmas, `CREATE TABLE IF NOT EXISTS model_requests` with the pre-column shape, `prompt_tiers`) plus the `pragma_table_info` loop over `ADDED_COLUMNS` (cache_write_tokens, output_tokens, reasoning_tokens, reasoning_effort, breakpoints, cache_lifetime_ms, likely_expired). Insert/select statements already reference all 7 columns.
- `store_quota.rs` also has `has_table`/`has_column` helpers used by `readings(View)` to emit `NULL` when `balance` is absent — that code tolerates reading a v1 catalog ("Read-only CLI access may still see a v1 catalog without the column"), so removing the helper use there needs care.
- `store_serving.rs::initialize_tool_call_index`: if `kind` exists → `initialize_message_headers`; else a transaction drops/rebuilds `message_tool_calls` (kind CHECK) and repopulates from message bodies, then `initialize_message_headers` (which itself is unversioned and creates `message_headers` if absent — not in the listed five but adjacent).
- `store.rs::ensure_checkpoint_schema` (line 3558): `CHECKPOINT_SCHEMA` (`CREATE TABLE IF NOT EXISTS` for `checkpoint_blobs`, `checkpoint_records` with nullable `preimage_hash`, two indexes) then a `pragma_table_info` notnull probe and, if notnull, a `BEGIN…COMMIT` rebuild through `checkpoint_records_relaxed`. Runs on every open/chat-v1 open.
- Runner ledger (`crates/bubblepaw-runner/src/delegate/ledger.rs`): `ADDED_COLUMNS` has exactly the 6 described entries; `migrate()` loops `PRAGMA table_info` + `ALTER TABLE`.
- `crates/bubblepaw-core/tests/schema_compatibility.rs` uses `include_str!` fixtures (`main-v6-schema.sql`, `agent-v4-schema.sql`), a `fixture()` helper inserting a session + message + input, and `assert_schema` asserting `user_version == 9`, presence of agent/context tables, `PRAGMA foreign_key_check` empty, and serving-vs-chat table placement — this is the template for the `old_style_*` tests, and its `user_version == 9` assertion will need to move if a chat v10 step is added.
- One search for `has_column|has_table` returned zero matches despite the helpers existing in `store_quota.rs` (`call_00_ca483kuy5vsbcfamv4pbf7qs`), so the text-search tool appears unreliable for regex/alternation — do not treat its misses as absence.

All five items match the description in the request as far as the code read; nothing was found to contradict it.

## Evidence and verification
Only reads/searches so far (`call_00_bf116fi2xcmvdf1peq3jg0hb`, `call_00_gqkookuk86yomh50s6gqux0b`, `call_00_ca483kuy5vsbcfamv4pbf7qs`, `call_00_gze0j513xk5mdyab824cbvsa`), all `ok`. No builds or tests were run, per the no-cargo rule.

## Planned version numbers (not yet implemented)
- Chat store: v10 carrying the checkpoint `preimage_hash` relaxation and the `message_tool_calls` kind rebuild; must also run for chat-v1 split records currently at 7–9.
- Catalog: 2 (existing step; the balance ALTER becomes part of v1→v2 with a one-time presence check).
- Serving projection: 2 (the 7 added `model_requests` columns); `supported` becomes 2 for app `0x42504232`.
- Delegations ledger: first step v1 with the 6 columns.
Fresh-store schemas must still be the full current shape.

## Actions
- Read `crates/bubblepaw-core/src` listing and `git log`/`git status` · tree clean at `e67bd80` [call_00_bf116fi2xcmvdf1peq3jg0hb]
- Read `store_layout.rs` 1–200, `store_quota.rs` 1–60, `store_serving.rs` 1–60, `store_prompt_log.rs` 1–120 [call_00_gqkookuk86yomh50s6gqux0b]
- Searched for `ensure_checkpoint_schema`, `initialize_tool_call_index`, `initialize_message_headers`, `has_column|has_table` · three found, last none [call_00_ca483kuy5vsbcfamv4pbf7qs] ! risk: alternation search misses known identifiers
- Read `store.rs` 490–550, `store_layout.rs` 820–940, `store_serving.rs` 210–330 · confirmed both open paths and the per-open migrations [call_00_gze0j513xk5mdyab824cbvsa]

## Open
Nothing implemented. Next: confirm the remaining call sites and file sizes (`file-size-allowlist.txt` entries for `store.rs`, `store_layout.rs`, `store_serving.rs`, `store_prompt_log.rs`, `store_quota.rs`, `ledger.rs`), then write the chat v10 step that both `Store::initialize` and the chat-v1 split branch in `store_layout.rs` invoke (the split branch must not stay on a bare `ensure_checkpoint_schema` + `initialize_tool_call_index` pair), the serving v2 step replacing the per-open `ADDED_COLUMNS` loop with `supported = 2`, the catalog v2 fold-in, and a new ledger chain; then the four `old_style_*` tests with SQL fixtures, update `assert_schema`'s expected version, adjust the allowlist ceilings, update the migration paragraphs in `docs/storage-model.md` and `docs/delegation-bridge.md`, and report per-item version/tests/checks. Avoid the `Store::scoped` region of `store_layout.rs`. Do not run cargo; the harness acceptance checks are the guard/ci scripts (`ci-check.ps1 -MinPassed 2045 -Require 'old_style_'`).
````

#### Episode 2

Source `791b9ca4` messages `7c1a8183`..`60af991f`: 26818 source tokens, 7 tool calls, 0 mutating.

**v3 title and gist**

````text
# Migration conversion still in exploration; nothing changed yet
Episode confirmed the remaining call sites for the five unversioned migrations but wrote no code; the whole conversion, the four `old_style_*` fixtures/tests and the doc/allowlist updates remain.
````

**v3 Actions**

````text
## Actions
- Searched `migrate_catalog`, `quota::|prompt_log::`; read ledger.rs 1–60 and 140–260 · found the `record()` call site; alternation search empty [call_00_tx8rxn3rhou7ryhbzkelvuvs] ! risk: search tool misses alternation
- Read `store_quota.rs` 88–178 · `record()` calls `migrate_catalog` inside its own write transaction [call_00_c0n4zk6aq4xun95jwhcghhnw] ! decision: that call must go when the ALTER moves into catalog v2
- Searched `message_tool_calls`, `message_headers`, `fn search_schema` · mapped all readers and the tests that drop those tables [call_00_jjh60i07ihnfplhd9yxe4vmq]
- Read `storage_cold.rs` 620–700, runner `reasoning_storage.rs` 460–520, `store_layout.rs` 500–550 · PhaseB tests assert `user_version == 9` and need 10 [call_00_s5pyrr29hcu2r7kjfstpx7l8]
- Ran `Get-Command sqlite3,python,py`; listed both fixture/test dirs · no sqlite3 CLI; python present; core fixtures are main-v6/agent-v4/v7-additions/generate_synthetic.py [call_00_4w725t9h1o57d94kg20ghveu]
- Read `v7-additions.sql` and `main-v6-schema.sql` · template for the new `old_style_*` SQL fixtures [call_00_tbk3h52yp60y0s25rnmpd2vr]
- Read `store.rs` 330–410; searched `pub fn open` · confirmed open/`open_data_dir`/`initialize` routing [call_00_kwk3rrm35x1jswd48p7j3izr]
````

#### Episode 3

Source `791b9ca4` messages `37518b55`..`dbdc3769`: 26222 source tokens, 7 tool calls, 0 mutating.

**v3 title and gist**

````text
# Migration conversion: call-site/allowlist reconnaissance complete, no edits yet
Episode mapped every test assertion, allowlist ceiling and ledger entry point that the five versioned-migration conversions will touch; still zero code written.
````

**v3 Actions**

````text
## Actions
- Read `store_layout.rs` 660–820 · open/projection/lock path, `scoped` boundary [call_00_yhsoz0ltu04q9ownkxi6c98z]
- Searched `user_version` in `core/tests` and `runner` · 12 chat-version-9 sites plus storage_layout literals; runner has none [call_00_o08qwtgnr9uelhw3kvxfzjtn]
- Read `tests/support/mod.rs`, `store_storage.rs`, `context_checkpoint.rs`, `request_usage.rs` excerpts · version assertions and the drop-table/reopen pattern [call_00_jcw5kgtvqrqer58ig7t1owq2]
- Read 7 version-assertion sites incl. `quota.rs` v1-catalog test · existing catalog v1→v2 coverage confirmed [call_00_c2ku430kom60zpknyfz344j3]
- Read `file-size-allowlist.txt` and `tests/file_size.rs` · exact-count ratchet semantics and ceilings [call_00_u7ozmlyy37y1reohxbq4zi16]
- Searched `Ledger::open`, `migrate\(`, `pub mod delegate` ; read `ledger.rs` 560–640 · all ledger open sites and the SCHEMA-replace tests [call_00_k7mp9u20aktt37ytokc5nveh]
- Read `ledger.rs` 380–560 · `merge_copy` migrates the copy via `Ledger::open`, column-set copy [call_00_jbbr77w6u6vjx45efxtdp5mp]
````

#### Episode 4

Source `791b9ca4` messages `25ab2fe9`..`34f660a7`: 24868 source tokens, 9 tool calls, 0 mutating.

**v3 title and gist**

````text
# Migration conversion: call sites and docs located, no code written yet
Episode completed reconnaissance of chat-record version checks, ledger schema text, fixtures and docs; conversion code still not started.
````

**v3 Actions**

````text
## Actions
- Read `store_layout.rs` 1150–1350 · record activity/catalog rebuild internals [call_00_gpnz95jt3p99vnx75m9jfksy]
- Read `ledger.rs` 40–140, `store_search.rs` 15–45; listed runner fixtures; searched `lib.rs` for `delegate` [call_00_pd1vanrfy9vfxvpguw7v4tws]
- Read `ledger.rs` 95–140 and searched `user_version` repo-wide · chat version gates + all 9-assertions mapped [call_00_2ah12jzq6jwraftislpuls5k]
- Read `store_layout.rs` 220–250, `store_exports.rs` 70–100, `src/tests.rs` 1925–1945 · `open_record` `(1..=9)`, catalog `(1..=2)`, unit test 9 [call_00_46w6k94ndne0c7zjewn5frc5]
- Searched docs for `user_version` and `migrat` · migration paragraphs and quota.md references found [call_00_ka3d0mgcld4iwgmrteg2d835]
- Read `docs/storage-model.md` 495–545, 600–680, 250–300 · migration/paragraph and ledger-copy wording [call_00_7hx5tyw0fs7h31pq77a804p8]
- Ran `Get-ChildItem` / `git config core.autocrlf` / `rustfmt.toml` probe · 13 root entries, autocrlf=true, no rustfmt/.gitattributes [call_00_t8sd758mvot8g0wuw4yf4abd, call_00_e3k9w7gzi1gtaitt1e0xe9yp, call_00_jgnwj0dtnu3apytaxsdysuo7]
````

#### Episode 5

Source `791b9ca4` messages `ef7958a8`..`fbca55b1`: 25065 source tokens, 19 tool calls, 10 mutating.

**v3 title and gist**

````text
# Migration conversion: code edits landed, tests not yet updated
Episode applied all five numbered-step conversions across core and runner; the test-expectation bump partly landed but three `user_version` assertions still read 9 and the new fixtures/tests are absent.
````

**v3 Actions**

````text
## Actions
- Read `docs/storage-model.md` schema lines (388, 583 area) and searched `schema`; read `docs/delegation-bridge.md` 100–120 · located migration/ledger paragraphs [call_00_v8cshiwcunxc0ovfbi45piad, call_00_f0i991si8ctsjk2jrq1qla0v, call_00_bml712h8fjjobvko3qqq91rt, call_00_tdb416wrnq3k3rlb8anamp4c]
- Edited `store.rs` · `<=11`, `(1..=11)`, call site → `migrate_checkpoint_schema(&mut connection, version)` [call_00_vumx8nsij5gd8pcxku59rh76]
- Edited `store.rs` · replaced `ensure_checkpoint_schema` with transactioned step 10 function [call_00_xpzobb12xdxtwcv1vo9hges1]
- Edited `store_layout.rs` · version gates to 11, chat-v1 call site, `SERVING_SCHEMA` literal dropped [call_00_2ixwwwzqfk4aptek3wb82bey]
- Edited `store_layout.rs` · `supported = 2`, catalog step-2 branch, serving step-2 branch [call_00_jxjx1sndu8s5ju0ncvdga3yu]
- Edited `store_prompt_log.rs` · `setup` slimmed, new `migrate` step 2 [call_00_174qom76ibdwlc2rbtfsmuuv]
- Edited `store_quota.rs` · doc + removed per-open `migrate_catalog` call in `record` [call_00_6u318tu57fb805h333m5inry]
- Edited `store_serving.rs` · step 11 gate, new `rebuild_tool_call_index`, borrow fix [call_00_hcsod5enagc6eajjpqito3xg, call_00_au9zj8pqd7cqmku6mojekhrv]
- Edited `ledger.rs` · `&mut` open, `VERSION = 1`, transactioned `migrate` [call_00_fc9gynbh6qw5mtznhn31mbq8]
- Read `tests/support/mod.rs` 1–60, 140–200 and searched `support::` uses · legacy fixture helper mapped [call_00_0lijgtifo7gwdbsohz5oxkg8, call_00_41dxwalqizp5qybr62kg1592]
- Ran batch of `user_version` edits · failed "matches 2 occurrences" in `request_usage.rs`, state reset ! error: ambiguous match aborted the batch [call_00_16whg0jr97i1dljdryr3jtiv]
- Ran `Select-String 'PRAGMA user_version'` over tests+src · mapped every remaining 9/8/7 site [call_00_784thii8zu0iee84126x40w5]
````

#### Episode 6

Source `791b9ca4` messages `2c164589`..`436f47b6`: 24393 source tokens, 12 tool calls, 4 mutating.

**v3 title and gist**

````text
# Migration conversion: version bumps finished, SQL fixtures created, fixtures unvalidated
Episode completed the remaining chat `user_version` bumps to 11 and created five SQL fixtures (four core, one runner), but the ad-hoc Python validation of the fixtures failed to launch, so no fixture has been executed.
````

**v3 Actions**

````text
## Actions
- Edited 5 files (`request_usage.rs`×2, `storage_cold.rs`, `store_storage.rs`, `src/tests.rs`) · user_version 9 → 11 · all `replacements: 1` [call_00_ad7z5pn2o8m2nhz256sbnore]
- Searched `user_version`, `=9` patterns repo-wide · mapped remaining producers/assertions; surfaced 5 uninspected readers [call_00_aq9xdd1jk9qp11492arx1rbp]
- Read `store_layout.rs` 100–190 · verified current `projection` code [call_00_eb1v4dr5777piog8k91vwdtz]
- Read `approvals/mod.rs:430`, `store_reminders.rs`, `store_cold.rs:80`, `host_state.rs:8` · confirmed additive unversioned schemas [call_00_t9p57aoe39ep54fhx6t2hvqa, call_00_miyg8lf08wgtceotsm8dp6c2]
- Read `ledger.rs` 1–100 · derived first-release schema by stripping 6 columns [call_00_a0g1tnr0i0zcyupvskkx5pkk]
- Created `crates/bubblepaw-runner/tests/fixtures/ledger-v0-schema.sql` · 2498 bytes [call_00_cqrs7zh64p57dar0fxo621pr]
- Created 4 fixtures under `crates/bubblepaw-core/tests/fixtures/` · catalog-v1, serving-v1, chat-v8-v9-additions, chat-v9-per-open [call_00_cwb05kupo9xk93350chigilm]
- Read `store_layout.rs` 960–1170 · catalog_rebuild per-chat skip path [call_00_y7noxekbn4020cn1j78le7bf, call_00_qzf231nsoletrxddls9wvpvpu]
- Read `quota.rs` 1–45, searched `DELETE FROM model_requests` · one prune site only [call_00_oeh7j347ipq3xux7w4t0vas7]
- Ran temp-dir Python fixture check · failed: write tool requires a workspace-relative path; state reset, script never executed ! error: fixtures remain unvalidated [call_00_joen1v1fdozjjmvfd80puyt3]
````

#### Episode 7

Source `791b9ca4` messages `52313039`..`18ba6ed7`: 24348 source tokens, 14 tool calls, 9 mutating.

**v2**

````text
# Numbered store migrations: fixtures verified, old_style tests and docs written, allowlist/CI pending

## Goal and constraints
Delegated child task `254e72ca-b1b6-43a9-8907-ebb84f286eab`: convert five per-open SQLite schema mutations into numbered `PRAGMA user_version` steps (catalog v2, serving projection v2, chat checkpoint v10, chat tool-call index v11, ledger v1), keeping every store release `e67bd80` can produce openable with data intact. Edits limited to core `store*.rs`, `crates/bubblepaw-runner/src/delegate/ledger.rs`, tests/fixtures in both crates, `file-size-allowlist.txt`, the migration paragraph of `docs/storage-model.md`, and `docs/delegation-bridge.md`. Do not touch `Store::scoped` in `store_layout.rs` ~770-900. Never run cargo, never `git stash`, no commits; no AI attribution. Acceptance scripts: `guard.ps1` (allowed paths) then `ci-check.ps1 -Branch s5/check-migrations -MinPassed 2045 -Require 'old_style_'`.

## Progress this episode
- **Fixture SQL verified.** A Python checker (written workspace-relative as `_migcheck.py`, run `python _migcheck.py`, exit 0) at `call_00_8qrtv9h9f861a5q8q3b8bl94` confirmed: `catalog-v1-schema.sql` → `user_version 1`, `quota_history` columns `id,provider,account,window,data` (no `balance`); `serving-v1-schema.sql` → version 1, `model_requests` 23 columns (7 added ones absent), `prompt_tiers` present; chat fixture chain (`main-v6-schema.sql` + `v7-additions.sql` + `chat-v8-v9-additions.sql` + `chat-v9-per-open-schema.sql`) → version 9, application_id 1112556080, `message_tool_calls` = `session_id,call_id,message_seq` (no `kind`), `checkpoint_records.preimage_hash` NOT NULL = true, `request_usage` includes `account`, metadata marker `storage_layout=chat-v1`, `context_views` absent, `one_running` index text as expected; after replaying the two old per-open scripts in SQL, `kind` present and `preimage_hash` nullable while version stays 9 (the "old code already applied" case is representable). `ledger-v0-schema.sql` → version 0, no `wire_bytes`/`undeclared_busts`; after the six ALTERs + `user_version=1` → 19 `delegation_calls` / 61 `delegation_attempts` columns.
- **Core test file created:** `crates/bubblepaw-core/tests/migration_style.rs` (first write `call_00_10trl3nyvll5jcuomzz05mc7`, then rewritten whole at `call_00_b7ewqruf0ephl8lwdu4rwyfn`, one small follow-up edit `call_00_3uhwlhudmvz0zqpa5qt1izms`). Contains `old_style_catalog_opens`, `old_style_serving_opens`, `old_style_chat_checkpoint_opens`; each loops `for applied in [false, true]`, builds the fixture directly with `rusqlite`, opens via `Store::open_data_dir`, asserts new version (2 / 2 / 11), the new column or relaxed constraint, and row survival, then reopens to assert no rewrite. Earlier `!reopened.storage_stats()?.catalog_transaction_count` and `latest_quota_balances()`/`model_requests()` assertions were deliberately replaced with raw-SQL counts by `call_00_pi6pl6c4b94ctq8msoln1d5k`/`call_00_4ujw25tebw8vq5kvqcrbqaky` — do not reintroduce those APIs without checking they exist.
- **Runner test file created:** `crates/bubblepaw-runner/tests/migration_style.rs` (`call_00_hpbgamx4gfz7mq2ob3drhp2c`), test `old_style_ledger_opens`, loops applied/unapplied, uses `Ledger::open`, `ledger.attempt_exists("old-attempt")`, asserts version 0 → 1 and all six columns present. **Unverified assumption:** that `bubblepaw_runner::delegate::ledger::Ledger` exposes `open(path)` and `attempt_exists(id)`, and that the 35-column `delegation_attempts` INSERT list matches the fixture column order/count (long single-line INSERT, line 45).
- **Docs updated:** `docs/storage-model.md` two CRLF-aware edits (`call_00_o63btt5c7unujp1rvfi60vu1`): chat schema wording 8 → 11 (nullable checkpoint preimage, kinded tool-call index) and a new paragraph stating every schema change is a numbered `user_version` step per database (chat 1–11, catalog 1–2, serving 1–2, ledger 1), steps check-then-bump in one transaction, no inspection on a current open, fresh stores at current shape, derived index tables rebuilt when missing. `docs/delegation-bridge.md` one edit (`call_00_b9eyt4lsic6pe0i9r372aca6`) documenting ledger step 1.

## Evidence and verification
- Fixture behaviour: observed output of `call_00_8qrtv9h9f861a5q8q3b8bl94` (exit 0) as detailed above.
- All writes/edits returned `ok`; no cargo/fmt/clippy/test run this episode, so the new tests are uncompiled and unrun.
- Attempt to read `guard.ps1` from the scratchpad failed (`call_00_bqw3es2cgljul20jfxg1iesp`: "path escapes workspace"), so the guard script contents are unknown.
- **Invalid data to discard:** the line-count sweep at `call_00_jvwoww01wng4ldsvjxz631z1` reported `store.rs` 132 and `store_layout.rs` 106 lines, which contradicts the known multi-thousand-line `store.rs`; the read tool clearly truncated content, so **none of those counts may be used for the `file-size-allowlist.txt` ratchet.** Counts must be re-measured with a real file-line counter (e.g. via `exec` running a shell/Python one-liner).

## Open
- Delete the leftover scratch file `_migcheck.py` at the repository root: it is a new untracked file outside the guard's allowed paths and would fail `guard.ps1`.
- Re-measure real line counts of every changed listed source file (`crates/bubblepaw-core/src/store*.rs`, `store_serving.rs`, `crates/bubblepaw-runner/src/delegate/ledger.rs`, and any other allowlisted file touched) and update `file-size-allowlist.txt` to exact ceilings; remove entries that dropped to ≤500 lines and add any new/grown >500-line file. Known-suspicious from the invalid sweep: `store_quota.rs`, `store_serving.rs`, `ledger.rs`.
- Confirm the APIs used by the new tests exist with the right signatures (`Ledger::open`, `attempt_exists`; `Store::open_data_dir`, `create_session`, `chat_path`, `messages`; `StorageStats`), and that the catalog fixture sets `PRAGMA application_id=1112556080+1` (the checker printed version but not application_id for catalog/serving), otherwise `Store::open_data_dir` will refuse the fixture.
- Sanity-check the catalog test's `Store::open_data_dir(dir.path())` path expectation (`catalog.db` name) and the chat test's file replacement (removing `-wal`/`-shm`, then `PRAGMA wal_checkpoint(TRUNCATE)`).
- Run `guard.ps1` then `ci-check.ps1 -Branch s5/check-migrations -MinPassed 2045 -Require 'old_style_'` and fix fmt/clippy (`-D warnings`, `--features web,bench`) and test failures; nothing committed yet.

Open: allowlist ratchet and CI verification not done; `_migcheck.py` leftover; new tests uncompiled.
````

**v3**

````text
# Migration style conversion: fixtures validated, old_style tests and docs written, allowlist counts still wrong

Episode validated all five SQL fixtures under real SQLite, created the core and runner `old_style_*` tests, updated both docs, but the allowlist line-count attempt returned truncated, unusable counts and a temporary Python script was left in the workspace.

## Goal and constraints (delegated task, source `254e72ca`)
Convert the five unversioned per-open SQLite migrations into numbered `PRAGMA user_version` steps in each database's own chain: catalog v1→v2 (`store_quota.rs::migrate_catalog`), serving projection v2 (`store_prompt_log.rs::setup`'s 7 `ADDED_COLUMNS`), chat steps 10 and 11 (`store.rs::ensure_checkpoint_schema` preimage_hash relaxation; `store_serving.rs::initialize_tool_call_index` kind rebuild), ledger v0→v1 (runner `delegate/ledger.rs::migrate`). Each step runs once inside the transaction that bumps the version, may inspect the column once, and a current open inspects nothing. Chain ends: chat **11**, catalog **2**, serving **2**, ledger **1**.

Tests must be fixture-based `old_style_*` cases covering both "old code already applied the change (old version)" and "change absent", asserting the new version, the new column/constraint and a surviving row. SQL fixtures only, no binary `.db`.

House rules: never run cargo, no `git stash`, no commit/push. `file-size-allowlist.txt` is an exact line-count ratchet. Clippy `-D warnings`. No AI attribution. Do not touch `Store::scoped` in `store_layout.rs` ~770–900. Allowed paths are exactly `crates/bubblepaw-core/src/*`, `crates/bubblepaw-core/tests/*`, `crates/bubblepaw-runner/src/delegate/*`, `crates/bubblepaw-runner/tests/*`, `file-size-allowlist.txt`, `docs/storage-model.md`, `docs/delegation-bridge.md`.

## Progress and current state

Fixture validation now has real evidence. A workspace-relative `_migcheck.py` ran under Python's `sqlite3` and exited 0 [call_00_8qrtv9h9f861a5q8q3b8bl94], confirming:
- `catalog-v1-schema.sql`: app id 1112556081, `user_version` 1, `quota_history` columns `['id','provider','account','window','data']` (no `balance`).
- `serving-v1-schema.sql`: `user_version` 1, first-release `model_requests` (23 columns), `prompt_tiers(sha256,content)`.
- The composed chat fixture (`main-v6-schema.sql` + `v7-additions.sql` + `chat-v8-v9-additions.sql` + `chat-v9-per-open-schema.sql`): `user_version` 9, app id 1112556080, `message_tool_calls` columns `['session_id','call_id','message_seq']` (no `kind`), `checkpoint_records.preimage_hash` `notnull` True, `request_usage` includes `account`, marker `('chat-v1',)` present, `context_views` absent, `one_running` re-created with `running`/`waiting_for_quota`. Running the release's two per-open bodies on it yields `kind` present and `preimage_hash` nullable while `user_version` stays 9 — exactly the "already applied" variant the test needs.
- `ledger-v0-schema.sql`: `user_version` 0, neither `wire_bytes` nor `undeclared_busts` present; after the six `ALTER`s, `user_version` 1, `delegation_calls` 19 columns, `delegation_attempts` 61 columns.

Core test file created: `crates/bubblepaw-core/tests/migration_style.rs` (final 218 lines) with `old_style_catalog_opens`, `old_style_serving_opens`, `old_style_chat_checkpoint_opens`; each loops `for applied in [false, true]`, builds the fixture at the old version, opens it via `Store::open_data_dir`, asserts the new version (2 / 2 / 11), the column via a `column()` helper reading `pragma_table_info(...)`'s `notnull` flag as `Option<bool>`, a surviving row count, then reopens and re-checks version and row count. Helpers `version`, `count`, `text`, `column`; fixtures are `include_str!`/`concat!` constants; the release's per-open bodies are reproduced verbatim as `ADDED_REQUEST_COLUMNS`, `RELAXED_CHECKPOINTS`, `KINDED_TOOL_CALLS` (source of truth for the "applied" variant). The chat test creates a session through the public API to get a real `chat_path`, deletes the fresh file plus `-wal`/`-shm`, writes the v9 fixture, inserts a session and message row and a `checkpoint_blobs`/`checkpoint_records` pair with preimage `'preimage'`, then asserts the preimage text survives at version 11.

Since the first draft, a bogus assertion was replaced (an earlier `assert!(!reopened.storage_stats()?.catalog_transaction_count > 0)` was removed — that field/meaning was never verified) and inline `query_row` calls were factored into `count`/`text` helpers to keep lines short. One collision was fixed: a loop local named `column` shadowing the helper was renamed `present`.

Runner test created: `crates/bubblepaw-runner/tests/migration_style.rs` (75 lines), `old_style_ledger_opens`, looping `applied in [false, true]`, inserting a full 38-column `delegation_attempts` row into the v0 fixture, optionally applying the six `ALTER`s, asserting version 0, opening via `Ledger::open`, asserting `attempt_exists("old-attempt")`, version 1 and all six columns present, then reopening to confirm version 1 and the row.

Docs edited (CRLF-aware edit helper):
- `docs/storage-model.md`: "schema 8" sentence rewritten to schema 11 with a nullable checkpoint preimage and the call/result tool-call index; a new paragraph after the workspace-bindings line stating every change is a numbered step run once in the same transaction (chat 1–11, catalog 1–2, serving 1–2, ledger 1), that a step checks the column once so old stores only gain the version and current opens inspect no table, and that fresh stores are created current [call_00_o63btt5c7unujp1rvfi60vu1].
- `docs/delegation-bridge.md`: the `delegations.db` bullet extended with the ledger's single numbered step [call_00_b9eyt4lsic6pe0i9r372aca6].

Allowlist work is **not done and its attempted counts are invalid**. The multi-file `read` with `lineCount: 100000` returned truncated content and reported impossible numbers (`store.rs` 132, `store_layout.rs` 106, `tests.rs` 136, `store_serving.rs` 574, `ledger.rs` 648, `quota.rs` 891, `storage_cold.rs` 1065) [call_00_jvwoww01wng4ldsvjxz631z1]; these must not be used as ceilings. Only the two new files' sizes are reliable from the write results (core 218/219 lines, runner 75/76).

Earlier unchanged facts: allowlist ceilings before this episode were `core/src/store.rs 3578`, `store_layout.rs 3296`, `store_serving.rs 558`, `runner/src/delegate/ledger.rs 636`; `store_quota.rs`, `store_prompt_log.rs`, `schema_compatibility.rs` are unlisted. `tests/quota.rs::v1_catalog_migrates_in_place_and_keeps_legacy_rows_null` already covers item 1.

## Evidence and verification
Fixture execution is independently verified (Python `sqlite3`, exit 0, output quoted above) [call_00_8qrtv9h9f861a5q8q3b8bl94]. Nothing Rust-side has been compiled or run; all src conversions, both new test files and the docs edits are unverified claims. The first action of the episode, reading `guard.ps1` outside the workspace, failed with a host path-escape error [call_00_bqw3es2cgljul20jfxg1iesp] — the guard script's contents are still unknown.

Two unverified assumptions in the new tests: that `Store::open_data_dir`, `Store::create_session`, `Store::chat_path`, `Store::model_requests` and `Store::messages` are reachable as used, and that `bubblepaw_runner::delegate::ledger::Ledger` is public with `open`/`attempt_exists`. Also the runner test's single-line 612-character `INSERT` will be reformatted by `fmt` and will almost certainly trip the line-length or clippy style expectations.

## Actions
- Read `scratchpad/guard.ps1` · failed, path escapes workspace [call_00_bqw3es2cgljul20jfxg1iesp] ! error: guard contents still unknown
- Created `_migcheck.py` in the workspace and ran `python _migcheck.py` · exit 0, all five fixtures load and migrate as intended [call_00_8qrtv9h9f861a5q8q3b8bl94] ! risk: temp script left behind outside allowed paths
- Created `crates/bubblepaw-core/tests/migration_style.rs` · 9810 bytes, three `old_style_*` tests [call_00_10trl3nyvll5jcuomzz05mc7]
- Edited the same file · dropped unused `Path` import, replaced the invented `catalog_transaction_count` assertion with a null-balance count [call_00_pi6pl6c4b94ctq8msoln1d5k] ! correction: unverified field
- Read the file 96–225 · confirmed the edits landed [call_00_yftqszro95t78qimdh2zqqfz]
- Edited the same file ×3 · second-open checks made version+row-count assertions [call_00_4ujw25tebw8vq5kvqcrbqaky]
- Rewrote the whole core test file · helpers `version`/`count`/`text`/`column`, 219 lines [call_00_b7ewqruf0ephl8lwdu4rwyfn]
- Edited the same file · renamed shadowing local `column` to `present` [call_00_3uhwlhudmvz0zqpa5qt1izms]
- Created `crates/bubblepaw-runner/tests/migration_style.rs` · 76 lines, `old_style_ledger_opens` [call_00_hpbgamx4gfz7mq2ob3drhp2c]
- Edited `docs/storage-model.md` ×2 · schema 11 wording + numbered-step paragraph [call_00_o63btt5c7unujp1rvfi60vu1]
- Edited `docs/delegation-bridge.md` · ledger step 1 sentence [call_00_b9eyt4lsic6pe0i9r372aca6]
- Read 19 files for line counts · truncated output, counts invalid [call_00_jvwoww01wng4ldsvjxz631z1] ! error: cannot set allowlist ceilings from this

## Open
1. **Delete `_migcheck.py`** (workspace root, outside every allowed path); `guard.ps1` will otherwise flag it.
2. Obtain real line counts for touched allowlisted files by a method that does not truncate (`pwsh (Get-Content f).Count` is the house rule; cargo is still forbidden), then set exact ceilings for `core/src/store.rs`, `store_layout.rs`, `store_serving.rs`, `runner/src/delegate/ledger.rs`, add `store_quota.rs`/`store_prompt_log.rs` if they now exceed 500 (they do not per the earlier survey: prompt_log 186), and add the two new test files if over 500 (core 218 and runner 75 are not, so only listed files changed matter).
3. Reformat the runner test's 612-character `INSERT` into a multi-line statement and re-check both new files for clippy issues (unused `params`/`OptionalExtension`, `assert!(... == 1)` style, `let present` shadowing).
4. Verify the public API surface used by the tests (`Store::open_data_dir`, `create_session`, `chat_path`, `model_requests`, `messages`; `Ledger::open`, `attempt_exists`).
5. Still uninspected readers of `user_version` that may assert old numbers: `src/store_exports.rs:82`, `src/store_layout.rs:247`, `src/store_layout.rs:881`, `tests/support/mod.rs:248`, `tests/context_episodes/legacy_metadata.rs:141`; also `src/store_quota.rs::record` after the per-open `migrate_catalog` removal, and the `ledger.rs` in-module old-schema tests (~483, ~590).
6. Do not run cargo; acceptance is `guard.ps1 -Base e67bd80 -Allowed …` and `ci-check.ps1 -Branch s5/check-migrations -MinPassed 2045 -Require 'old_style_'`, then the per-item delegation report (catalog 2, serving 2, chat 10/11, ledger 1).
````

#### Episode 8

Source `791b9ca4` messages `b58ec586`..`007c3f89`: 15481 source tokens, unclosed tail, 7 tool calls, 1 mutating.

**v3 title and gist**

````text
# Migration style conversion: allowlist ceilings corrected, source diffs audited
All five per-open migrations are now versioned steps in code, exact line counts fixed four allowlist ceilings, and the remaining risks are the still-present `_migcheck.py`, a per-open schema probe in the tool-call index, and unverified test APIs.
````

**v3 Actions**

````text
## Actions

- Ran `Get-Content …).Count` on 19 files · exit 0, exact counts recorded [call_00_rlrcxjwdpu6ud4oojblst6x8]
- Edited `file-size-allowlist.txt` ×4 · raised store/layout/serving/ledger ceilings to 3584/3310/574/648 [call_00_qcsac47c84d68t4wzoe6uwfk]
- Ran `git diff --stat; git status --short` and searches · 19 modified, `_migcheck.py` still untracked [call_00_2jgrvovf795pwv07a4cwoaev]
- Read `store_legacy.rs` and `store_layout.rs` snippets · legacy import gates only on application id and marker [call_00_0oyqxydcvaiv3tkih5co7d05]
- Ran `git diff` on core store/quota/prompt-log/serving files · conversion paths match the design [call_00_t4tmmd5xfgnyvmx59z0im75i]
- Ran `git diff` on `store_layout.rs` and runner `ledger.rs` · catalog/serving/ledger steps and version gates confirmed [call_00_qs75zvfh3cno5smbr93ks0nw]
- Read both crate `Cargo.toml` files · test dependencies present [call_00_5v3i9so4jcqsmxjezrrlgd48]
````

## Reproducing

```text
cargo build -p bubblepaw-runner --features bench --bin bubblepaw-episode-prompt-bench
bubblepaw-episode-prompt-bench --config examples/worker-opencode-go.toml --data-dir <store copy> \
  --out <dir> --episodes 8 --max-cost-usd 2 <session>...
```

Run the bench on a copy of a store, never on the live one. It reads the summary route's normal credentials from the environment and stops before any call that would pass `--max-cost-usd`. Its only output is `<out>/episode-prompt-bench.json`.
