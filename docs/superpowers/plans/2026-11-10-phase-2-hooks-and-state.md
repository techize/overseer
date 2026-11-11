# Phase 2: Hooks and State Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Every Claude slot reports live state (working, needs you, idle) through Claude Code hooks, Jonathan gets a macOS notification when a session needs him, and after a daemon restart each session can be resumed with one action.

**Architecture:** The daemon launches `claude` with per-slot generated files (`--settings` holding Overseer's hooks, `--append-system-prompt-file` holding the role prompt) plus `--session-id`, `--name` and `OVERSEER_SLOT`. Each hook runs `overseer hook <Event>`, which forwards a small event to the daemon over the existing socket and exits. The daemon keeps an activity state per slot, publishes it in `SlotInfo`, and records sessions in a SQLite ledger so it can offer `claude --resume` after a restart.

**Tech Stack:** Rust, tokio, GPUI 0.2.2, `rusqlite` (bundled), `uuid`, objc 0.2 (notifications).

**Spec:** `docs/superpowers/specs/2026-11-09-overseer-design.md`, sections 7.1, 7.2, 7.3, 7.9 (notifications), 7.10 (ledger) and 9 item 2.

## Global Constraints

- Never edit `~/.claude/settings.json` or Jonathan's global hooks. Overseer's hooks are added per session through `--settings` (spec 4, 7.2).
- `overseer hook` must finish in under 50 ms and must never block or fail a Claude turn: on any error it exits 0 with no output (spec 7.1).
- A crash or quit of the GUI never touches sessions; a daemon restart makes every session resumable (spec 4).
- Only commands whose program is `claude` get the Overseer flags. Any other command (scripts, `sh`, tests) is launched exactly as today.
- Ledger at `~/Library/Application Support/Overseer/overseer.db` (`paths::state_dir()`), SQLite through `rusqlite` with the `bundled` feature (spec 7.10).
- No em dashes in any human-readable text the app writes. Commit messages human style, no AI language.
- Claude Code flags relied on, all present in 2.1.259: `--settings`, `--append-system-prompt-file`, `--session-id`, `--resume`, `--name`.
- The protocol stays backward compatible within the phase: new `SlotInfo` fields use `#[serde(default)]`.

## Review Focus

1. **A hook firing while the daemon is down or slow.** Claude must carry on as if no hook existed: `overseer hook` gives up after its budget and exits 0 silently.
2. **A slot started with a non-claude command** (a shell, the fake script, `OVERSEER_DEFAULT_COMMAND`). It gets no flags, no generated files, and its card shows plain Running, never a stuck "working".
3. **`/clear` or a compact inside a session** changes the Claude session id. `SessionStart` must update the ledger, or resume would reopen the old conversation.
4. **Daemon restart while a slot sits at a permission prompt.** The slot comes back as resumable, not as needs you, and the GUI does not fire a stale notification.
5. **Host variables from a Claude parent** (desktop app, SDK) must not reach slots when the daemon is spawned directly, but the same names set in Jonathan's shell rc files must survive.

Each line has its test in the owning task below.

---

### Task 1: Scrub host variables from a parent Claude (I5b)

**Files:**
- Modify: `crates/overseer-daemon/src/env.rs`

**Decisions:** these names come from the Claude desktop parent's own environment (checked 2026-11-10). They are removed only when the daemon's environment shows it was started inside a Claude session (`CLAUDECODE` set), and only from the environment the login shell starts with, exactly like `PARENT_TOKEN_VARS` today, so values a user's rc files set survive.

- Exact names: `ANTHROPIC_BASE_URL`, `CLAUDE_CODE_ACCOUNT_UUID`, `CLAUDE_CODE_ORGANIZATION_UUID`, `CLAUDE_CODE_USER_EMAIL`, `CLAUDE_CODE_OAUTH_SCOPES`, `CLAUDE_CODE_CHILD_SESSION`, `CLAUDE_CODE_HOST_SESSION_ID`, `CLAUDE_CODE_HOST_SHORT_TEMP_DIRS`, `CLAUDE_CODE_MESSAGING_SOCKET`, `CLAUDE_CODE_MESSAGING_TOKEN`, `CLAUDE_CODE_EXECPATH`, `CLAUDE_CODE_EMIT_STARTUP_TIMING`, `CLAUDE_CODE_EMIT_TOOL_USE_SUMMARIES`, `CLAUDE_CODE_STARTUP_FAILURE_RESULTS`, `CLAUDE_CODE_REPORT_FINDINGS`, `CLAUDE_CODE_TERMINAL_MCP_TOOLS`, `CLAUDE_CODE_ENABLE_ASK_USER_QUESTION_TOOL`, `CLAUDE_CODE_ENABLE_SDK_FILE_CHECKPOINTING`, `CLAUDE_CODE_EAGER_FLUSH`, `CLAUDE_CODE_DISABLE_CRON`, `CLAUDE_CODE_SIMPLE`, `CLAUDE_EFFORT`, `CLAUDE_PID`, `CLAUDE_PREVIEW_CLASSIFIER_FLOOR`, `GIT_EDITOR`.
- Exact prefixes: `CLAUDE_CODE_DESKTOP_`, `CLAUDE_AGENT_SDK_`, `MCP_`.
- Kept on purpose (user preferences that a parent also sets): `CLAUDE_CODE_NO_FLICKER`, `CLAUDE_CODE_DISABLE_TERMINAL_TITLE`, `CLAUDE_CODE_ENABLE_TELEMETRY`.

- [ ] **Step 1: Write the failing tests** in `env.rs` tests: `host_vars_dropped_inside_a_parent_session` (with `CLAUDECODE` in the own env, every name and prefix above is gone from the start env, `CLAUDE_CODE_NO_FLICKER` and `PATH` kept) and `host_vars_from_rc_files_survive` (the login shell's env setting `GIT_EDITOR=vim` and `MCP_TIMEOUT=5` keeps both).
- [ ] **Step 2: Run** `cargo test -p overseer-daemon env::` and see both fail.
- [ ] **Step 3: Implement** `PARENT_HOST_VARS: &[&str]` and `PARENT_HOST_PREFIXES: &[&str]` beside `PARENT_TOKEN_VARS`, removed by the same `drop_tokens` path in `scrub`. Rename the flag to `from_parent`.
- [ ] **Step 4: Run** the tests; all of `env::` passes.
- [ ] **Step 5: Commit** "Drop host variables a parent Claude sets from slot environments".

### Task 2: Session ledger

**Files:**
- Create: `crates/overseer-daemon/src/ledger.rs`
- Modify: `crates/overseer-daemon/Cargo.toml` (`rusqlite = { version = "0.32", features = ["bundled"] }`), `src/lib.rs`

**Interfaces:**
- Produces: `Ledger::open(path: &Path) -> anyhow::Result<Ledger>`, `Ledger::in_memory() -> Ledger`, `record_start(&self, s: &SessionRow) -> Result<()>`, `set_session_id(&self, slot: SlotId, id: &str) -> Result<()>`, `record_exit(&self, slot: SlotId, code: Option<i32>, at: i64) -> Result<()>`, `live_at_shutdown(&self) -> Result<Vec<SessionRow>>` (rows started and never exited), `mark_resumed(&self, slot: SlotId) -> Result<()>`.
- `SessionRow { slot: SlotId, session_id: String, label: String, cwd: PathBuf, command: Vec<String>, started: i64 }`.

One table `sessions(slot INTEGER PRIMARY KEY, session_id TEXT NOT NULL, label TEXT, cwd TEXT, command TEXT /* JSON */, started INTEGER, exited INTEGER, exit_code INTEGER)`: one row per slot, replaced on each start. `PRAGMA user_version = 1` for later migrations. Writes happen on a blocking thread, never on the async runtime.

- [ ] **Step 1: Write the failing tests** in `ledger.rs`: `start_then_exit_is_not_live`, `start_without_exit_is_live_after_reopen` (open a temp file, record, drop, reopen, `live_at_shutdown` returns the row), `session_id_update_replaces_the_id`, `restart_replaces_the_row`.
- [ ] **Step 2: Run** `cargo test -p overseer-daemon ledger::`; fail.
- [ ] **Step 3: Implement** the functions above.
- [ ] **Step 4: Run** the tests; pass.
- [ ] **Step 5: Commit** "Add the session ledger".

### Task 3: Launch claude with Overseer's flags and generated files

**Files:**
- Create: `crates/overseer-daemon/src/launch.rs`
- Modify: `crates/overseer-daemon/src/manager.rs` (StartSlot path), `src/lib.rs`, `Cargo.toml` (`uuid = { version = "1", features = ["v4"] }`)

**Interfaces:**
- Produces: `pub fn is_claude(argv: &[String]) -> bool` (basename of `argv[0]` is `claude`); `pub struct LaunchFiles { settings: PathBuf, role: PathBuf }`; `pub fn write_slot_files(dir: &Path, slot: SlotId, overseer_bin: &Path) -> io::Result<LaunchFiles>`; `pub fn claude_argv(base: &[String], slot: SlotId, label: &str, files: &LaunchFiles, start: Start) -> Vec<String>` where `enum Start { New(String /* uuid */), Resume(String) }`.
- Consumed by Task 4 (hook command names) and Task 7 (resume).

Decisions:
- Files live in `state_dir()/slots/S<n>/settings.json` and `role.md`, rewritten on every start.
- `settings.json` holds only `"hooks"`: `SessionStart`, `UserPromptSubmit`, `Notification`, `Stop`, `PostToolUse` (matcher `*`), each one command hook `"<overseer_bin>" hook <Event>` with `"timeout": 2`. `overseer_bin` is `std::env::current_exe()` of the daemon.
- `role.md` is copied from a template: `state_dir()/roles/worker.md` for S1 to S6, `state_dir()/roles/coordinator.md` for S0 (Task 8 writes that one). Missing templates are written from compiled-in defaults on first use and never overwritten, so Jonathan can edit them. `{slot}`, `{label}` and `{overseer}` (the binary path) are replaced on copy. Worker default: "You are {slot} of an Overseer team. Jonathan runs up to six Claude sessions side by side under Overseer, with S0 as coordinator. Your slot is {slot}, working on {label}."
- Argv for `New(id)`: base argv, then `--session-id <id> --name "S<n> <label>" --settings <settings> --append-system-prompt-file <role>`. For `Resume(id)`: the same with `--resume <id>` in place of `--session-id <id>`.
- Env adds `OVERSEER_SLOT=S<n>` for every slot (claude or not).
- The ledger row is written with the new uuid at start.

- [ ] **Step 1: Write the failing tests** in `launch.rs`: `only_claude_gets_flags` (`/bin/sh` unchanged, `/opt/homebrew/bin/claude --model x` gains the flags after its own args), `resume_uses_resume_not_session_id`, `settings_file_has_five_hooks_pointing_at_the_binary` (parse the JSON), `label_with_spaces_stays_one_argument`.
- [ ] **Step 2: Run**; fail.
- [ ] **Step 3: Implement**, and call it from the manager's start path. Non-claude commands skip the files and the uuid; their ledger row keeps `session_id` empty and is never offered for resume.
- [ ] **Step 4: Run** `cargo test -p overseer-daemon`; all pass, including the existing slot and server tests (they use non-claude commands).
- [ ] **Step 5: Commit** "Launch claude slots with generated settings, role prompt and session id".

### Task 4: `overseer hook <event>`

**Files:**
- Create: `crates/overseer/src/hook.rs`
- Modify: `crates/overseer/src/main.rs` (new `Hook { event: String }` subcommand), `crates/overseer-proto/src/msg.rs` (new `Request::Hook(HookEvent)`)

**Interfaces:**
- Produces: `pub struct HookEvent { slot: SlotId, event: HookKind, session_id: Option<String>, tool: Option<String>, detail: Option<String>, notification: Option<String> }`, `pub enum HookKind { SessionStart, UserPromptSubmit, Notification, Stop, PostToolUse }`.
- `fn parse_hook(event: &str, slot_env: &str, stdin: &[u8]) -> Option<HookEvent>`: reads Claude's hook JSON (`session_id`, `tool_name`, `tool_input`, `message`, `notification_type` when present). `detail` is a one-line summary of the tool input under 80 chars: Bash `command`, else `file_path`, else `pattern`, else empty.

Behaviour: reads all of stdin, parses, connects to the socket and sends `Request::Hook` without waiting for longer than 50 ms in total, then exits 0 with no stdout. Any failure (no `OVERSEER_SLOT`, bad JSON, no daemon, timeout) also exits 0 silently. It never starts the daemon.

- [ ] **Step 1: Write the failing tests** in `hook.rs`: `parses_post_tool_use_bash` (detail is the command, cut to 80 chars on a char boundary), `parses_notification_message`, `no_slot_env_gives_none`, `bad_json_gives_none`; an integration test in `crates/overseer/tests/hook.rs` runs the built binary with no daemon and asserts exit 0, empty stdout, under 200 ms wall time.
- [ ] **Step 2: Run**; fail.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run**; pass.
- [ ] **Step 5: Commit** "Add overseer hook".

### Task 5: Activity state in the daemon

**Files:**
- Modify: `crates/overseer-proto/src/msg.rs` (`SlotInfo.activity`), `crates/overseer-daemon/src/manager.rs`, `server.rs` (handle `Request::Hook`)
- Create: `crates/overseer-daemon/src/activity.rs`

**Interfaces:**
- Produces: `#[serde(default)] pub activity: Option<Activity>` on `SlotInfo`; `pub struct Activity { pub kind: ActivityKind, pub since: i64, pub now_doing: Option<String>, pub reason: Option<String> }`; `pub enum ActivityKind { Working, NeedsYou, Idle }`.
- `pub fn apply(prev: Option<&Activity>, ev: &HookEvent, now: i64) -> Option<Activity>`, pure.

Transition table (spec 7.3): `UserPromptSubmit` → Working (now_doing cleared). `PostToolUse` → Working, now_doing `"<tool> <detail>"`, `since` kept if already Working. `Notification` → NeedsYou with `reason` = message. `Stop` → Idle. `SessionStart` → Idle, and the event's `session_id` goes to `Ledger::set_session_id` when it differs. Events for a slot that is not Running are ignored. Exit clears `activity`. Every change emits `Event::SlotChanged`.

- [ ] **Step 1: Write the failing tests** in `activity.rs` for each row of the table, plus `post_tool_use_keeps_working_since` and `events_after_exit_are_ignored`; in `crates/overseer-daemon/tests/server.rs` add `hook_requests_change_slot_activity` (start a fake slot, send `Request::Hook` events, read `SlotChanged` events) and `session_start_with_new_id_updates_the_ledger` (Review Focus 3).
- [ ] **Step 2: Run**; fail.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test -p overseer-daemon`; pass.
- [ ] **Step 5: Commit** "Track working, needs you and idle from hook events".

### Task 6: Board and session view show activity; macOS notifications

**Files:**
- Modify: `crates/overseer-gui/src/board.rs`, `session.rs`, `app.rs`
- Create: `crates/overseer-gui/src/notify.rs`

Decisions:
- Card colours (spec 7.9): needs you orange (the loudest), working green, idle grey. Running with no activity (non-claude command) keeps today's look (Review Focus 2). The card shows the state with time in state ("Working 3m") and the now-doing line.
- Board header gains "N need you" when N > 0.
- Session panel shows the activity state and reason.
- Notification on a transition into NeedsYou (not on a NeedsYou that was already set when the GUI connected, and not for a resumable slot, Review Focus 4): title "S3 needs you", body the label and reason. Clicking it brings Overseer forward and opens that session. Use `UNUserNotificationCenter` through objc 0.2, with authorization requested once at launch; if authorization is denied the board still shows the state. The builder may fall back to `NSUserNotification` if `UNUserNotificationCenter` cannot deliver from the ad hoc signed bundle; record which in the acceptance doc.

- [ ] **Step 1: Write the failing tests:** `board.rs` `card_model_for_each_activity` (colour, state text with elapsed time, now-doing), `running_without_activity_looks_as_before`; `notify.rs` `notify_only_on_transition_into_needs_you` (pure function `fn should_notify(prev: Option<&SlotInfo>, next: &SlotInfo, first_sync: bool) -> bool`).
- [ ] **Step 2: Run**; fail.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test -p overseer-gui`; pass. Then by hand with the installed app and a real claude slot: prompt, permission request, finish; record in the acceptance doc.
- [ ] **Step 5: Commit** "Show live session state on the board and notify when a session needs Jonathan".

### Task 7: Resume after a daemon restart

**Files:**
- Modify: `crates/overseer-proto/src/msg.rs` (`SlotState::Resumable { session_id: String, at: i64 }`, `Request::ResumeSlot { id }`), `crates/overseer-daemon/src/manager.rs`, `server.rs`, `crates/overseer-gui/src/board.rs`, `session.rs`, `app.rs`, `crates/overseer/src/ctl.rs` (`ctl resume <n>`)

Decisions:
- On daemon start, every `Ledger::live_at_shutdown` row with a non-empty `session_id` becomes `Resumable` with its label, cwd and command. Rows from non-claude commands become `Exited` with no code.
- `ResumeSlot` relaunches with `Start::Resume(session_id)` in the same cwd, same base command and fresh generated files, then `Ledger::mark_resumed`. `StartSlot` on a resumable slot starts fresh and drops the resume offer.
- GUI: resumable cards are grey with "Resumable" and a Resume button; `r` on a selected resumable card resumes it. No notifications for resumable slots.

- [ ] **Step 1: Write the failing tests:** `crates/overseer-daemon/tests/server.rs` `restart_offers_resume` (start a daemon on a temp ledger with `OVERSEER_DEFAULT_COMMAND` pointing at a stub named `claude` that records its argv; shut down without stopping the slot; start a new daemon on the same ledger; `ListSlots` shows Resumable; `ResumeSlot` runs the stub with `--resume <same id>` in the same cwd), `non_claude_slot_is_not_resumable`, `exited_slot_is_not_resumable`; GUI `resumable_card_model`.
- [ ] **Step 2: Run**; fail.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test --workspace`; pass. By hand: start a real claude slot, `launchctl kickstart -k` the agent, resume it from the board, check the conversation continues.
- [ ] **Step 5: Commit** "Offer resume for sessions that were running when the daemon stopped".

### Task 8: S0 kickoff and first prompts

**Files:**
- Modify: `crates/overseer-proto/src/msg.rs` (`StartSlot` gains `#[serde(default)] prompt: Option<String>`), `crates/overseer-daemon/src/launch.rs`, `manager.rs`, `crates/overseer/src/ctl.rs` (`ctl start ... --prompt <text>`)
- Create: `crates/overseer-daemon/src/roles/coordinator.md`, `roles/kickoff.md`, `roles/worker.md` (compiled-in defaults via `include_str!`)

**Interfaces:**
- Consumes: Task 3's `claude_argv`, `Start`, the template copy.
- Produces: `claude_argv(..., prompt: Option<&str>)`, which adds the prompt as the final positional argument on `Start::New` only. `pub fn kickoff_prompt(dir: &Path) -> io::Result<String>` reads `state_dir()/roles/kickoff.md` (written from the default if missing).

Decisions:
- A first prompt is passed at launch, never typed in afterwards, because text sent before Claude's input box is ready can be lost.
- S0 started fresh with a claude command and no `prompt` gets `kickoff_prompt` as its prompt. A resumed S0 gets none, and neither does any non-claude command.
- Coordinator default role: S0 coordinates S1 to S6 from `~/Repos/2ndBrain`. Each round it checks `bd ready --json` in 2ndBrain, checks Jonathan's Jira items in open sprints with `acli` (JSON output), and runs `{overseer} ctl status`. It then proposes a table with one row per idle slot (slot, work item, folder, first prompt), plus anything it would leave for later and why. It does nothing until Jonathan says yes, and then only what he approved. Its tools are `{overseer} ctl start <n> --cwd <dir> --label "<item key>" --prompt "<text>"` for an empty or exited slot and `{overseer} ctl send <n> "<text>"` for a running one. It never stops a slot, never touches S0, and never starts a slot that `ctl status` shows running.
- Kickoff default: "Run the start-of-day round: check beads and Jira, check the board, and propose who works on what."
- Jonathan can start another round at any time by typing into S0 or with `overseer ctl send 0 "Run another round"`.

- [ ] **Step 1: Write the failing tests:** `launch.rs` `prompt_is_last_positional_on_new`, `prompt_is_dropped_on_resume`, `s0_fresh_gets_the_kickoff`, `s0_resumed_gets_no_kickoff`, `edited_template_is_not_overwritten`, `placeholders_are_filled`; `crates/overseer-daemon/tests/server.rs` `start_with_prompt_reaches_the_command` (a stub `claude` records its argv).
- [ ] **Step 2: Run**; fail.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test --workspace`; pass. By hand: start S0 from the board, check it runs the round and proposes a table, approve one row, and check that slot starts in the right folder with its first prompt.
- [ ] **Step 5: Commit** "Start S0 with a coordinator role and a kickoff round".

### Task 9: Phase 2 acceptance checklist

**Files:**
- Create: `docs/acceptance/phase-2.md`

Rows: I5b scrub checked in a slot started from inside a Claude session (`env | grep CLAUDE_CODE_ACCOUNT` empty); flags present (`ps -o args` for the slot's claude); each state shown on the card from a real session; notification fires once, click opens the session; daemon restart then resume continues the same conversation; `/clear` then restart resumes the new conversation; hook budget under 50 ms (`time overseer hook Stop </dev/null` with and without the daemon); non-claude slot unaffected; S0 kickoff round proposes a table and starts an approved slot with its prompt; a resumed S0 does not rerun the kickoff.

- [ ] **Step 1:** Write the checklist in the phase 1 format.
- [ ] **Step 2:** Run every scriptable row, fill in results, leave hand rows for Jonathan.
- [ ] **Step 3: Commit** "Add the phase 2 acceptance checklist".

---

## Decision: how S0 gets started (2026-11-10)

Jonathan chose a role prompt plus a kickoff over waiting for phase 6. The other option left S0 as a plain session until the coordinator tools exist. Task 8 builds the chosen approach on `overseer ctl` and the `bd` and `acli` CLIs, with Jonathan approving every allocation. The phase 6 MCP tools (`get_board`, `propose_plan` and the rest) later take the place of the shell commands, and the role text and approval rule stay the same.
