# Converse with slots from the board (#92)

## Why

Most of a working day goes on opening each slot's session to read what it
said, pick from the options it offers, write a reply or a question, and
redirect it. The board only shows that a slot needs you and a one-line
reason, so every exchange means leaving the board. The user wants to run
every slot from the board the way they run a chat with Claude: read,
choose, reply, redirect, then jump to the next slot that is waiting.

## Goals

- A conversation pane on the right of the board for the selected slot. The
  board stays whole beside it (the user's answer, option A).
- The pane shows the session's back-and-forth like a chat: the user's
  messages and Claude's replies in full, each tool call folded to one line.
- A question with options shows its options as buttons; a permission prompt
  shows its choices as buttons.
- A reply box under it sends text into the session. If the slot is working,
  Send stops it first (Esc), then sends (the user's answer).
- A key jumps to the next slot waiting on the user, in attention-list order.
- The terminal stays the source of truth: everything the pane sends goes in
  as the keys the user would have pressed, so opening the session shows the
  same state.

## Non-goals

- Checking diffs. Still done in the session or the editor.
- Replacing the session view. It stays for terminal work, scrollback and
  anything the pane cannot show.
- Starting, stopping or assigning slots from the pane (the board already
  does these).
- Rendering markdown beyond plain readable text (see "Left to the builder").

## Anti-goals

- A pane that shows something the terminal does not: a button that answers
  a prompt already gone, or a reply that never reached the session.
- A pane that breaks when Claude Code's screen layout changes. History and
  prompts come from structured sources (the transcript and hook input), not
  from parsing the screen.
- Text typed on the board landing in a different slot than the one shown.

## Approaches considered

1. **Transcript plus keystrokes (chosen).** The daemon reads each slot's
   open transcript file for the history, takes the content of a question
   or permission prompt from the hook event that raised it, and sends every
   answer as keystrokes into the slot's terminal through the existing input
   path (as Kickoff and the relay courier do).
2. **Read the screen.** Parse the slot's live screen, which the daemon can
   already read, for the reply and the prompt. Less new code, but only what
   fits on screen, no real history, and it breaks quietly when the layout
   changes.

The user picked 1.

## How it works today (relevant parts)

- Hooks: `overseer hook <event>` reads Claude Code's hook JSON on stdin and
  sends a `HookEvent` to the daemon. Registered events: SessionStart,
  UserPromptSubmit, Notification, Stop, PostToolUse, and PreToolUse
  (matcher `Bash|Edit|Write|MultiEdit|NotebookEdit`, lock and write
  checks). `HookEvent` carries `session_id`, tool, an 80-char `detail`, the
  notification text and type, but not `transcript_path`.
- Activity: `Working`, `NeedsYou` (with `reason`), `Idle` (with
  `idle_wait`).
- Typing: the GUI sends `Frame::Input { id, data }` to a slot's PTY; the
  daemon's courier pastes relays (`encode_paste`) then Return, checking the
  screen (`relay::screen::input_box`, `shows_working`) first.
- Board: has a selected slot moved by arrow keys; Enter or a click opens it
  (`open_card`, which starts the session view).

## Decisions

**Layout (the user's answer, A).** The board narrows and a pane takes the
right side. The pane follows the board's selected slot: arrow keys and a
single click select a card and show it in the pane. Opening the full
session moves to Enter, a double click, or an "Open session" button in the
pane header (my call: a single click now selects rather than opens). The
pane can be hidden with a key or button, giving the board its full width
back (my call); hidden or shown is remembered across launches. With the
pane hidden, a single click opens the session as today (my call).

**History (my call).** The pane shows the current
transcript's turns, newest at the bottom, scrolled to the bottom when new
text arrives unless the user has scrolled up. User messages and assistant
text appear in full. Each tool call is one line, `Bash: cargo test` (the
tool name and a short summary of its input), with no tool output. Thinking
blocks, system reminders, hook output and sidechain (subagent) entries are
left out. After `/clear` the pane shows the new transcript; earlier ones
are not shown. History is limited to the last N turns sent to the app
(builder's choice of N, at least 50 messages); the session view has the
rest.

**Where history comes from.** The hook adds `transcript_path` (from the
hook's stdin JSON) to `HookEvent`, `#[serde(default)]`. The daemon keeps,
per slot, the transcript path of the slot's current session, and a
`ConversationService` tails that file (reading only new bytes since the
last read, off the async runtime). It parses the JSONL into
`ConvEntry` items and sends them to the app. A transcript line it cannot
parse is skipped, not fatal. A daemon restart re-reads the file from the
start for running slots whose path it knows (stored in the ledger next to
the stretch row, or re-learnt on the next hook; builder's choice).

**Prompts.** A prompt the session is waiting on shows as buttons above the
reply box:

- *Question with options* (`AskUserQuestion`): PreToolUse gains the
  `AskUserQuestion` matcher. The hook sends its questions and options to
  the daemon (no check, never blocks). The pane shows each question with its
  options (toggles for a multi-select question). The pane collects an
  answer for every question in the prompt and a Submit button sends them
  all in one `Answer`, which walks Claude Code's question tabs and submits
  (my call: nothing is typed until the whole prompt is answered, so a
  prompt is never left half-answered). The free-text "Other" is the reply
  box: typing a reply while a question is open sends Esc, then the text
  (the Send rule below).
- *Permission prompt*: Claude Code's `PermissionRequest` hook (registered
  for all tools, matcher `*`) gives the tool and its input; the pane shows
  `Bash: rm -rf target` and the choices Claude Code offers (Yes, Yes and
  don't ask again, No), as buttons. The hook returns no decision: the
  prompt still appears in the terminal, and the button types the choice
  there. Plan approval (`ExitPlanMode`) arrives the same way; the pane shows
  the plan text above its choices (my call). The installed Claude Code
  (2.1.x) has this hook; the minimum supported version is the first one
  with `PermissionRequest`, and the builder verifies an older version does
  not reject the settings (no fallback path, my call).
- *Which hook wins*: for `AskUserQuestion` the PreToolUse question is the
  prompt and its `PermissionRequest`, if any, is ignored.
- *Subagent prompts* (my call): a permission prompt raised inside a
  subagent is still drawn in the main terminal and blocks it, so it shows
  in the pane, labelled `subagent`. Other subagent events stay filtered as
  today.
- *Prompts the pane cannot show* (other dialogs Claude Code draws, MCP
  elicitation): the pane says `waiting on a prompt Overseer can't show`
  with an Open session button, and Send is refused there (my call). This
  holds whenever the slot needs you and no prompt is shown, with or
  without history, as at Claude Code's folder trust question.
- A prompt clears when the next hook event for that slot arrives that ends
  it: PostToolUse with the prompt's `tool_use_id` (not just the same tool
  name, since parallel calls can finish while a dialog is open),
  UserPromptSubmit, Stop, or a new prompt. So a prompt answered in the
  terminal also clears in the pane.

**Sending keys safely.** Every send names the slot and the prompt or turn
it was meant for: `session` and `prompt` (the id of the prompt the pane
showed, or none). The daemon checks, before typing, that the slot still
runs that session and that its open prompt is the same one (or still
none); working or idle does not matter for a reply. If either differs it
refuses and the pane says so (`S2 moved on, not sent`), so Esc is never
typed at a prompt the user has not seen. After typing, the daemon checks the screen as the courier does
(the input box is empty, or the prompt is gone) and reports failure to the
pane if it is not. Keys go through one daemon path, shared with the
courier, so two senders never interleave on one slot.

**Send (the user's answer, stop then send).** The reply box sends on
Enter, Shift+Enter for a new line. If the slot is working, Send types Esc,
waits until the screen shows the empty input box (up to a few seconds;
builder's choice), then pastes the text and Return. If the slot is idle,
it pastes and Returns at once. At an open permission prompt the pane
shows the reply box as `No, and tell it instead`: Send types Esc (which
answers No) and then the text, the same as Claude Code's own "No, and
tell Claude what to do differently" (my call, following the user's stop
then send). A separate Stop button types Esc alone. "Working" here is the
turn in progress: the hooks say working and the transcript has recorded
neither the end of the turn nor an interrupt since the last prompt. While
Claude streams its reply the screen shows no working line, so the screen
alone cannot tell; and no hook follows an Esc, so the transcript's
`[Request interrupted by user]` is what says a stop landed. A stop then
send waits for that too before it pastes. The
text stays in the box until the daemon reports it sent, so a refused send
loses nothing.

**Next waiting slot.** A key (builder's choice, shown in the pane header)
selects the next slot in attention-list order that has `NeedsYou` or an
open prompt, wrapping round. With none waiting it does nothing and says
`nothing waiting`.

**Proto (additive).**

- `HookEvent.transcript_path: Option<String>`, `HookEvent.prompt:
  Option<PromptInfo>`, `#[serde(default)]`; `HookKind::PreToolUse` and
  `HookKind::PermissionRequest` where the existing hook sends them, with a
  `#[serde(other)] Other` on `HookKind` so later kinds do not break older
  daemons.
- `ConvEntry { at: i64, kind: ConvKind }`, `ConvKind = User(String) |
  Assistant(String) | Tool { name: String, summary: String } | Other`
  (`#[serde(other)]` on the tag).
- `Prompt { id: String, kind: Question { questions } | Permission { tool,
  summary, choices } }`.
- `Event::ConversationChanged { slot, entries_from: usize, entries:
  Vec<ConvEntry>, prompt: Option<Prompt> }` (appends from `entries_from`,
  so a long history is not resent on every line; a full send on connect
  and after `/clear`).
- `Request::Conversation { slot }` answered by the full view;
  `Request::Answer { slot, prompt: String, answer: Answer }`, where
  `Answer = Permission(choice index) | Questions(Vec<QuestionAnswer>)` and
  `QuestionAnswer = Options(Vec<option index>) | Other(String)`, one per
  question in order,
  `Request::Reply { slot, session: String, prompt: Option<String>, text:
  String }`,
  `Request::Interrupt { slot }`; each answered `Ok` or a refusal message.

**Activity stays as is.** The board's card and attention list keep their
current wording; the pane adds detail, it does not change what "needs you"
means.

## Constraints

- Public repo: invented names in tests and docs (DEMO-n, demo-api,
  demo@example.test). No em dashes. No emojis in commits.
- Additive proto changes with a test that old JSON still reads.
- No file reading on the async runtime; transcript tailing reads only new
  bytes.
- The hook stays fast and never blocks Claude Code: the new hook kinds only
  report, they never wait for an answer.
- Never type into a slot without the state check above.

## Acceptance

- Selecting a slot on the board shows its conversation in the pane, with
  the board still fully visible and usable.
- New assistant text appears in the pane while the slot works, without
  reopening it.
- An `AskUserQuestion` prompt shows its options as buttons; clicking one
  answers it in the terminal, and the pane then shows the session moving
  on.
- A permission prompt shows the tool and its choices as buttons; clicking
  answers it in the terminal.
- A prompt answered in the terminal disappears from the pane.
- Typing a reply to an idle slot sends it; typing one to a working slot
  stops the turn first, then sends it. Stop alone interrupts.
- A send to a slot that has moved on is refused and the text is kept.
- The next-waiting key walks the waiting slots in attention-list order.
- After `/clear`, the pane shows the new conversation.
- With the pane hidden, the board looks as it does today.

## Left to the builder

- Pane width (resizable or fixed), styling, markdown handling (plain text
  with code blocks kept monospaced is enough).
- The key bindings for next-waiting, hide pane and Stop.
- How many entries are kept and sent.
- Exactly which keystrokes choose an option in the question and permission
  UIs (verify against the running Claude Code version and pin them in
  tests with recorded screens).
- Whether the transcript path survives a daemon restart through the ledger
  or the next hook.
