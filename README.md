# Overseer

A macOS app (GPUI GUI plus a background daemon) for hosting Claude Code sessions.
The daemon owns up to seven Claude Code sessions (S0 to S6). The app shows them,
and quitting or closing the app never stops a session.

## Getting started

### What Overseer is

S0 is the coordinator session: it reads your sprint, proposes a plan and relays
requests. S1 to S6 are worker sessions, one per ticket, each in its own git
worktree. The board shows the sessions, your sprint tickets by Jira status, and
open pull requests. Sessions keep running when you close the app.

### Requirements

- macOS 13 or later.
- Xcode command line tools: `xcode-select --install`. The full Xcode app is not needed.
- Rust through rustup. `make install` installs it for you if it is missing (it asks first).
- Claude Code (`claude` on your `PATH`).
- An Atlassian API token with scopes (create one at
  https://id.atlassian.com/manage-profile/security/api-tokens, "Create API token
  with scopes") for Jira Software, with `read:jira-work`,
  `read:sprint:jira-software` and `read:board-scope:jira-software`.
- Optional, for the Open PRs panel: a second Atlassian API token with scopes,
  this one for Bitbucket, with `read:repository:bitbucket` and
  `read:pullrequest:bitbucket`. See "Pull requests from Bitbucket" below.
- Optional: `bd` (beads) for task tracking. `bb` (the Bitbucket CLI) is no
  longer needed for pull requests. See "What each optional tool adds" below.

### Install

```
git clone https://github.com/techize/overseer.git
cd overseer
make install
```

This builds the app, puts it in `~/Applications/Overseer.app` and starts the
background daemon. Details are under Install below.

### First run

Create `~/.config/overseer/config.toml` (the folder may not exist yet; create it)
with your own site, email and board:

```toml
jira_site = "demo.atlassian.net"
jira_email = "demo@example.test"
jira_board = "1"
```

`jira_board` is optional. The board id is the number in the board's URL
(`.../boards/1`). With it, the sprint name and dates come from Jira.

Restart the daemon (`overseer ctl restart`), open the app, then press `Cmd+,`
for Settings. In the Jira group, type your API token into the "Jira token" row
and Save. The token goes into the macOS Keychain (service `overseer-jira`,
account = your `jira_email`). It is never written to `config.toml` or the logs.
You can instead set `ATLASSIAN_API_TOKEN` in your shell environment.

Overseer removes `ATLASSIAN_API_TOKEN` and `BITBUCKET_API_TOKEN` from every
session's environment. Sessions still run as you, though, so a session can read
either token from the Keychain with `security find-generic-password`. Overseer
does not block that read.

If you change `jira_email` in Settings, restart the daemon before saving the
token, since the token is stored under the email the running daemon has.

When `jira_site` is set Overseer uses the Jira API. With it empty it falls back
to the `acli` command, and without `acli` installed the board says
`acli not found: set jira_site`. Set `jira_site` (the API needs no `acli`). `ticket_pattern`, when left empty, is built
from your Jira project keys. `user_name` defaults to the first word of your
macOS full name.

### Pull requests from Bitbucket

The Open PRs panel reads Bitbucket's REST API with its own token, so `bb` is
optional.

1. At https://id.atlassian.com/manage-profile/security/api-tokens choose
   "Create API token with scopes", pick the Bitbucket app, and give it
   `read:repository:bitbucket` and `read:pullrequest:bitbucket`. A scoped token
   covers one app, so this is a separate token from the Jira one.
2. In Settings, type it into the "Bitbucket token" row (under the Bitbucket
   heading, after the Jira group) and Save. It goes into the Keychain with
   service `overseer-bitbucket` and your `jira_email` as the account. You can
   instead set `BITBUCKET_API_TOKEN` in your shell environment.
3. Tell Overseer which repos to watch, in `config.toml`:

```toml
pr_workspace = "demo-ws"
```

or list clones with `pr_repos = ["~/Repos/demo-api"]`; each folder's `origin`
remote (`https://bitbucket.org/demo-ws/demo-api.git` or
`git@bitbucket.org:demo-ws/demo-api.git`) names the repo. Restart the daemon.

With no Bitbucket token saved, the PR source runs `pr_command` (`bb`) as
before.

### The notes folder

Overseer keeps journals, session notes and sprint files in a notes folder
(`notes_dir`, also accepted as `brain_dir`).

- Overseer owns a notes folder only when it holds the `.overseer-notes` marker.
  It reads any other folder, such as an existing vault, but never creates sprint
  or ticket folders there.
- It creates and marks a folder itself in one case: when `notes_dir` is unset
  and `~/Repos/2ndBrain` does not exist, it uses `~/OverseerNotes`, creating it
  if needed. It then adds the marker, a `Sprints/` folder and its own local git
  history (one starting commit, no push), and turns on the notes writer, which
  writes the journal and session notes and, with `jira_board` set, the sprint
  and ticket folders.
- If `~/OverseerNotes` already holds other files and no marker, Overseer leaves
  it alone, turns the notes writer off and leaves a notice on the board (in
  S0's panel) that names the folder and says how to fix it: set `notes_dir` to
  another folder, or move that one aside, then restart the daemon.
- When git has no `user.name` or `user.email` set, Overseer writes them into
  the owned folder's own git config (your `user_name` and `overseer@localhost`),
  so its commits work on a machine with no git identity.
- Deleting the marker stops Overseer writing sprint and ticket folders there at
  the next sync, without a restart.
- A folder Overseer marked stays owned if you later set `notes_dir` to it
  explicitly, for example after moving it.
- With `jira_board` set, Overseer keeps
  `Sprints/sprint-<slug>/<KEY>-<slug>/ticket.md` in the notes folder, one per
  ticket, in step with Jira. `ticket.md` is the only file it writes there, so
  your own notes in a ticket folder are left alone.

### What each optional tool adds

- `bb` (Bitbucket CLI): only needed when no Bitbucket token is saved. Then the
  Open PRs panel runs `bb` instead of calling the Bitbucket API. With neither,
  the panel shows the error from `bb`.
- `bd` (beads): the task list sessions read and write, shown on the board.
  In a notes folder Overseer owns, a missing `bd` shows as `beads not set up
  here`. In a folder Overseer doesn't own, a missing `bd` shows as a source
  error (`bd not found`). Either way sessions and the Jira board still work.

### Troubleshooting

- Token rejected. The board says `Jira token rejected (401): check the token and
  its scopes` (or 403). Make sure the token is a scoped one with the three
  scopes above, that `jira_email` is the account that created it, and that
  `jira_site` is the site host only, such as `demo.atlassian.net`.
- No tickets. The default `jql` is `sprint in openSprints() AND assignee =
  currentUser() ORDER BY priority DESC`. Check you have an open sprint with
  tickets assigned to you, run the JQL in Jira's search, and check `jira_board`
  is a board of that sprint. If Jira returns none after a poll that had some,
  Overseer keeps the last list once and says so.
- Keychain prompt. Overseer saves the token with `security`, so its own reads
  normally get no prompt. If macOS does ask to allow `security` to use the
  item, choose Always Allow. If it is left unanswered the source says
  `Keychain did not answer`. Unlock your login keychain and wait for the next
  poll, or use `ATLASSIAN_API_TOKEN` instead.
- Bitbucket token rejected. The PR source says `Bitbucket token rejected (401):
  check the token and its scopes` (or 403). Make sure the token was made for
  the Bitbucket app with `read:repository:bitbucket` and
  `read:pullrequest:bitbucket`, and that `jira_email` is the account that
  created it.
- `no Bitbucket remote`. A `pr_repos` folder whose `origin` is not on
  `bitbucket.org` cannot be read through the API. Point `origin` at Bitbucket,
  or poll it through `pr_workspace` instead.
- Token status in Settings. Each token row says `set` (in the Keychain),
  `set (environment)` (only the environment variable holds one), `not set`, or
  `unknown (Keychain did not answer)` when the Keychain is locked or waiting on
  a prompt.
- Notifications not showing. Allow Overseer in System Settings > Notifications,
  and check Focus is not hiding them.

## Install (no admin rights needed)

You need the Xcode Command Line Tools (`xcode-select --install`). The full
Xcode app is not needed: GPUI is built with `runtime_shaders`.

```
git clone https://github.com/techize/overseer.git ~/overseer
cd ~/overseer
make install
```

`make install` runs `scripts/install.sh`, which:

1. installs Rust for your user if `cargo` is neither on your `PATH` nor in
   `~/.cargo/bin` (see below),
2. builds the release binary (`cargo build --release -p overseer`),
3. assembles `~/Applications/Overseer.app` with `scripts/bundle.sh` and signs it
   ad hoc (`codesign --sign -`),
4. links `~/.local/bin/overseer` to the binary inside the app,
5. installs the LaunchAgent `uk.co.techize.overseer`
   (`~/Library/LaunchAgents/uk.co.techize.overseer.plist`), which runs the
   daemon, and starts the daemon if it is not running.

It ends with a summary naming the app, the link, the agent's label and where
the logs go.

### Rust

If Rust is missing, `make install` installs it through rustup into `~/.cargo`
and `~/.rustup`, with no admin rights:

```
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path --profile minimal --default-toolchain none
```

Because this downloads and runs a script, you are asked first. Without a
terminal (for example from another script) `make install` stops and prints that
command instead; run it yourself, or run `OVERSEER_INSTALL_RUST=1 make install`
to let the installer do it. `rust-toolchain.toml` picks the toolchain.
`--no-modify-path` means your shell files are left alone: add `~/.cargo/bin` to
your `PATH` if you want to run `cargo` yourself.

### Environment

Sessions start with your login shell's environment: when the daemon starts it
runs `$SHELL -l -i -c` once to read it, so variables from your shell rc files
(`PATH`, `LANG`, proxies, `AWS_*`, `ANTHROPIC_*` and so on) reach Claude.
Variables that belong to a running Claude Code session (`CLAUDECODE`,
`CLAUDE_CODE_SESSION_*` and the like) are removed, and each session gets
`OVERSEER_SLOT=S<n>`. If the shell fails or takes more than 5 s, sessions get
the daemon's own environment instead and `daemon.log` says so. The agent also
records the `PATH` of the shell you ran `make install` from, which the daemon
itself uses (for example to find `claude` in `/opt/homebrew/bin`). If you
change your shell setup, run `overseer ctl restart` to pick it up; if you move
`claude`, run `make install` again.

Open Overseer from `~/Applications` in Finder or with
`open ~/Applications/Overseer.app`. Put `~/.local/bin` on your `PATH` to use the
`overseer` command.

### An isolated home

With `OVERSEER_HOME` set, the install is kept apart from the default one:

- its agent's label is `uk.co.techize.overseer-home-<hash of the path>` and its
  plist lives in `$OVERSEER_HOME/agent`, not in `~/Library/LaunchAgents`,
- the app goes to `$OVERSEER_HOME/Applications/Overseer.app` and the link to
  `$OVERSEER_HOME/bin/overseer`, not `~/Applications` and `~/.local/bin`,
- its logs go to `$OVERSEER_HOME/logs`.

So installing or uninstalling it never replaces, removes or stops the default
app and agent. `OVERSEER_APP_DIR` and `OVERSEER_LINK_DIR` choose other folders
for the app and the link, with or without `OVERSEER_HOME`. Run such an app with
`OVERSEER_HOME` set, as the summary shows; opening it from Finder would start
it without, against the default home. Its agent is not loaded at login.

Only the install is kept apart. With `OVERSEER_HOME` set and `OVERSEER_CONFIG`
unset, the daemon reads the default config file, `~/.config/overseer/config.toml`,
and Settings writes to it, so that is your real config. Set `OVERSEER_CONFIG`
(and `OVERSEER_TARGETS`) as well for a fully isolated home.

## Upgrade

```
git pull && make install
```

Restarting the daemon ends every running session. Each comes back on the board
as resumable, and `r` on its card resumes it. So when sessions are running,
`make install` says which ones would end and asks before it restarts the
daemon; without a terminal it keeps the daemon. With no session running it
restarts without asking, and when the build has not changed it restarts
nothing.

- `make install YES=1` restarts without asking, even with sessions running.
- `make install NO_RESTART=1` always keeps the running daemon.

A kept daemon goes on running the old build until you restart it with
`overseer ctl restart`. `overseer ctl version` shows the app's build and the
running daemon's, and says when they differ.

## Uninstall

```
make uninstall
```

This runs `scripts/uninstall.sh`, which stops the daemon (ending any sessions),
removes the LaunchAgent, the `~/.local/bin/overseer` link and
`~/Applications/Overseer.app`. It leaves the state dir
`~/Library/Application Support/Overseer/` and the logs in place; delete those by
hand if you want them gone. With `OVERSEER_HOME` set it removes that home's
agent, app and link instead, and leaves `$OVERSEER_HOME` and the default
install in place.

## Daemon

- `overseer ctl status` lists the slots. Any `overseer ctl` or `overseer attach`
  command (or opening the app) starts the daemon if it is not running.
- `overseer ctl version` shows the app's version and build and the running
  daemon's, without starting one.
- `overseer ctl restart` restarts the daemon. Running sessions end and come
  back as resumable; with any running you are asked first on a terminal, and
  `overseer ctl restart --yes` restarts without asking.
- `overseer ctl shutdown` stops the daemon and every session. The daemon then
  stays down until the next `overseer ctl` or `overseer attach` command or app
  launch starts it again. launchd only restarts it by itself if it crashes.
- Logs live in `~/Library/Logs/Overseer/` (`$OVERSEER_HOME/logs` for an isolated
  home): `daemon.log` (the daemon's own log), `daemon.out.log` and
  `daemon.err.log` (its stdout and stderr under launchd) and `gui.log` (the
  app). Set `RUST_LOG` (for example `RUST_LOG=debug`) to change the level; the
  default is `info`.

## Settings

`Cmd+,` (or Overseer > Settings in the menu) opens the Settings view. It shows
`config.toml`, the file the daemon reads: `$OVERSEER_CONFIG` when set, else
`~/.config/overseer/config.toml`. The app reads its terminal and editor keys
from the same file. Each key says whether its value comes from the file or is
the default; `Reset` takes a key out of the file so the default applies again.
Lists are typed as comma-separated items. `Cmd+S` saves, `Esc` closes (with
unsaved edits the first `Esc` only warns).

- Terminal keys (`terminal.font_family`, `terminal.font_size`,
  `terminal.background`, `terminal.foreground`) and `editor.name` apply in the
  app at once, open terminals included.
- Every other key is the daemon's and applies after it restarts; the view marks
  such a key `restart needed` until then. Restart the daemon in the view asks
  first, naming the sessions a restart would end (each comes back as
  resumable), then runs `overseer ctl restart --yes`.
- The Jira token and Bitbucket token rows send a token to the daemon, which
  stores it in the Keychain. They show only whether a token is found, never
  the token itself.
- Read-only here: `brain_writer` (it has a setup checklist), `jira_command`,
  `bd_command` and `sync_command` (a typo turns a source off) and
  `terminal.ansi` (the 16 colours). Change them in `config.toml`; Open opens
  each file that exists in your text editor. `targets.toml` is shown with the
  number of patterns the daemon read from it.

The daemon writes the file, not the app: it keeps your comments, key order and
any keys it does not know, and checks the result still loads. Editing the file
by hand while Settings is open is safe: Save refuses to write over a file that
changed since the view read it, and says to close and reopen Settings.

The About block shows the app's build and the running daemon's, and says when
they differ.

## Jira and notes keys

These live in `config.toml`, and most show in Settings (Jira group, and Notes
group for the notes keys). They apply after a daemon restart.

| Key | Default | Meaning |
|-----|---------|---------|
| `jira_site` | `""` | Your site host, such as `demo.atlassian.net`. Set: the Jira API is used. Empty: `jira_command` (`acli`) is used. |
| `jira_email` | `""` | The account that owns the token; also the Keychain account name. |
| `jira_board` | `""` | Optional board id. With it, sprint name and dates come from Jira and `Sprints/sprint-<slug>/<KEY>-<slug>/ticket.md` is kept in the notes folder. |
| `jira_cloud_id` | `""` | Optional. Looked up from `jira_site` when empty. |
| `jql` | open sprints, assigned to you | The ticket query. |
| `ticket_pattern` | `""` | Empty: built from your Jira project keys, as `(DEMO|OPS)-\d+`. |
| `user_name` | first word of your macOS full name | The name used in prompts and messages. |
| `notes_dir` (alias `brain_dir`) | `~/Repos/2ndBrain` if it exists, else `~/OverseerNotes` | The notes folder. Overseer owns a folder only when it holds the `.overseer-notes` marker; it creates one itself only for the default `~/OverseerNotes`. |

The tokens are not config keys. Set them in Settings (Keychain) or in the
environment:

| Token | Keychain service (account = `jira_email`) | Environment variable | Scopes |
|-------|-------------------------------------------|----------------------|--------|
| Jira | `overseer-jira` | `ATLASSIAN_API_TOKEN` | `read:jira-work`, `read:sprint:jira-software`, `read:board-scope:jira-software` |
| Bitbucket | `overseer-bitbucket` | `BITBUCKET_API_TOKEN` | `read:repository:bitbucket`, `read:pullrequest:bitbucket` |

## Sprint board and open PRs

The board shows a sprint kanban under the session cards and an Open PRs panel
at the top right. All of these keys live in `config.toml` and apply after a
daemon restart.

Sprint stages. Each ticket lands in one column by its Jira status. Six keys
list the status names for a column (matched case-insensitively, after
trimming):

| Key | Default |
|-----|---------|
| `stage_todo` | Backlog, Prioritised Backlog, To Do |
| `stage_blocked` | Blocked, Waiting, On Hold |
| `stage_next` | Next, Selected for Development |
| `stage_in_progress` | In Progress, In Review, Code Review, Review |
| `stage_to_be_tested` | To Be Tested, Testing, In Testing, QA |
| `stage_done` | Done, Closed, Resolved |

- A status in none of the lists lands in In progress and the card shows the
  raw Jira status (for example `Ready for Release`).
- If a status is in more than one list, the first matching list wins, in the
  order Todo, Blocked, Next, In progress, To be tested, Done.
- After you edit a stage list and restart the daemon, a ticket whose status now
  maps to a different stage starts its time in stage from the restart.
- A ticket stays in its Jira column whether or not it has an open PR.
- The old keys `stage_waiting`, `stage_testing` and `stage_in_review` still
  load: the first two fill `stage_blocked` and `stage_to_be_tested` when the
  new key is absent, and the review names are added to In progress.
- Click a session card, or press Cmd+0 to Cmd+6, to open that session over the
  board. Cmd+Esc, the x button, or a click outside the panel closes it.
- Click a ticket card to open its session, or to start one in a free slot.
  Cmd+click opens its ticket folder, and hovering shows its details. A click
  on a Done card still opens its folder. A PR row with a session chip opens
  that session, and Cmd+click on any PR row opens it in the browser.
- A running session waiting for its next prompt shows Your turn, and the
  header counts them ("1 your turn") beside Needs you.
- Pace needs `sprint_start` (a date like `2026-11-09`) and `sprint_days`
  (default 10). Without `sprint_start` the pace marker and the pace warning
  are hidden. The kanban is hidden when there are no sprint tickets.

PRs from Bitbucket. With a Bitbucket token saved (see "Pull requests from
Bitbucket" above), the PR source calls `https://api.bitbucket.org/2.0/`
itself: each `pr_repos` folder's `origin` remote names its repo, and
`pr_workspace` lists the workspace's repos. `pr_command` and
`pr_repos_command` are then not used. Without a token, the PR source runs
`bb` once in each folder listed in `pr_repos`, so each folder must be a clone
`bb` understands (for example `~/Repos/demo-api` and `~/Repos/demo-web`).

| Key | Default | Meaning |
|-----|---------|---------|
| `pr_repos` | `[]` | Repo folders to poll. Empty turns the PR source off. |
| `pr_command` | `bb pullrequest list --state open --output json` | Command run in each repo folder when no Bitbucket token is saved. |
| `pr_poll_secs` | `300` | Seconds between polls. |
| `pr_author` | `""` | Empty shows every open PR in those repos; otherwise only that author's. |
| `pr_workspace` | `""` | A Bitbucket workspace whose repos are found and polled. Empty turns this off. |
| `pr_project` | `""` | Only repos in this Bitbucket project key. Empty means the whole workspace. |
| `pr_active_days` | `30` | Only repos updated in this many days are polled. `0` polls every repo. |
| `pr_repos_command` | `bb repo list --workspace {workspace} --output json` | Lists the workspace's repos when no Bitbucket token is saved; `{workspace}` is replaced by `pr_workspace`. |

A PR links to a ticket when its title or branch name matches `ticket_pattern`
(for example `DEMO-12` in `DEMO-12-fix-login`). If one repo fails, the
other repos' PRs still show and the PR source message names the failing repo.

Whole workspace. Instead of listing folders, set `pr_workspace` (for example
`demo-ws`) and the daemon finds the repos itself. It lists the workspace's
repos (through the API, or by running `pr_repos_command` without a token),
keeps the repos updated in the last `pr_active_days` days (a repo with no
update time is kept), and reads each one's open PRs: through the API, or by
running `pr_command` with `--repository demo-ws/<repo>` added, from `/`, so no
clone is needed. Set `pr_project` to list only that project's repos (without a
token, `--project <key>` is added to the repo list). The repo list is read
again once an hour; if a read fails, the last good list is kept and the PR
source message shows `repos: <error>`. `pr_repos` and `pr_workspace` can both
be set: repos from both are polled, and a folder whose name matches a
workspace repo is polled once, from the folder. The PR source is off only when
both are empty.

This costs one Bitbucket call per active repo per poll, plus one repo list an
hour, and Bitbucket limits API calls per hour. With many active repos, raise
`pr_poll_secs` or set `pr_project` (or lower `pr_active_days`).

## Emergency fallback: `overseer attach`

If the app will not open, you can still reach a session from any terminal:

```
overseer attach 3
```

This mirrors S3 in the current terminal. Press `Ctrl-]` to detach; the session
keeps running.

## Development

```
make build
make test
make lint
cargo run -p overseer
```

Tests run against a temporary `OVERSEER_HOME`, so they never touch the installed
daemon.
