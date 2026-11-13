# Phase 4 acceptance

Phase 4 is done only when every row below passes. Scriptable rows were run
against a daemon started from the freshly built release binary under a temp
`OVERSEER_HOME`. Rows that need the GUI, a real Claude session or the real
Claude Code 2.1 prompt are left for Jonathan, with exact steps in the Setup
below. They run against a temp `OVERSEER_HOME` too, so the installed app, its
LaunchAgent and its state are not touched.

The Result column says what was actually observed and how. "For Jonathan"
means nobody has checked it by hand yet. Unit tests cover logic only and do
not replace a hand check where a row asks for one.

Checks marked "verified 2026-11-12" were run on home.local by script: a daemon
started with `overseer daemon` from `target/release/overseer` built at 59b2437
(branch `phase-4/locks`), with `OVERSEER_HOME` set to a temp dir
(`/tmp/ov-accept4-*`, short because a unix socket path is limited to 104
characters), `OVERSEER_CONFIG` pointing at a temp `config.toml` (`jira_command
= []`, `lock_idle_mins = 1`, beads and brain folders in the temp dir) and
`HOME` set to a temp dir with a `.profile` that puts the stub folder on `PATH`
(`SHELL=/bin/sh`). The installed app, its LaunchAgent
(`uk.co.techize.overseer`), its socket, `~/Library/Application Support/overseer`
and `~/.claude` were not touched, and no real `claude` session, GUI, `bd` write
or real Jira call was made. Everything the daemon ran was a stub:

- `claude`: a script named `claude` (the daemon adds its `--settings`,
  `--mcp-config` and `--append-system-prompt-file` flags only to a command with
  that name) that logs its argv and then runs a small program which draws an
  input box (a rule, a `>` line with a dim placeholder, a rule), turns
  bracketed paste on, and logs each paste and each Enter it receives. It can
  show typed text, empty the box, draw a permission dialog without a box, or
  leave bracketed paste off, on a command typed with `ctl send --raw`. It
  imitates the layout the reader expects; it is not Claude Code, so the real
  prompt is still a Jonathan row (I1 to I5).
- `bd`: serves two invented beads (`ov-3` titled `DEMO-12 wire the widget`,
  `ov-4` titled `Standalone chore`) for `bd list --json --limit 0` and logs the
  other calls.
- `kubectl`: prints `staging-eu` for `config current-context`.
- Hook events: piped into `overseer hook <Event>` with `OVERSEER_SLOT` set,
  the way Claude runs them. Slot activity (Working, Idle, needs you) was set
  this way, not by a real session.
- The git repo (`demo-api`, plus a worktree of it) was created under the temp
  dir. No command that a pattern matches was ever executed: the hook only
  asks, and the stubs never run them.

The temp daemons were stopped with `ctl shutdown` and the temp dirs removed
afterwards. The examples in this file (`demo-api`, `uat/root`, `staging-eu`,
`DEMO-12`, `ov-3`) are invented.

## Setup for the hand checks

These steps use a temp state folder and a temp copy of the app bundle. They do
not run `make install`, which would restart the installed daemon.

1. In a Terminal tab (use this tab for every step):

   ```
   export OVERSEER_HOME=/tmp/ov-hand4
   export OVERSEER_CONFIG=$OVERSEER_HOME/config.toml
   export OVERSEER_TARGETS=$OVERSEER_HOME/targets.toml
   export OV=/tmp/ov-hand4/Overseer.app/Contents/MacOS/overseer
   mkdir -p $OVERSEER_HOME/work
   cd ~/overseer && cargo build --release -p overseer
   scripts/bundle.sh target/release/overseer /tmp/ov-hand4/Overseer.app
   ```

   `OVERSEER_HOME` makes the GUI and `ctl` talk to a daemon of their own
   (`/tmp/ov-hand4/overseer.sock`) and keeps the installed LaunchAgent out of
   it. `targets.toml` does not exist, so the compiled-in patterns are used.
2. Config and a demo repo with a local bare remote (a push succeeds and
   touches nothing real):

   ```
   printf 'beads_dir = "/tmp/ov-hand4/work"\nbrain_dir = "/tmp/ov-hand4/work"\njira_command = []\nlock_idle_mins = 1\n' > $OVERSEER_HOME/config.toml
   cd $OVERSEER_HOME/work && git init -q --bare demo-remote.git && git init -q -b main demo-api
   cd demo-api && git commit -q --allow-empty -m init && git remote add origin ../demo-remote.git && git push -q origin main
   ```
3. A helper that sends a `PreToolUse` event the way Claude does (the
   `ARGOCD_SERVER` value becomes the ArgoCD context in a target):

   ```
   export ARGOCD_SERVER=uat
   pre() { python3 -c 'import json,sys;print(json.dumps({"session_id":"s","tool_name":"Bash","tool_use_id":sys.argv[3],"cwd":sys.argv[2],"tool_input":{"command":sys.argv[1]}}))' "$2" "$OVERSEER_HOME/work/demo-api" "tu-$RANDOM" | OVERSEER_SLOT=$1 $OV hook PreToolUse; }
   ```

   Use it as `pre S1 'argocd app set root --revision feat-x'`.
4. Start the app from the same tab so it keeps the variables:
   `$OV`. It starts the temp daemon itself. Start real sessions with
   `$OV ctl start 1 --cwd $OVERSEER_HOME/work/demo-api --label demo-1` (and
   `2`, `3` likewise). The first run in the folder shows Claude's folder trust
   prompt: answer it in the terminal view. Real sessions need your normal
   Claude login, which they find through your normal `HOME`. Answering that
   prompt makes Claude Code record the temp folder's trust in your real
   `~/.claude.json`; that is Claude Code's own write, not Overseer's.
   Overseer writes nothing under `~/.claude` or `~/.claude.json`.
5. The notification row needs the app bundle, which step 1 made. Bring another
   app to the front before triggering it.
6. When done: `$OV ctl shutdown`, quit the app, `rm -rf /tmp/ov-hand4`.

## targets.toml

| ID | Check | Steps | Pass | Result |
|----|-------|-------|------|--------|
| T1 | `targets.toml` at the default path is read and adds to the defaults | A `targets.toml` under `$HOME/.config/overseer/` (temp `HOME`) with one pattern `make deploy-<env>` giving `deploy:demo-api/{env}`; start the daemon; send `PreToolUse` for `make deploy-staging-eu` and for `git push origin main` | The new pattern claims `deploy:demo-api/staging-eu`; the compiled-in `git push` pattern still applies; no warning in `daemon.log` | Verified 2026-11-12 at 59b2437: the claim `deploy:demo-api/staging-eu  S5  command` was recorded, `git push origin main` was still matched (denied, since S2 held `repo:demo-api:main`), `targets not loaded` count was 0 |
| T2 | `OVERSEER_TARGETS` wins, and `replace_defaults = true` drops the defaults | `OVERSEER_TARGETS` pointing at a file with `replace_defaults = true` and one pattern `ship <what>` giving `custom:ship/{what}`; restart; probe `ship demo-api`, `git push origin main`, `make deploy-staging-eu` | `ship demo-api` claims `custom:ship/demo-api`; `git push` is not matched (allowed, nothing recorded); the default-path file is ignored (`make deploy-...` not matched) | Verified 2026-11-12: `custom:ship/demo-api  S5  command` recorded; `git push origin main` gave no output and no new wait; `make deploy-staging-eu` was not matched |
| T3 | Broken TOML falls back to the defaults | `OVERSEER_TARGETS` pointing at `[[pattern` followed by junk; restart; probe `git push origin main` and `ship demo-api` | The daemon starts, logs one warning with the parse error, and runs on the defaults only | Verified 2026-11-12: `daemon.log` said `targets not loaded, using the defaults: <path>: TOML parse error at line 1, column 10 ... invalid table header`; `git push origin main` was denied (default pattern live), `ship demo-api` was allowed and unrecorded |
| T4 | A bad regex falls back and says which pattern | A pattern whose `command` is `^ship(\s+` | Defaults only; one warning naming the pattern and the regex error | Verified 2026-11-12: `pattern "bad": bad command regex: regex parse error: ^ship(\s+ ^ error: unclosed group`; `git push` still matched |
| T5 | A target without a known prefix is refused | A pattern with `target = "ship:x"` | Defaults only; one warning naming the pattern and the allowed prefixes | Verified 2026-11-12: `pattern "nope": target "ship:x" must start with one of repo: argo: deploy: tf: kube: custom: and have no spaces or stray braces`; `git push` still matched |
| T6 | A missing `targets.toml` is silent | `OVERSEER_TARGETS` pointing at a path that does not exist; restart; probe `git push origin main` | Defaults; no warning | Verified 2026-11-12: no new `targets not loaded` line; `git push origin main` was matched |

## Default patterns against invented commands

Run in a temp git repo `demo-api` (branch `main`) and a worktree of it
(`demo-api-wt`, branch `wt-branch`). Each command was sent as `PreToolUse`
for S1 from the folder named, then its `PostToolUse`, and `ctl locks` read in
between.

| ID | Check | Steps | Pass | Result |
|----|-------|-------|------|--------|
| P1 | `git push` in its forms (Review Focus 3) | `git push origin main`; `git -C ../demo-api push origin HEAD:main` and `cd ../demo-api && git push`, both from the worktree folder; plain `git push` from the worktree folder | Each is matched; the target is `repo:<repo folder>:<branch>`, and two worktrees of one repo share the repo name | Verified 2026-11-12: `repo:demo-api:main` for the first three, `repo:demo-api:wt-branch` for the worktree push; the claim was released at `PostToolUse` each time |
| P2 | ArgoCD set and sync | `argocd app sync demo-api`, `argocd app set demo-api -p image.tag=1`, with `ARGOCD_SERVER=argo.example` | Both claim `argo:<context>/demo-api` | Verified 2026-11-12: `argo:argo.example/demo-api  S1  command` for both |
| P3 | trigger.dev deploy | `npx trigger.dev@latest deploy --env staging-eu`, `npx trigger.dev deploy` | The env from the command, else `prod` | Verified 2026-11-12: `deploy:trigger.dev/staging-eu` and `deploy:trigger.dev/prod` |
| P4 | Terraform and tofu | `terraform apply -auto-approve`, `tofu destroy`, `FOO=1 terraform apply` | Each claims `tf:<workspace>`; a leading `FOO=1` does not hide it | Verified 2026-11-12: `tf:default` all three times (see L4) |
| P5 | kubectl write verbs | `kubectl apply -f x.yaml -n demo`; `kubectl delete pod x`; `kubectl --context uat/root patch deploy a -p {}` | `kube:<context>/<namespace>`: the namespace flag, else `default`; the context flag, else `kubectl config current-context` | Verified 2026-11-12: `kube:staging-eu/demo`, `kube:staging-eu/default`, `kube:uat/root/default` |
| P6 | Read-only commands are never matched | `git log --oneline`, `kubectl get pods`, `terraform plan` | Allowed, nothing recorded | Verified 2026-11-12: no output, empty `ctl locks` after each |
| P7 | Dry runs are never matched (Review Focus 3) | `git push --dry-run`, `git push -n`, `git push --dry-run origin main`, `argocd app sync demo-api --dry-run`, `kubectl apply --dry-run=client -f x`, `kubectl apply --dry-run=server -f x`, `kubectl apply --dry-run -f x` | Allowed, nothing recorded | Verified 2026-11-12: all seven allowed with an empty `ctl locks` |
| P8 | Write-shaped text that is not a write (Review Focus 3) | `git commit -m "then git push"`; `git commit -m 'then git push' && echo done`; a heredoc whose body is `kubectl apply -f x`; `echo 'argocd app sync x'`; `echo "terraform apply"` | None is matched | Verified 2026-11-12: all five allowed, nothing recorded |
| P9 | Compound lines | `git status; kubectl apply -f a.yaml`; `git push # argocd app sync x`; `git push && argocd app sync demo-api && kubectl apply -f x` | The write in a later segment is found; text after `#` is not; a line with three writes claims three targets | Verified 2026-11-12: `kube:staging-eu/default`; only `repo:demo-api:main` for the comment case; three claims (`argo:...`, `kube:...`, `repo:...`) for the third |

## The hook (Review Focus 1)

Times are for the release binary, run from a script with the hook's own
process start included, 3 to 20 runs each. Each run exited 0 with empty stdout
and empty stderr.

| ID | Check | Steps | Pass | Result |
|----|-------|-------|------|--------|
| H1 | Allow is silent and a free target is auto-claimed | `PreToolUse` for `git push origin main` as S1 with the target free | No output, exit 0; a `command` claim for S1 is in `ctl locks` | Verified 2026-11-12: empty output, `repo:demo-api:main  S1  command` |
| H2 | Deny prints Claude Code's decision JSON, exits 0, and queues the caller | Same command as S2 while S1 holds; then S3 | One JSON line with `hookEventName` `PreToolUse`, `permissionDecision` `deny` and a reason naming the holder and the queue; exit 0; each caller shows as a waiter | Verified 2026-11-12: reason `Overseer: repo:demo-api:main is held by S1 since 12:53. You are in its queue and Overseer will tell you here when you hold it. Do not run this again until then.`; `ctl locks` listed `waiting: S2` then `waiting: S3` |
| H3 | A command's claim is released at its `PostToolUse` by `tool_use_id`, and only that one | S1 sends `git push` (id ta) and `terraform apply` (id tb); then `PostToolUse` for tb | Only the `tf:default` claim goes | Verified 2026-11-12: `repo:demo-api:main` stayed, `tf:default` went |
| H4 | The slot's `Stop` releases what is left | `Stop` for S1 after H3 | The `repo:demo-api:main` claim goes | Verified 2026-11-12: `ctl locks` empty after the `Stop` |
| H5 | A `PostToolUse` with no `tool_use_id` releases every command claim of the slot | Two command claims for S1, then `PostToolUse` without an id | Both go | Verified 2026-11-12: `ctl locks` empty |
| H6 | No daemon | Temp `OVERSEER_HOME` with no socket file; 10 runs of `PreToolUse git push origin main` | Exit 0, silent, fast (the command is never delayed) | Verified 2026-11-12: min 5 ms, median 7 ms, max 7 ms |
| H7 | A socket file with no daemon behind it | The same, after a daemon socket was bound and closed | Same | Verified 2026-11-12: min 7 ms, median 7 ms, max 7 ms |
| H8 | A hung daemon (accepts, reads, never answers) | A `python3 -I` script listening on a unix socket under a temp `OVERSEER_HOME` that never replies; 3 runs | Allowed after about 1 s, silent, exit 0 | Verified 2026-11-12: 1012, 1014 and 1014 ms |
| H9 | A daemon that answers junk | The same script replying with a bad frame | Allowed at once, silent, exit 0 | Verified 2026-11-12: 7 to 8 ms |
| H10 | A target lookup that is slow inside the daemon (700 ms budget) | `kubectl` stub that sleeps 5 s on `config current-context`; `PreToolUse kubectl apply -f x.yaml` 3 times | Allowed after about 0.7 s, silent; no claim and no wait recorded, so a late deny can never leave a stray wait; one warning per run in `daemon.log` | Verified 2026-11-12: 711, 713 and 715 ms; no `kube:` entry in `ctl locks`; `targets not resolved within 700ms; allowing "kubectl apply -f x.yaml"` three times |
| H11 | A command no pattern matches costs one round trip | 20 runs of `git log --oneline` and 20 of `terraform plan` against the live daemon | Same speed as a no-op hook | Verified 2026-11-12: median 7 ms, max 8 ms for both |
| H12 | Odd input is silent | No `OVERSEER_SLOT`; non-JSON stdin; an `Edit` tool event; `OVERSEER_SLOT=S9` with empty stdin | Exit 0, no output, nothing recorded | Verified 2026-11-12: four runs, exit 0 each, `ctl locks` unchanged |
| H13 | Hooks and MCP are wired per slot only | Start a slot with the stub `claude`; read `slots/S1/settings.json`, `slots/S1/mcp.json` and the argv | `settings.json` has a `PreToolUse` entry with matcher `Bash` and `timeout` 5, and `permissions.allow` lists the four `mcp__overseer__*` tools; `mcp.json` runs `overseer mcp` with `OVERSEER_SLOT` set; the argv has `--settings`, `--mcp-config` and `--append-system-prompt-file` pointing at the slot folder; nothing written under the temp `HOME`'s `.claude` | Verified 2026-11-12: all as described (`PreToolUse`, matcher `Bash`, timeout 5; env `OVERSEER_SLOT: S1`; the three flags); the temp `HOME` held only `.profile` and `.config/overseer` |

## Pins

| ID | Check | Steps | Pass | Result |
|----|-------|-------|------|--------|
| N1 | A pin is recorded and survives its command | S2 sends `argocd app set demo-api --revision feat-x`, then its `PostToolUse`, then a `Stop` | A `pin` claim with revision `feat-x`; neither the `PostToolUse` nor the `Stop` releases it | Verified 2026-11-12: `argo:argo.example/demo-api  S2  pin` after both; `--json` showed `pin_revision: feat-x` |
| N2 | Others queue behind a pin | S3 sends `argocd app sync demo-api` | Denied; S3 waits | Verified 2026-11-12: deny reason naming S2; `waiting: S3` |
| N3 | Only an unpin ends it | S2 re-pins to `v2` (stays pinned), then runs `argocd app set demo-api --revision main`, then its `PostToolUse` | The re-pin keeps the pin; the unpin turns it into a command claim until its `PostToolUse`, which releases it and hands it to S3 | Verified 2026-11-12: after `main` the claim read `command`, after `PostToolUse` S3 held a `grant` (see L2) |

## Queue, handoff and release

| ID | Check | Steps | Pass | Result |
|----|-------|-------|------|--------|
| F1 | Callers queue in order | S1 holds `repo:demo-api:main`; S2 then S3 send `git push` | `ctl locks` lists the waiters S2, S3 in that order | Verified 2026-11-12 (see H2) |
| F2 | A release grants the first waiter and tells it, under the delivery rules | `PostToolUse` for S1's push while S2 is Working, then S2 `Stop` | S2 holds a `grant`; the relay `S1 has released repo:demo-api:main. You now hold it.` is pending (`session is working`) and is typed into S2 once it is idle | Verified 2026-11-12: `ctl locks` showed the pending relay with that reason; after the `Stop` the stub logged one paste and one Enter with exactly that text |
| F3 | A grant becomes a command claim when the slot retries | S2 sends `git push` again, then its `PostToolUse` | The claim reads `command`; the `PostToolUse` hands the target to S3 with a relay | Verified 2026-11-12 |
| F4 | An unused grant is released at the first `Stop` after its relay was delivered | S3's grant, relay delivered, then `UserPromptSubmit` and `Stop` | The grant goes and nobody holds the target | Verified 2026-11-12: `ctl locks` showed no claim |
| F5 | `ctl claim` and `ctl release <slot> <target>` | `claim 1 custom:demo-lane "trying a thing"`; `claim 2` and `claim 3` on it; `claim 1 notatarget`; `release 2 custom:demo-lane`; `release 1 custom:demo-lane` | S1 holds (exit 0); S2 and S3 are told their queue position (exit 2); the bad target is refused (exit 1) with the allowed prefixes; a waiter releasing leaves the queue; the holder releasing hands to S3 with a relay | Verified 2026-11-12: `You hold custom:demo-lane.`; `You are number 1` and `number 2 in the queue`, exit 2 each; `"notatarget" is not a target: it needs a repo:, argo:, deploy:, tf:, kube: or custom: prefix and no spaces`, exit 1; `You are no longer queued for custom:demo-lane.`; `Released custom:demo-lane. S3 holds it now.` |
| F6 | Force release | `ctl force-release custom:demo-lane` while S3 holds it; again when it is free | Released, and the holder is told (`Jonathan released custom:demo-lane, which you held.`); the second is an error | Verified 2026-11-12: `Released custom:demo-lane.`, the relay reached S3 after its 10 s cooldown (D9); the second call printed `overseer: custom:demo-lane is not held`, exit 1. Force release with a waiter printed `Released custom:demo-lane. S2 holds it now.` |
| F7 | Releasing a work item releases the targets claimed for it | `ctl assign 3 ov-4`; S3 claims `argo:argo.example/demo-api` and `kube:staging-eu/demo`; S1 queues on the first; `ctl release ov-4` | Both claims show `for ov-4` while held; both are released by the release; S1 is granted the first with a relay | Verified 2026-11-12: `ctl locks --json` showed `item: ov-4` on both; after `ctl release ov-4` the argo target was S1's and the kube target was gone |
| F8 | The item a claim is for | S1 owns only `ov-3`; its hook claim and an MCP claim naming `item: ov-3` | `ctl locks` says `for ov-3`; with several owned items and no `item` argument, none | Verified 2026-11-12: `repo:demo-api:main  S1  command  ... for ov-3` and `custom:demo-lane  S6  since 12:58  for ov-3` over MCP. The several-items case is unit tested only |
| F9 | A long-held lock is flagged, never dropped | `lock_idle_mins = 1`; leave a grant and a claim alone for over a minute | The line gains `held long`; the claim is still there | Verified 2026-11-12: `repo:demo-api:main  S2  grant  since 12:56  held long` at 12:57 |

## Holder and waiter exit (Review Focus 5)

| ID | Check | Steps | Pass | Result |
|----|-------|-------|------|--------|
| X1 | A holder that exits hands on its command claims at once | S1 holds `repo:demo-api:main` by a command, S2 waits; `ctl stop 1` | The claim is released and granted to S2 with a relay | Verified 2026-11-12: `repo:demo-api:main  S2  grant` and the relay `S1 has released repo:demo-api:main. You now hold it.` within a second |
| X2 | Its explicit claims and pins stay | S1 holds `custom:demo-lane` (`ctl claim`) and a pin on `argo:argo.example/demo-api`, with waiters S2 and S3; `ctl stop 1` | Both stay, with the waiters; nothing is handed on | Verified 2026-11-12: unchanged `ctl locks` after the stop |
| X3 | A waiter that exits leaves the queue | `ctl stop 3` while S3 waits on the pin | S3 is no longer listed | Verified 2026-11-12: `waiting: S3` gone, S3 `exited` |
| X4 | Jonathan's force release hands a stuck claim on | `ctl force-release` of the two stuck targets | The target with a waiter goes to it | Verified 2026-11-12: `Released custom:demo-lane. S2 holds it now.` |

## Restart (Review Focus 4)

| ID | Check | Steps | Pass | Result |
|----|-------|-------|------|--------|
| S1 | Claims, waits and pending relays survive a restart | With a claim, a grant, a waiter and nine pending relays, save `ctl locks --json`, `ctl shutdown`, start the daemon, save it again straight away, diff | The same claims, queue order and relays; slots that were running are `resumable` | Verified 2026-11-12: the JSON differed only in the `held` reasons of five relays (`session is working` and `bracketed paste is off` became `slot is not running`, which is the runtime reason and not stored); `ctl status` showed S2, S4, S5 and S6 `resumable` |
| S2 | Pending relays deliver once the slot is resumed and idle | `ctl resume 4` then S4 `Stop`; `ctl resume 2` then S2 `Stop` | S4 gets `survives the restart`; S2 gets every pending relay in one paste | Verified 2026-11-12: S4's stub logged the paste and Enter; S2's logged one paste of five paragraphs separated by blank lines (the four lock relays and `Message from S0 via Overseer: mcp reply test`, with the answer hint) |

## Relay delivery (Review Focus 2)

| ID | Check | Steps | Pass | Result |
|----|-------|-------|------|--------|
| D1 | Idle and the box empty: delivered | S2 `Stop`, relay pending | Pasted once, then Enter; the relay turns delivered | Verified 2026-11-12: the stub logged `PASTE: ...` then `SUBMIT: ...`; `daemon.log` said `1 relay(s) delivered to S2` |
| D2 | While the session works: waits | S4 `UserPromptSubmit`, `ctl send 4 hello` | Pending with the reason | Verified 2026-11-12: `relay  S4  pending (session is working)  hello` |
| D3 | At a permission prompt: never typed | `Notification` with `notification_type` `permission_prompt` | Pending with a reason; nothing reaches the terminal | Verified 2026-11-12: `pending (waiting at a prompt for Jonathan)`; the stub log was empty |
| D4 | Activity says idle but the screen shows a dialog, not the box | `ctl send 4 ':dialog' --raw` (the stub draws a dialog with no input box), then `Stop` | Waits: the box cannot be found | Verified 2026-11-12: `pending (input box not found)`; nothing typed |
| D5 | Jonathan has text in the box: waits, then goes | `:typed half-written note`, then `:empty` | Pending `input box has text`; delivered after the box is emptied, and the typed text was never overwritten | Verified 2026-11-12: `pending (input box has text)`; after `:empty` the stub logged the paste and Enter |
| D6 | Several pending relays go in one paste | Two relays to a Working S4, then `Stop` | One paste, the texts separated by a blank line, both marked delivered | Verified 2026-11-12: `PASTE: Message from Jonathan via Overseer: alpha note\n\nMessage from Jonathan via Overseer: beta note` |
| D7 | Without bracketed paste a multi-line relay waits (a Task 6 follow-up) | S5 with the stub's bracketed paste off: a two-line relay, then a one-line relay | The two-line relay waits `bracketed paste is off`; a one-line relay on its own is typed with Enter (S6) | Verified 2026-11-12: S5 `pending (bracketed paste is off)` (the later one-line relay shares the same paste so waits with it); S6 logged `SUBMIT: Message from Jonathan via Overseer: single line` with no paste markers |
| D8 | Control characters are stripped | `ctl send 2 "ctrl<ESC>[31mred"` | No Escape reaches the session | Verified 2026-11-12: the stub's log had `ctrl[31mred` and no ESC byte (`grep -c` of ESC was 0) |
| D9 | One idle state never gets two pastes | A second relay for S3 right after a delivery | Waits until the activity changes or 10 s pass | Verified 2026-11-12: the force-release relay for S3 stayed pending for under 10 s and then went (see F2 in Findings for how it reads meanwhile) |
| D10 | Phase 3 notices are delivered | S2 owns `ov-4`; `ctl assign 4 ov-4 --move` while S2 is Working; S2 `Stop` | The notice also queues a relay; it is typed as `Overseer: ov-4 was moved to S4`; `ctl items` still lists the notice | Verified 2026-11-12: `SUBMIT: Overseer: ov-4 was moved to S4` and `notice  S2  ov-4 was moved to S4` |
| D11 | The text says who it is from | Relays from Jonathan, from S0, and from Overseer | `Message from Jonathan via Overseer: ...`; `Message from S0 via Overseer: ...` plus `(You can answer once with notify_session.)` for a worker; `Overseer: ...`, except release texts, which stand alone | Verified 2026-11-12 in the pastes above |

## The reply rule

| ID | Check | Steps | Pass | Result |
|----|-------|-------|------|--------|
| Y1 | A worker cannot start a conversation | As S2: `ctl send 0 ...`, `ctl send 3 ...`, `ctl send 2 ...` | Refused (exit 1), naming the rule; sending to yourself is refused | Verified 2026-11-12: `S2 can message S0 only to answer a message from S0.`, the same for S3, `you cannot message yourself` |
| Y2 | S0 may send; the worker may answer once | As S0 `ctl send 2 ...`, delivered; then as S2 `ctl send 0 ...` twice | The first answer is queued for S0, the second refused; sending to S3 stays refused | Verified 2026-11-12 (`Queued for S0.`, then the refusal), by `ctl` and by the MCP `notify_session` tool |
| Y3 | Bad input | Empty text; 4001 characters; slot 9 | Refused with a clear message | Verified 2026-11-12: `the message is empty`, `the message is over 4000 characters`, `invalid slot id 9` |

## ctl

| ID | Check | Steps | Pass | Result |
|----|-------|-------|------|--------|
| C1 | `ctl locks` | With claims, waits, pending relays and `--json` | One line per claim with kind, time, item or reason and `held long`; waiters beneath; pending relays with their reason; `--json` is a LockView | Verified 2026-11-12: the outputs quoted throughout this file; `--json` carried `flag_after_secs: 60` |
| C2 | Exit codes | `claim` held (2), `claim` bad target (1), `release` not held (1), `force-release` not held (1), `send` queued (0) | As listed | Verified 2026-11-12 |
| C3 | `ctl release <key>` keeps its phase 3 meaning | `ctl release ov-4` | Frees the work item (and its claims, F7) | Verified 2026-11-12: `bd` was called with `update ov-4 --remove-label overseer:S3` |
| C4 | `ctl send --raw` types at once | `ctl send 4 ':empty' --raw` and the other stub commands | Typed with Enter, not queued | Verified 2026-11-12: the stub acted at once each time |

## MCP, scripted (over stdio against the temp daemon)

| ID | Check | Steps | Pass | Result |
|----|-------|-------|------|--------|
| M1 | Handshake and tool list | Pipe `initialize`, `notifications/initialized`, `ping`, `tools/list` to `overseer mcp` with `OVERSEER_SLOT=S6` | Server `overseer`, protocol version echoed, `tools` capability, four tools: `claim_target`, `release_target`, `notify_session`, `list_targets` | Verified 2026-11-12: `serverInfo overseer 0.1.0`, `2025-06-18`, `['tools']`, the four names; `claim_target` requires `target` and `reason` (optional `item`) |
| M2 | Claim, queue, list, release | `list_targets`; `claim_target`; `claim_target` again; `list_targets`; `release_target` twice; a bad target | Granted or queued with the position; "already hold"; a queued caller can release its place; errors come back as tool errors | Verified 2026-11-12: `You hold custom:demo-lane.`; `You already hold custom:demo-lane.`; queue position text when held by S4; `You are no longer queued for ...` then `is not held by S6` (`isError` true); a bad target gave the prefix message (`isError` true) |
| M3 | `notify_session` follows the reply rule | As S6, a message to S4 and to S0; as S2 after S0's relay was delivered, an answer and a second answer | Refused for a worker with no relay to answer; the first answer queued, the second refused | Verified 2026-11-12 |
| M4 | No slot, no tools | `overseer mcp` with `OVERSEER_SLOT` unset | Tool calls say so | Verified 2026-11-12: `This session is not in an Overseer slot.` (`isError` true) |
| M5 | Protocol errors | An unknown tool; `claim_target` without `reason` | JSON-RPC error `-32602`, the server keeps running and exits 0 when stdin closes | Verified 2026-11-12: `unknown tool \`nosuch\``, `missing \`reason\``, exit 0, empty stderr |

## MCP from a real claude session (hand)

Setup steps 1 to 4, then start S1: `$OV ctl start 1 --cwd
$OVERSEER_HOME/work/demo-api --label demo-1`.

| ID | Check | Steps | Pass | Result |
|----|-------|-------|------|--------|
| MR1 | The tools are seen | In S1's terminal run `/mcp` | `overseer` is listed as connected, with `claim_target`, `release_target`, `notify_session` and `list_targets` | Pass, 2026-11-12 at 11a20aa: `/mcp` listed `overseer` under Built-in MCPs as connected with 4 tools; its tool list read `claim_target`, `release_target`, `notify_session`, `list_targets` |
| MR2 | `OVERSEER_SLOT` and the socket reach the server, and no permission prompt appears | Ask S1: `Use the overseer list_targets tool, then claim_target custom:demo-lane with reason "hand check", then list_targets again, then release_target custom:demo-lane.` Meanwhile in another tab `$OV ctl locks` after the claim | Claude runs all four with no permission prompt; `ctl locks` shows `custom:demo-lane  S1  claim` with `for hand check`, so the server knew it was S1 and reached the temp daemon; the last call releases it | Pass: Claude called `list_targets` (`No targets are held.`), `claim_target` (`You hold custom:demo-lane.`), `list_targets` (`custom:demo-lane  S1  since 17:24  for hand check`) and `release_target` (`Released custom:demo-lane.`) with no prompt; `ctl locks` during the run read `custom:demo-lane  S1  claim  since 17:24  for hand check` and was empty after. The slot settings allow the four tools; Jonathan's user settings also default to auto mode |
| MR3 | `notify_session` from a real session | Start a session in S0 (a slot with no session refuses messages since a55f1a2), then `OVERSEER_SLOT=S0 $OV ctl send 1 "Reply to me with notify_session: ok"`; wait for it to be typed in | Claude calls `notify_session` for S0 with no prompt; `$OV ctl locks` shows a relay for S0, pending while S0 works, and S0 gets `Message from S1 via Overseer: ok` once; a second `notify_session` for S0 is refused | Pass, with the Pass text out of date: with S0 empty Claude called `notify_session` with no prompt and got `S0 has no session to message.` (the a55f1a2 rule, see E3 and F5), so nothing was queued. With a session started in S0 the same send gave `relay  S0  pending (session is working)`, then `waiting at a prompt for Jonathan`, and S0 got `Message from S1 via Overseer: ok` once. Asked twice for a second call, Claude refused on its own and never called the tool, so the server's refusal was not exercised by hand (the reply rule rows cover it) |

## The input-box reader against Claude Code 2.1 (hand)

Setup steps 1 to 4, S1 running and sitting at its prompt. Read the reason a
relay waits with `$OV ctl locks` (the line `relay  S1  pending (<reason>)`).
Send with `$OV ctl send 1 "reply with the word ok"`.

| ID | Check | Steps | Pass | Result |
|----|-------|-------|------|--------|
| I1 | Empty box, placeholder showing | Fresh prompt, nothing typed | The relay is typed in as one pasted message and answered once within a few seconds; `ctl locks` ends with no pending relay | Pass: with the dim suggestion showing, the relay was typed in as one message within a second and answered `ok` once (transcript); `ctl locks` then showed no relay |
| I2 | Text typed by you | Type `half a thought` in S1's box without pressing Enter, then send | The relay stays pending with `input box has text`, and your text is untouched. Clear the box (Ctrl-U): the relay goes within a few seconds | Pass: `pending (input box has text)` for 16 s with `half a thought` untouched; after Ctrl-U it was typed in within 5 s and answered once |
| I3 | Several lines in the box | Type two lines (Shift-Enter between them) without submitting, then send | Pending with `input box has text`; goes after the box is cleared | Pass: with `one` and `two` on two lines, `pending (input box has text)`; typed in and answered once within 5 s of clearing the box |
| I4 | A permission dialog | Ask S1 `run touch /tmp/ov-hand4/x with Bash`; while the permission dialog shows, send | Nothing is typed into the dialog; the relay is pending (`waiting at a prompt for Jonathan` or `input box not found`). Answer the dialog: the relay is typed in once the turn ends and the box is empty | Fail (part): during the permission dialog nothing was typed and the relay read `pending (waiting at a prompt for Jonathan)`. Answering No (option 4) interrupts the turn, which fires no `Stop` hook: the slot stayed `Needs you`, `Claude needs your permission` and the relay stayed pending for over a minute. It went only after the next prompt (`say hi`) ended, then was typed and answered once. See F6. Recheck 2026-11-13 after the F6 fix: Pass. With No, the relay read `pending (waiting at a prompt for Jonathan)` during the dialog and was typed about 2 s after No, answered once. With Yes on a 25 s foreground command, it stayed pending through the command and went once the turn ended |
| I5 | A multi-line relay arrives as one message | `$OV ctl send 1 "$(printf 'line one\nline two')"` at an idle empty prompt | Claude shows one pasted message and answers it once (bracketed paste is on) | Pass: Claude showed one message `Message from Jonathan via Overseer: line one` / `line two` and answered it once (transcript) |

## Board, panel and keys (GUI, checked by hand)

Setup steps 1 to 4 with the app open. Start S1, S2 and S3 as in step 4.

| ID | Check | Steps | Pass | Result |
|----|-------|-------|------|--------|
| U1 | Lock pills on cards | `$OV ctl claim 1 custom:demo-lane`; `pre S1 'argocd app set root --revision feat-x'`; wait over a minute | S1's card shows both targets as pills within a second, the pin (`argo:uat/root`) first; after `lock_idle_mins` (1) the claim pill is marked as held long | Pass: S1's card showed `argo:uat/root` then `custom:demo-lane` within a second; after a minute both read `· held long` |
| U2 | Blocked card | `$OV ctl claim 2 custom:demo-lane` while S2 is not Working | S2's card turns red with `blocked by lock` and `waiting on custom:demo-lane`; when S2 is Working its tone stays Working and the waiting line still shows | Pass: S2's card turned red with `blocked by lock` and `waiting on custom:demo-lane`; while S2 ran a 20 s sleep it read `Working` in green with the waiting line still shown |
| U3 | Pending count | Type `half` in S3's box without Enter, `$OV ctl send 3 hello` | S3's card says `1 relay pending`; clearing the box makes it go and the count disappear | Pass: S3's card read `1 relay pending`; after Ctrl-U the relay went and the count disappeared |
| U4 | Relay strip | After U3, and a few more `ctl send` calls | The strip at the bottom of the board lists the five newest relays, newest first, as `HH:MM told S3: hello`, `HH:MM for S3, pending: hello` or `HH:MM for S3, dropped: session ended: hello`; it is hidden when there are none | Pass: the strip listed five, newest first, e.g. `17:43 for S2, dropped: session ended: drop me`, `17:42 told S3: only after the sleep`, and earlier `17:32 for S3, pending: hello`; it was not shown before the first relay |
| U5 | Holds box | Click S1's card, open its panel | A Holds box lists each hold with its kind (`pin`, `claim`, `command`, `grant`), time, what it is for, `held long` when flagged, and how many are queued behind it; waits show beneath; `Nothing held` for a slot with none; a Release button beside each hold | Pass with gaps: Holds listed `argo:uat/root  pin since 17:32  held long` and `custom:demo-lane  claim since ...  held long  waiting: S2`, each with Release; `Nothing held` for S3 before its pin. Gaps: the reason (`for lane test`) and the pin revision are not shown, and the claim's time was cut to `claim sinc` when `held long` and `waiting:` both showed. See F7. Recheck 2026-11-13 after the F7 fix: Pass. `argo:uat/root  pin since 09:27` over `held long  at feat-x`, and `custom:demo-lane  claim since 09:27` over `held long  for lane test  waiting: S2`, each with Release; nothing cut |
| U6 | Release, with its key | In S1's panel press Release (or Cmd+Shift+R) | An inline `Release S1's <target>? Enter / Esc` row; Up and Down pick among the holds; Enter force-releases (S2 is granted `custom:demo-lane`, its card un-blocks), but only while S1 still holds it (E4); Esc cancels; plain `r` and `m` still go to the terminal | Pass: Cmd+Shift+R gave `Release S1's argo:uat/root? Enter / Esc`, Down moved it to `custom:demo-lane`, Esc cancelled, plain `r` and `m` reached the terminal. With the pin moved to S3 behind the prompt, Enter said `argo:uat/root is now held by S3; nothing released.` The Release button then Enter gave `Released custom:demo-lane. S2 holds it now.` and S2's card showed the pill, no longer blocked |
| U7 | Claim entry | Press `Claim...` in S2's panel, type `custom:other lane test`, Enter; then an empty Enter and `bogus` | The claim is made with the reason `lane test`; an empty or bad entry shows its error and sends nothing | Pass: `custom:other lane test` gave `You hold custom:other.` and `ctl locks` `custom:other  S2  claim  since 17:35  for lane test`; an empty Enter showed `Enter a target, then an optional reason`, `bogus` showed `"bogus" is not a target: it needs a repo:, argo:, deploy:, tf:, kube: or custom: prefix and no spaces`; nothing else was claimed |
| U8 | Relays box and message | Open S3's panel; Message... (or Cmd+Shift+M), type `hello again`, Enter | The Relays box lists S3's relays with state (pending reasons or delivered); the message goes through the delivery rules (typed in only when S3 is idle with an empty box); an empty message shows `Type a message first` | Pass: Cmd+Shift+M with nothing typed showed `Type a message first`; `hello again` was typed in at the idle empty prompt and answered; the Relays box listed `17:35 from you: hello again` and `17:33 from you: hello`. Pending reasons showed in S0's box (`from S1: ok (pending: waiting at a prompt for Jonathan)`) |
| U9 | Denied-write notification | Bring another app to the front; with S1 holding `repo:demo-api:main` (`pre S1 'git push origin main'`), run `pre S2 'git push origin main'` | A banner `S2 blocked: repo:demo-api:main is held by S1`; one per new wait (a repeat does not post again); clicking it opens S2's session. This needs the bundle from step 1; under `cargo run` there is no bundle and no banner | Pass: triggered at 17:36 with the Claude app in front; the hook denied S2 (`repo:demo-api:main is held by S1 since 17:36`) and a repeat added no second wait. Jonathan saw the banner by eye (confirmed 2026-11-13) |
| U10 | Everything survives a daemon restart | `$OV ctl shutdown`, then reopen the window | The same pills, waits and strip show at once; slots that were running are shown as resumable | Pass: after `ctl shutdown` the window said `Lost the connection to the daemon. Quit and reopen Overseer to reconnect.`; reopened, it showed at once the same pills (S1 `repo:demo-api:main`, S2 `custom:demo-lane` and `custom:other`, S3 `argo:uat/root`, all held long), S2's `waiting on repo:demo-api:main` and the same strip, with S1 to S3 `Resumable` and a Resume button |

## Session end and holder checks (fix wave)

Run 2026-11-12 by script against a temp daemon from `target/release/overseer`
built at 183db70, the same way as the rows above (`OVERSEER_HOME=/tmp/ov-fix4`,
temp `config.toml`, temp `HOME` with a `.profile` putting the stub folder on
`PATH`, `SHELL=/bin/sh`). The stub `claude` logged its argv, created an empty
transcript under a temp `CLAUDE_CONFIG_DIR` for its `--session-id` (so a
restart offers to resume it) and slept. The holder-checked request was sent
over the socket by a `python3 -I` script, since `ctl force-release` sends no
holder. The daemon was stopped with `ctl shutdown` and `/tmp/ov-fix4` removed.

| ID | Check | Steps | Pass | Result |
|----|-------|-------|------|--------|
| E1 | A fresh start ends the old session first (F1) | `ctl start 1`; `ctl claim 2 custom:lane`; `ctl claim 1 custom:lane` (waits); `ctl send 1 "hello e1"`; `ctl stop 1 && ctl start 1` back to back | S1's wait is gone; the relay is listed as dropped and never typed into the new session | Verified 2026-11-12 at 183db70: `ctl locks` read `custom:lane  S2  claim ...` with no waiter and `relay  S1  dropped: session ended  hello e1`; S1 `running` with the new label |
| E2 | Declining a resumable slot hands its grant on | `ctl start 1` and `3`; `ctl claim 2 tf:default`; `PreToolUse terraform apply -auto-approve` for S3 then S1 (both denied, both wait); `ctl release 2 tf:default` (S3 is granted); `ctl shutdown`, start the daemon; `ctl stop 3` | After the restart S1 and S3 are `resumable` and S3 still holds the grant with its relay pending; the decline hands `tf:default` to S1 with a relay and retires S3's relay | Verified 2026-11-12: before the decline `tf:default  S3  grant`, `waiting: S1`, `relay  S3  pending (slot is not running)`; after it `tf:default  S1  grant`, `relay  S1  pending (slot is not running)  S3 has released tf:default. You now hold it.` and `relay  S3  dropped: session ended  S2 has released tf:default. You now hold it.`; S3 `empty`. A run of the same steps with `ctl claim` waits gave S3 an explicit `claim`, which stays after the decline, as X2 says |
| E3 | A message to a slot with no session is refused (F5) | `ctl send 5 "hi there"` with S5 empty | Exit 1 with the reason; nothing queued | Verified 2026-11-12: `overseer: S5 has no session to message.`, exit 1; `ctl locks` listed no relay for S5 |
| E4 | A force release names the holder Jonathan was shown | S3 holds `custom:lane` (`ctl claim`); `ForceRelease` with `holder` 2, then with `holder` 3 | The first releases nothing and names the real holder; the second releases; the notice for S3, which has no session, is queued already dropped | Verified 2026-11-12: `custom:lane is now held by S3; nothing released.` (an `Error` response) with the claim unchanged; then `Released custom:lane.` and `relay  S3  dropped: session ended  Jonathan released custom:lane, which you held.` |

## Findings from the 2026-11-12 run

No scriptable check failed. Observations:

- **F1 (note): relays queued for an exited session are typed into the next
  session started in that slot.** S1 exited holding nothing but with three
  relays pending (`S3 has released argo:... You now hold it.`, two `Jonathan
  released ...`). A fresh `ctl start 1` later got all three in one paste at its
  first `Stop`. This follows the rule that pending relays survive restarts
  (Review Focus 4, delivered on resume), but a fresh start is not a resume, and
  the texts can describe holds the new session never had. Repro: `ctl start 1`,
  queue a relay for S1, `ctl stop 1`, `ctl start 1`, send S1 a `Stop` event.
  Fixed in a55f1a2: when a session ends (exit, stop, decline of a resumable
  slot, a fresh start, or a restart that cannot resume it) its waits, grants
  and command claims go and its pending relays are retired as `dropped:
  session ended`. They are never typed. See E1 and E2.
- **F2 (note): a relay waiting out the 10 s cooldown reads `pending
  (pending)` in `ctl locks`.** The courier has not set a reason (it skips the
  slot), so the text shows the word twice. It goes once the cooldown ends.
  Fixed in a55f1a2: `ctl locks` prints `pending` alone when there is no reason,
  and during the cooldown the reason reads `just delivered, waiting for the
  session`.
- **F3 (note): `ctl locks` does not show a pin's revision,** only `--json`
  does (`pin_revision`). The Holds box may show it; that is a GUI row to read.
  Fixed in a55f1a2: the pin line ends `  at <revision>`.
- **F4 (note): `claim_target` without `reason` is a protocol error
  (`-32602 missing reason`) rather than a tool error,** while `ctl claim` and
  the GUI treat the reason as optional. A session that leaves it out sees a
  failed tool call, not text it can act on. Fixed after sign-off: `reason`
  is optional in the schema and the call.
- **F5 (note): a relay for a slot that is empty or exited stays pending
  forever** (`slot is not running`), including the replies S2 queued for S0 when
  no S0 session exists. It is shown, not lost, and is listed by `ctl locks`.
  Fixed in a55f1a2: `ctl send` and `notify_session` to a slot with no session
  are refused (`S5 has no session to message.`, E3), and an Overseer notice
  for a holder with no session is queued already dropped. A relay still waits
  while its slot is resumable.
- **Environment note:** the temp state dir was under `/tmp`, not the session
  scratch directory, because the unix socket path would have been longer than
  the 104 character limit.

## Findings from the hand run (2026-11-12)

MR1 to MR3, I1 to I5 and U1 to U10 were run by a Claude session for
Jonathan, against `/tmp/ov-hand4/Overseer.app` bundled at 11a20aa, with
Claude Code 2.1.259 sessions in S1 to S3 (S0 for part of MR3). Jonathan
answered the folder trust prompt. Text was typed into sessions with `ctl send
--raw`, and the window was driven by computer use for the U rows' clicks and
keys. "Answered once" was read from the session transcripts. The installed
app, its LaunchAgent and its state were not touched. Failures are listed for
Jonathan; nothing was fixed.

- **F6 (fail, I4): a relay waits forever after a permission dialog is
  answered No.** Choosing No (or Esc) interrupts the turn, and Claude Code
  sends no `Stop` hook for an interrupt, so the slot keeps `Needs you`,
  `Claude needs your permission` and every relay for it stays `pending
  (waiting at a prompt for Jonathan)` until the next prompt ends. Answering
  Yes is not affected. Repro: I4 with No. Fixed after sign-off: the
  courier now reads the screen of a slot waiting at a prompt, and once two
  looks a second apart show the empty input box and no working line, the
  relay goes. The working line is read in both forms: older Claude Code's
  `esc to interrupt`, and 2.1's `✻ Churning… (4s · ↓ 323 tokens)`, which
  has no hint. I4 passed on recheck with No and Yes.
- **F7 (gap, U5): the Holds box leaves out the reason and the pin revision,**
  which `ctl locks` shows (`for lane test`, `at feat-x`), and the `since`
  text is cut short (`claim sinc`) when `held long` and `waiting:` share the
  row. `render_holds` in `crates/overseer-gui/src/session.rs` draws only
  target, kind, since, held long and waiters. Fixed after sign-off: each
  hold has two lines, the target and `<kind> since <time>` first, never
  cut, then `held long`, `for <item or reason>`, `at <revision>` and
  `waiting:`. U5 passed on recheck.
- **F8 (note): `ctl send --raw` does not submit text that wraps.** A short
  text (`/mcp`, `say hi`) is submitted, but the MR2 request and a two-line
  text arrived with the Enter as a newline in the box, so the prompt sat
  unsent until Enter was pressed by hand. Fixed after sign-off: `--raw`
  sends the text, waits 100 ms, then sends Enter on its own, as the courier
  does.
- **F9 (note): the coordinator's role file points at `~/Repos/2ndBrain` even
  under a temp `OVERSEER_HOME`.** When S0 was started for MR3 it began a
  start-of-day round and read the real journal and `bd ready --json` (reads
  only; S0 was stopped afterwards). A hand run that starts S0 touches real
  data unless the role's folder follows the temp config. Fixed after
  sign-off: the coordinator role says `{brain}` and `{beads}`, filled from
  `brain_dir` and `beads_dir` in `config.toml` (the hand run's config sets
  both to `/tmp/ov-hand4/work`). An unedited older `roles/coordinator.md`
  is upgraded in place; an edited one keeps its fixed path.
- **Not a finding:** two relays went in while a `sleep 30` was asked for.
  The transcripts show Claude Code refused the standalone sleep and ended the
  turn, so the slot was idle.

## Known limitations

- **L1: background commands release early.** A command claim is released at
  its `PostToolUse`, and a slot's command claims go at its `Stop` and exit. A
  push started with `run_in_background` releases when the tool returns, not
  when the push finishes. Two Bash calls of one session on one target at the
  same time share one claim: the first call's `PostToolUse` releases it while
  the second call still runs.
- **L2: a pin only ends on a revision in the pattern's `unpin` list.** The
  default list is `main`, `master`, `HEAD`. The revision in force before the
  pin cannot be learned from the command line inside the hook budget, so
  "set back to the previous target" is "set back to a default-branch revision".
  A pin to restore a non-default revision must be released by hand.
- **L3: the input-box reader has blind spots (Task 6).** It cannot detect a
  placeholder drawn without the dim attribute (reads as typed, so relays wait),
  input that is only spaces or one character under the cursor (reads as empty),
  text hidden in vim normal mode or scrolled out of a box taller than the
  screen, and a layout change in a future Claude Code (reads as not found, so
  relays wait and show why). When in doubt a relay waits.
- **L4: `tf:<workspace>` shares `default`.** Every stack in the `default`
  workspace shares the one lock `tf:default`. A `{dir}` placeholder is available
  to `targets.toml` to split them.
- **L5: the relay courier looks at a slot once a second and leaves it alone for
  10 s after a delivery** unless its activity changes, so a second relay can
  take up to 10 s.
- **L6: a hook that cannot resolve its targets within 700 ms allows the
  command and records nothing.** A slow `kubectl config current-context` means
  an unprotected write; the daemon logs one warning for it.
- **L7: the stub-based rows imitate the input box and hook events.** They show
  the daemon's rules; I1 to I5 and MR1 to MR3 are the only checks against
  Claude Code itself.
- **L8: contexts are read from the environment, not from every flag.**
  `kubectl --kubeconfig <file>` is ignored, so the target uses the current
  context of the default kubeconfig. For ArgoCD, `$ARGOCD_SERVER` outranks the
  `current-context` in the ArgoCD config. Two sessions that reach one cluster
  by different routes can get different target names and so not block each
  other; that fails open, never shut.
- **L9: some command shapes are not resolved.** A shell variable is not
  expanded (`cd "$DIR" && git push` claims `repo:$DIR:<branch>`), and a flag
  before the app name (`argocd app sync --grpc-web demo-api`) hides the write,
  which is allowed and not recorded. Both are pinned in
  `crates/overseer-daemon/tests/fixtures/commands.tsv`.
- **L10: a resumable slot's command claims outlive the command.** The command
  died with the terminal at the restart, but its claim stays until the resumed
  session's `Stop` or until Jonathan declines the resume, and it blocks
  waiters until then.
- **L11: lock ownership and the reply rule are cooperative.** They bind the
  hook, the MCP tools and the GUI. A worker's own Bash can still run
  `overseer ctl send --raw` or `overseer ctl force-release`; Overseer does not
  stop a session that sets out to get round it.

## Sign-off

Phase 4 is done when every Result above reads as passed. Record the date and
any failures here.

- Signed off by: Jonathan, accepting I4 and U5 as open follow-ups
- Date: 2026-11-13
- Open follow-ups:
  - F6 (I4): a relay stays pending after a permission dialog is answered No,
    until the next prompt ends.
  - F7 (U5): the Holds box shows no reason or pin revision, and cuts the
    `since` text when `held long` and `waiting:` share the row.
  - F8: `ctl send --raw` does not submit text that wraps.
  - F9: the coordinator's role file points at `~/Repos/2ndBrain` under a temp
    `OVERSEER_HOME`.
  - F4: `claim_target` without `reason` is a protocol error, not a tool error.
  - MR3's expected result predates the a55f1a2 rule and should say a session
    must be running in S0.
  - All of the above were fixed on 2026-11-13 after sign-off, on branch
    `phase-4/followups`; MR3's row now says to start S0 first. I4 (with No
    and Yes) and U5 passed a hand recheck the same day. The same branch fixes a race in the
    `restart_offers_resume` test's stub that failed CI once.
