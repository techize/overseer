# Board Conversation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Run each slot from the board like a chat: a side pane shows the selected slot's conversation and open prompt as buttons, and a reply box, Stop and a next-waiting key drive it without opening its terminal.

**Architecture:** The hook forwards `transcript_path`, `AskUserQuestion` (PreToolUse) and `PermissionRequest` content to the daemon. A daemon `ConversationService` tails each slot's transcript, tracks its open prompt and sends `ConversationChanged`; `Answer`, `Reply` and `Interrupt` requests type keys into the slot's PTY through one per-slot key lock shared with the relay courier, after checking the slot still shows what the pane showed. The gpui board gains a right-hand pane bound to the selected slot.

**Tech Stack:** Rust workspace (tokio daemon, serde proto, gpui app), SQLite ledger.

**Spec:** docs/superpowers/specs/2026-11-25-board-conversation-design.md

## Global Constraints

- Public repo: invented names only in tests and docs (DEMO-n, demo-api, demo-ws, demo@example.test). No em dashes anywhere (code, comments, docs, commits). No emojis in commits.
- Commits: run `git log -5` first; plain subject in the existing style; message ends with a blank line then `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.
- Additive proto changes: `#[serde(default)]` on new fields of existing types, a `#[serde(other)] Other` on every new or extended enum that crosses the wire, and a test that old JSON still reads.
- No file reading on the async runtime (`spawn_blocking`); transcript tailing reads only bytes after the last offset.
- The hook stays fast and never blocks Claude Code: `AskUserQuestion` and `PermissionRequest` only report, print nothing, and never wait for an answer.
- Never type into a slot without the state check of Task 5.
- Ledger migration is step 12 in the existing `migrate` list; `VERSION` becomes 12; old ledgers upgrade keeping their rows.
- Never run the real `security` binary in tests. Never install or download packages. Do not launch the app.
- Tests: `cargo test -p <crate> <filter>` while iterating; `cargo test --workspace` before committing. Lint: `cargo clippy --workspace --all-targets -- -D warnings`; `cargo fmt --all`. Known flakes: `pre_tool_use_ignores_other_tools`, `a_listener_that_never_replies_does_not_hold_the_hook`, `slow_resolution_allows_and_records_nothing` (rerun alone).
- Doc comments: short plain-English `///` in the project's voice.

## Review Focus

1. A prompt appears after the pane last drew and the user presses Send: the reply must be refused with the text kept, never Esc typed at the unseen prompt (Task 5 test `reply_is_refused_when_a_prompt_opened_since`).
2. The transcript's last line is half written when read: it must be held until its newline, not dropped or parsed as garbage (Task 3 test `a_partial_last_line_waits_for_its_newline`).
3. A very long transcript on first read: only the last `KEEP` entries are kept and sent, the app stays responsive (Task 4 test `a_long_transcript_keeps_the_last_entries`).
4. A multi-line reply to a slot whose terminal has no bracketed paste: refused like the courier's `NO_BRACKETS`, text kept (Task 5 test `multi_line_reply_needs_bracketed_paste`).
5. The user selects another slot while text is in the reply box: the draft stays with the slot it was typed for, and Send goes to the slot shown (Task 6 test `drafts_are_kept_per_slot`).

---

### Task 1: Protocol types

**Files:**
- Modify: `crates/overseer-proto/src/msg.rs` (HookKind, HookEvent, new conversation types, Event, Request, Response)
- Modify: match arms wherever `HookKind`, `Event`, `Request`, `Response` are matched exhaustively (compiler-led), with stub server handlers returning `Response::Error { message: "not yet".into() }` for the new requests

**Interfaces:**
- Produces:
  - `HookKind` gains `PreToolUse`, `PermissionRequest`, and `#[serde(other)] Other`.
  - `HookEvent` gains `#[serde(default)] transcript_path: Option<String>` and `#[serde(default)] prompt: Option<PromptInfo>`.
  - `PromptInfo = Question { questions: Vec<Question> } | Permission { tool: String, summary: String, plan: Option<String> }` (internally tagged `kind`, plus `#[serde(other)] Other`).
  - `Question { header: String, question: String, options: Vec<String>, multi: bool }`.
  - `ConvEntry { at: i64, kind: ConvKind }`, `ConvKind = User { text } | Assistant { text } | Tool { name, summary } | Other` (tagged, `#[serde(other)] Other`).
  - `Prompt { id: String, subagent: bool, info: PromptShown }`, `PromptShown = Known(PromptInfo) | Unknown` (Unknown: a prompt Overseer can't show).
  - `ConversationView { slot: SlotId, session: String, entries_from: usize, entries: Vec<ConvEntry>, prompt: Option<Prompt> }`.
  - `Event::ConversationChanged(ConversationView)`.
  - `Request::Conversation { slot }` answered by `Response::Conversation(ConversationView)` (full, `entries_from: 0`).
  - `Request::Answer { slot, prompt: String, answer: Answer }`, `Answer = Permission { choice: usize } | Questions { answers: Vec<QuestionAnswer> }`, `QuestionAnswer = Options { picked: Vec<usize> } | Other { text: String }`.
  - `Request::Reply { slot, session: String, prompt: Option<String>, text: String }`, `Request::Interrupt { slot }`; all three answered `Response::Ok` or `Response::Error { message }`.

- [ ] **Step 1: Write failing tests** in msg.rs: `conversation_types_round_trip` (each new Request, the Event and Response round-trip through JSON); `old_hook_event_json_still_reads` (a HookEvent JSON without `transcript_path`/`prompt` decodes with both `None`); `unknown_hook_kind_reads_as_other`, `unknown_conv_kind_reads_as_other`, `unknown_prompt_info_reads_as_other`.
- [ ] **Step 2:** Run `cargo test -p overseer-proto conversation` and the others: FAIL (types missing).
- [ ] **Step 3:** Add the types; fix exhaustive matches across the workspace (`activity::apply` returns the current activity unchanged for `PreToolUse`, `PermissionRequest`, `Other`; add a test `new_hook_kinds_leave_activity_alone` in activity.rs).
- [ ] **Step 4:** `cargo test --workspace`, clippy clean.
- [ ] **Step 5: Commit** `Add conversation types to the protocol`.

### Task 2: Hook reports transcripts, questions and permission prompts

**Files:**
- Modify: `crates/overseer/src/hook.rs` (`kind`, `parse_hook`, `run`, `pre_tool_use`)
- Modify: `crates/overseer-daemon/src/launch.rs` (`HOOK_EVENTS`, `hook_shape`)

**Interfaces:**
- Consumes: Task 1 types.
- Produces: `hook::parse_hook` fills `transcript_path` from stdin `transcript_path` for every kind; `kind("PermissionRequest")` and `kind("PreToolUse")` map to the new kinds. `pub fn prompt_of(event: &str, json: &serde_json::Value) -> Option<PromptInfo>`: for PreToolUse with `tool_name == "AskUserQuestion"`, the `tool_input.questions[]` (`header`, `question`, `options[].label`, `multiSelect`); for PermissionRequest, `tool` = `tool_name`, `summary` = the existing `detail()` of `tool_input` (or the tool name alone), `plan` = `tool_input.plan` when the tool is `ExitPlanMode`.
- Behaviour: in `run`, a PreToolUse for `AskUserQuestion` sends `Request::Hook` with the prompt (same delivery and spool path as other events) and prints nothing; the existing PreToolUse lock/write checks are unchanged for their tools. PermissionRequest is sent like any other event and prints nothing (no decision).
- launch.rs: `HOOK_EVENTS` gains `"PermissionRequest"`; `hook_shape("PermissionRequest") = (Some("*"), 2)`; the PreToolUse matcher becomes `Bash|Edit|Write|MultiEdit|NotebookEdit|AskUserQuestion`.

- [ ] **Step 1: Write failing tests** in hook.rs: `transcript_path_is_forwarded` (stdin `{"transcript_path":"/tmp/demo/t.jsonl"}` on Stop gives that path); `ask_user_question_becomes_a_question_prompt` (two questions, one `multiSelect: true`, options `["demo-api","demo-ws"]` map exactly); `permission_request_becomes_a_permission_prompt` (`tool_name":"Bash","tool_input":{"command":"rm -rf target"}` gives `tool:"Bash", summary:"rm -rf target", plan: None`); `exit_plan_mode_carries_the_plan`; `pre_tool_use_for_ask_user_question_prints_nothing` (the decision function returns no output for it). In launch.rs: `permission_request_hook_is_registered_for_all_tools` and `ask_user_question_is_in_the_pre_tool_use_matcher` against the generated settings JSON.
- [ ] **Step 2:** Tests fail.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** `cargo test --workspace`, clippy clean.
- [ ] **Step 5: Commit** `Report questions, permission prompts and transcripts from the hook`.

### Task 3: Transcript parser

**Files:**
- Create: `crates/overseer-daemon/src/conversation/parse.rs` (and `conversation/mod.rs` declaring it; register `mod conversation` in lib.rs)

**Interfaces:**
- Produces: `pub struct Tail { offset: u64, partial: Vec<u8> }` with `pub fn feed(&mut self, bytes: &[u8]) -> Vec<ConvEntry>` (keeps an unterminated last line in `partial` until its newline; skips unparseable lines), and `pub fn entry_of(line: &serde_json::Value) -> Vec<ConvEntry>`.
- Rules for `entry_of` (Claude Code transcript JSONL): skip lines with `isSidechain: true` or `isMeta: true`; `type: "user"` with string `message.content`, or text blocks, gives `User` (text blocks joined with a blank line), skipping text that starts with `<system-reminder>`, `<command-`, or `<local-command-`; user lines whose content is only `tool_result` blocks give nothing; `type: "assistant"` gives one `Assistant` per `text` block and one `Tool { name, summary }` per `tool_use` block (summary: first line of `input.command`, `input.file_path`, `input.pattern`, `input.description` or `input.url`, cut to 80 chars, else empty); `thinking` blocks and every other `type` give nothing. `at` is the line's `timestamp` (RFC 3339) as unix seconds, 0 when missing.

- [ ] **Step 1: Write failing tests** with an invented fixture (a few JSONL lines in a test constant, demo names only): `user_and_assistant_text_become_entries`, `tool_calls_fold_to_one_line` (`Bash` with `command: "cargo test\n--quiet"` gives summary `cargo test`), `sidechain_meta_thinking_and_reminders_are_left_out`, `tool_results_are_left_out`, `a_partial_last_line_waits_for_its_newline` (feed half a line: no entries; feed the rest: one entry), `a_bad_line_is_skipped`.
- [ ] **Step 2:** Tests fail.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** `cargo test -p overseer-daemon conversation`, clippy clean.
- [ ] **Step 5: Commit** `Read a session transcript into conversation entries`.

### Task 4: ConversationService

**Files:**
- Create: `crates/overseer-daemon/src/conversation/service.rs`, `crates/overseer-daemon/src/conversation/prompt.rs`
- Modify: `crates/overseer-daemon/src/ledger.rs` (V12 `slot_transcripts(slot INTEGER PRIMARY KEY, session TEXT, path TEXT)` with `put_transcript`, `transcript(slot)`), `crates/overseer-daemon/src/manager.rs` (`pub fn hook_applies(&self, ev: &HookEvent) -> bool`: the running and run-token checks of `hook`, without the `agent_id` filter), `crates/overseer-daemon/src/server.rs` (spawn, route hooks, answer `Request::Conversation`)

**Interfaces:**
- Consumes: Task 1 types, Task 3 `Tail`.
- Produces:
  - `prompt.rs`: `pub struct PromptState` with `pub fn on_hook(&mut self, ev: &HookEvent) -> bool` (true when the open prompt changed) and `pub fn open(&self) -> Option<&Prompt>`. Rules: an `AskUserQuestion` PreToolUse opens a `Known(Question)` prompt keyed by `tool_use_id`; a `PermissionRequest` opens `Known(Permission)` keyed by `tool_use_id`, except for `AskUserQuestion` (ignored); a Notification with `notification_type == "permission_prompt"` while no prompt is open opens `Unknown`; a PostToolUse with the open prompt's `tool_use_id`, any UserPromptSubmit, or Stop clears it; a new prompt replaces the old. Main-conversation events only, except `PermissionRequest` with an `agent_id`, which opens a prompt with `subagent: true`. The prompt `id` is the `tool_use_id`, or `n<counter>` when there is none.
  - `service.rs`: `ConversationService::spawn(ledger, manager, events) -> Arc<Self>`, `pub fn hook(&self, ev: &HookEvent)`, `pub fn current(&self, slot: SlotId) -> ConversationView`, `pub fn open_prompt(&self, slot: SlotId) -> Option<Prompt>`, `pub fn session(&self, slot: SlotId) -> Option<String>`. `KEEP = 200` entries per slot. A hook with a new `transcript_path` (or new session) resets the slot (new `Tail`, entries cleared, full send) and stores it with `put_transcript`. On a hook, the PostToolUse/Stop/UserPromptSubmit kinds, and a 1 s poll while the slot is running, the service reads new bytes in `spawn_blocking` and sends `Event::ConversationChanged` with `entries_from` = the index of the first new entry (0 on reset), only when entries or the prompt changed. On spawn it loads each running slot's stored transcript (same session as `running_session`) from the start. A slot that exits keeps its last view until the next start.
  - server.rs: `Request::Hook(ev)` calls `conversation.hook(&ev)` when `manager.hook_applies(&ev)`, next to the existing `manager.hook`.

- [ ] **Step 1: Write failing tests**: prompt.rs `question_opens_and_its_post_tool_use_clears`, `another_tools_post_tool_use_keeps_the_prompt`, `permission_for_ask_user_question_is_ignored`, `subagent_permission_is_shown_as_subagent`, `permission_notification_with_nothing_known_is_unknown`, `user_prompt_and_stop_clear`. service.rs (temp dir transcript): `appending_lines_sends_only_the_new_entries` (two sends; second has `entries_from == 2`), `a_new_transcript_resets_the_slot`, `a_long_transcript_keeps_the_last_entries` (500 lines gives 200), `restart_reloads_the_stored_transcript`, `nothing_changed_sends_nothing`. ledger.rs `v11_ledger_upgrades_to_v12`.
- [ ] **Step 2:** Tests fail.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** `cargo test --workspace`, clippy clean.
- [ ] **Step 5: Commit** `Follow each slot's conversation in the daemon`.

### Task 5: Answer, Reply and Interrupt

**Files:**
- Create: `crates/overseer-daemon/src/conversation/keys.rs`
- Modify: `crates/overseer-daemon/src/manager.rs` (per-slot key lock), `crates/overseer-daemon/src/relay/courier.rs` (take the same lock around its paste and Return), `crates/overseer-daemon/src/server.rs` (handlers replacing Task 1 stubs)

**Interfaces:**
- Consumes: Task 4 `open_prompt`, `session`; `relay::screen::{screen_of, input_box, shows_working}`; `overseer_term::encode_paste`.
- Produces:
  - `SlotManager::key_lock(&self, id) -> Arc<tokio::sync::Mutex<()>>`, held for the whole of any typing (courier delivery, answer, reply, interrupt).
  - `keys.rs` pure: `pub fn answer_keys(prompt: &PromptInfo, answer: &Answer) -> Result<Vec<Vec<u8>>, String>`, the key chunks to type in order. Permission: `choice` n gives `[b"<n+1>"]` (digits choose in Claude Code's permission list); out of range is an error. Questions: one answer per question, in order, else an error; single-select `Options{[i]}` gives `b"<i+1>"`; multi-select gives each `b"<i+1>"` then `b"\x1b[C"` (Right, to the next tab); `Other{text}` gives the digit of the Other row (`options.len() + 1`), the text, then `b"\r"`; after the last question, `b"\r"` submits the review screen. These sequences are a best reading of Claude Code 2.1.x and are confirmed in Task 7's manual check; keep them in this one function.
  - `pub fn check_reply(shown_session: &str, shown_prompt: Option<&str>, now_session: Option<&str>, now_prompt: Option<&Prompt>) -> Result<(), String>`: the refusal text is `S<n> moved on, not sent`; an open `Unknown` prompt refuses with `S<n> is at a prompt Overseer can't show, not sent`.
  - Handlers: `Answer` checks the open prompt's id equals `prompt` and is `Known`, then types the chunks with 150 ms between them. `Reply` runs `check_reply`, then if the slot is working or has an open prompt, writes `\x1b` and waits (up to 5 s, polling the screen every 200 ms) for `input_box` to be empty and `shows_working` false, else refuses `S<n> did not stop, not sent`; then pastes with `encode_paste` (multi-line text without bracketed paste refuses with the courier's `NO_BRACKETS` text) and writes `\r` after the courier's `RETURN_DELAY`; it then checks the screen for up to 2 s and answers `S<n> did not take the reply` if the box still holds the text. `Interrupt` writes `\x1b`. A write failure answers `Response::Error`.

- [ ] **Step 1: Write failing tests**: keys.rs `permission_choice_is_its_digit`, `question_answers_walk_the_tabs_and_submit` (two single-select questions give `["1"],["2"],["\r"]`), `multi_select_toggles_then_moves_on`, `other_types_the_text`, `answer_count_must_match`; `reply_is_refused_when_a_prompt_opened_since`, `reply_is_refused_in_another_session`, `reply_at_an_unknown_prompt_is_refused`, `reply_matching_the_shown_prompt_is_allowed`; server tests with a fake slot: `multi_line_reply_needs_bracketed_paste`, `interrupt_types_escape`, `courier_and_reply_do_not_interleave` (both run; the written bytes are one whole paste then the other).
- [ ] **Step 2:** Tests fail.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** `cargo test --workspace`, clippy clean.
- [ ] **Step 5: Commit** `Answer, reply to and stop a slot from the daemon`.

### Task 6: App conversation state

**Files:**
- Create: `crates/overseer-gui/src/conversation_view.rs` (pure model for now)
- Modify: `crates/overseer-gui/src/app.rs` (state, requests, events), `crates/overseer-gui/src/lib.rs`

**Interfaces:**
- Consumes: Task 1 types; the existing `AttentionView` for order.
- Produces:
  - `pub struct Conv { view: ConversationView, draft: TextField, picks: Vec<QuestionAnswer>, status: Option<String> }` per slot in `OverseerApp.convs: [Conv; SLOT_COUNT]`.
  - `pub fn apply(conv: &mut ConversationView, ev: ConversationView)` (append from `entries_from`, or replace when `entries_from == 0` or the session differs; a gap, `entries_from > len`, replaces and asks again by returning `false`).
  - `pub fn next_waiting(attention: &AttentionView, slots: &[SlotInfo], convs: &[Conv], current: SlotId) -> Option<SlotId>`: slots that are `NeedsYou` or have an open prompt, ordered as the attention list orders slot items, then by id; the first after `current`, wrapping; `None` when none.
  - `pub fn entry_text(e: &ConvEntry) -> Option<String>` (`Tool` as `Bash: cargo test`, `Other` as `None`).
  - app.rs: on connect and on a selection change, `Request::Conversation { slot }` for the selected slot; `Event::ConversationChanged` applied to its slot (a gap re-requests); clear on connection lost. `send_reply`, `send_answer`, `interrupt` issue the requests with the shown `session` and prompt id; the draft is cleared only on `Response::Ok`; an error sets `status`.
- [ ] **Step 1: Write failing tests**: `appends_from_entries_from`, `a_new_session_replaces`, `a_gap_asks_again`, `next_waiting_follows_the_attention_order_and_wraps`, `next_waiting_with_none_is_none`, `tool_entries_read_as_one_line`, `drafts_are_kept_per_slot` (typing in S1, selecting S2 and back keeps S1's draft and S2's is empty).
- [ ] **Step 2:** Tests fail.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** `cargo test -p overseer-gui`, clippy clean.
- [ ] **Step 5: Commit** `Keep each slot's conversation in the app`.

### Task 7: The pane on the board

**Files:**
- Modify: `crates/overseer-gui/src/conversation_view.rs` (element), `crates/overseer-gui/src/board.rs` (split layout; click selects when the pane shows), `crates/overseer-gui/src/app.rs` (keys), `crates/overseer-gui/src/kanban_size.rs` (persist `pane_hidden` in `gui.toml` beside `kanban_height`), `README.md` (one short section)

**Interfaces:**
- Consumes: Task 6.
- Behaviour:
  - The board's content row splits: board on the left, the pane on the right (fixed 420 px, my call; builder may make it resizable). Hidden pane: the board is exactly as today.
  - Pane header: slot name and title, `Open session` button, the next-waiting key hint. Body: entries, newest at the bottom, auto-scroll unless scrolled up; user text and assistant text as plain wrapped text with fenced code blocks in the monospace font; tool lines muted. Prompt area: question tabs with option buttons (toggles for multi), a Submit enabled once every question has an answer; permission choices as buttons (`Yes`, `Yes, and don't ask again`, `No`) with the plan text above for `ExitPlanMode`; `subagent` label when set; `Unknown` shows `waiting on a prompt Overseer can't show` and the Open session button, with the reply box disabled. Reply box: Enter sends, Shift+Enter is a new line; placeholder `No, and tell it instead` at a permission prompt, else `Reply to S<n>`; a Stop button while working. `status` shows under the box.
  - Keys (builder confirms they do not clash with existing board keys and lists them in README): `Tab` jumps to the next waiting slot (`nothing waiting` in status when none), `Cmd+\` hides or shows the pane, `Esc` in the reply box with the slot working is Stop.
  - Clicks: with the pane shown, a single click on a card selects it; a double click or Enter opens the session (`open_card`). With the pane hidden, a single click opens, as today.
  - Manual check (the user, after install; the implementer lists it in the report): answer a real `AskUserQuestion` and a permission prompt from the pane and confirm Task 5's key sequences; fix `answer_keys` if the live UI differs.
- [ ] **Step 1: Write failing tests** for the pure parts: `click_selects_when_the_pane_shows_and_opens_when_hidden`, `pane_hidden_is_remembered` (gui.toml round trip, old file without the key reads as shown), `submit_waits_for_every_question`, `reply_placeholder_follows_the_prompt`.
- [ ] **Step 2:** Tests fail.
- [ ] **Step 3:** Implement the element, layout and keys.
- [ ] **Step 4:** `cargo test --workspace`, clippy clean, `cargo build --release -p overseer` succeeds.
- [ ] **Step 5: Commit** `Converse with the selected slot from the board`.
