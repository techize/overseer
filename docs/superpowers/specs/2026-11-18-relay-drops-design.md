# Dropping stale relays (issues #44, #45, #46)

Date: 2026-11-18
Issues: techize/overseer#44, #45, #46 (follow-ups from #21)

## Intent

Relays that wait too long come back as a stale burst. Today a pending relay can only be dropped by ending its slot's session. The user wants three fixes:

- withdraw a waiting relay by hand (#44);
- stop an old plan kickoff from being delivered after a newer one (#45);
- stop a lock notice from being delivered once it is no longer true (#46).

Every drop stays in the relay history with a reason.

## Design

1. **Relay kind and drop reason.** Add three columns to the `relays` table through a ledger schema migration: `kind`, `ref`, and `drop_reason`, all nullable text.
   - `kind` is one of: `note` (the default, which also covers existing rows), `kickoff`, or `lock`.
   - `ref` holds the plan number for a kickoff and the target for a lock notice.
   - In `overseer-proto`, `Relay` gains `kind: RelayKind` and `drop_reason: Option<String>`, both `#[serde(default)]`.
   - `RelayKind` is `Note`, `Kickoff { plan: i64 }` or `Lock { target: String }`, with `Note` as the default.
   - The GUI and ctl show the reason next to a dropped relay ("dropped: replaced by plan 20's kickoff").
   - The existing session-ended drops get the reason "session ended".
2. **#44, drop by hand.**
   - Add a request to drop one pending relay by id, or every pending relay to a slot. The answer says how many were dropped. A relay that was delivered or already dropped cannot be dropped, and the error says which.
   - ctl: `ctl relay drop <id>` and `ctl relay drop --to S2`.
   - MCP: a new tool, `drop_relay`, taking either `id` or `to`. A session may only drop relays it sent itself (`from` is its own slot); dropping another sender's relay is refused. Add it to `ALLOWED_TOOLS` and the tool-list tests.
   - GUI: a Drop button on each pending relay row, in the session's Relays box and in the board's relay list.
   - Reasons: "dropped by you" from ctl and the GUI, "dropped by S<n>" from MCP.
   - Pending relays show their age (`pending 32m`) in the GUI and in `ctl locks`.
3. **#45, a kickoff replaces an older one.**
   - Plan kickoffs are queued with kind `kickoff` and their plan id.
   - When one is queued for a slot, any still-pending kickoff to that slot from a different plan is dropped in the same transaction, with the reason `replaced by plan N's kickoff`.
   - Other kinds are untouched, and delivered kickoffs are never touched.
4. **#46, re-check lock notices.**
   - The "... You now hold it." notices from `locks/table.rs` are queued with kind `lock` and their target.
   - The courier re-checks them just before it composes a paste. A lock notice whose target is no longer held by the receiving slot is marked dropped with the reason `no longer holds <target>`, and the rest of the batch is delivered.
   - If nothing is left after the re-check, nothing is pasted.

## Acceptance (from the issues)

- A dropped relay is never typed in, and the slot's other pending relays still go.
- A delivered relay cannot be dropped, and the command says so.
- Approving plan 20 while plan 19's kickoff to S2 is still pending leaves only plan 20's kickoff pending for S2. The history shows plan 19's kickoff as dropped, with the reason.
- A lock notice whose target the slot still holds is delivered as today. One whose target was released, or granted to another slot, is dropped with the reason.
- Tests cover each surface (ctl, MCP, GUI). For #46 they cover both cases, plus a notice delivered in the same paste as other relays.

## Non-goals

- Guessing the kind of relays that existed before the migration.
- Changing when the courier delivers.

## Constraints

- The repo is public: use invented names only (DEMO-n, demo.atlassian.net).
- No em dashes in code, comments or docs.
- Commits end with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.
- The PR says "Fixes #44", "Fixes #45" and "Fixes #46".
- Never run the real `security` binary against the user's Keychain in tests.
