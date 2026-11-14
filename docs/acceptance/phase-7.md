# Phase 7 acceptance

Phase 7 (polish) is done only when every row below passes. It adds the
installer and upgrade path (`make install`, `make uninstall`, a per-home
LaunchAgent, build ids, the restart rules), the Locks view, the Settings view
with `ctl settings`, and the sprint name and day in the board header. Every row
runs on the home Mac in an isolated rig: a state folder at `/tmp/ov-hand7`, a
throwaway git repo `demo-api`, a small test vault, stub `bd`, Jira and
`claude`, and an installed copy of the app inside the rig (the install goes to
`$OVERSEER_HOME/Applications`, not to `~/Applications`). Never run any of it
against the real vault (`~/Repos/2ndBrain`), the real
`~/.claude/CURRENT_SESSION.md`, `~/Repos/.worktrees`, the installed app
`~/Applications/Overseer.app`, its LaunchAgent `uk.co.techize.overseer`, the
link `~/.local/bin/overseer`, `~/Library/Application Support/Overseer` or
`~/.config/overseer`. Row Z1 checks exactly that.

Unit and integration tests cover the logic (`cargo test --workspace`). They do
not replace a hand check where a row asks for one: rows marked "GUI" need the
app on screen, rows marked "terminal" need a real terminal (a tty), and the
rest are scripted. Three things only a hand check covers: the y/N Rust prompt
on a real terminal (I3), the layout and clicks of the Settings view (S1, S12)
and what the Locks view looks like (L2, L6). The Result column reads `pending`
until Jonathan (or a controller) has run the row; then write what was
observed, and when. The examples in this file (`DEMO-12`, `DEMO-13`, `ov-3`,
`demo-api`, `uat/root`, the `demo lane` label, `S1` to `S5`, builds `rig-a`,
`rig-b` and `rig-c`) are invented.

The checks quote the texts the code produces. If a text differs on screen,
that is a finding: record it in the Result column.

## Setup for the checks

Use one Terminal tab for every step so it keeps the variables.

**Risk, read first.** `make install` builds, bundles, installs a LaunchAgent
and starts a daemon. Run with your normal environment it would replace the real
installed app, rewrite the real agent `uk.co.techize.overseer`, restart your
real daemon (ending the real sessions) and link `~/.local/bin/overseer`. So
every install, uninstall and app start in this list carries the isolation
prefix in step 1: `OVERSEER_HOME` set, which moves the app to
`$OVERSEER_HOME/Applications`, the link to `$OVERSEER_HOME/bin` and the agent to
a per-home label `uk.co.techize.overseer-home-<8 hex>` with its plist under
`$OVERSEER_HOME/agent`; plus a temp `HOME`, `SHELL=/bin/sh` and a throwaway
`CLAUDE_CONFIG_DIR`, so no session reads your settings, hooks or
`~/.claude.json`. Never `open` the rig's bundle and never start it from Finder
(LaunchServices would start it with your real environment, against the real
socket); always run `$OVERSEER_HOME/Applications/Overseer.app/Contents/MacOS/overseer`
with the prefix. Never run `ctl` in a tab where `OVERSEER_HOME` is not exported:
it would talk to your real daemon. A temp `HOME` has no `~/.cargo`, so export
the real `CARGO_HOME` and `RUSTUP_HOME` first (step 1) and build once in the
real environment. No row uses a real `claude`; every session is the stub from
step 2. Step 0 records the real state so Z1 can prove nothing leaked.

### Step 0: record the real state

```
git -C ~/Repos/2ndBrain status --short > /tmp/ov-hand7-real-status.before
git -C ~/Repos/2ndBrain log -3 --format='%h %s' > /tmp/ov-hand7-real-log.before
cp -L ~/.claude/CURRENT_SESSION.md /tmp/ov-hand7-real-cs.before
ls -laR ~/Repos/.worktrees > /tmp/ov-hand7-real-wt.before 2>&1
launchctl list 2>/dev/null | awk '$3 == "uk.co.techize.overseer"' > /tmp/ov-hand7-real-agent.before
find ~/Library/Application\ Support/Overseer -type f 2>/dev/null | sort > /tmp/ov-hand7-real-state.before
shasum ~/Applications/Overseer.app/Contents/MacOS/overseer ~/Applications/Overseer.app/Contents/Info.plist > /tmp/ov-hand7-real-app.before 2>&1
readlink ~/.local/bin/overseer > /tmp/ov-hand7-real-link.before 2>&1
shasum ~/Library/LaunchAgents/uk.co.techize.overseer.plist > /tmp/ov-hand7-real-plist.before 2>&1
shasum ~/.config/overseer/config.toml ~/.config/overseer/targets.toml > /tmp/ov-hand7-real-cfg.before 2>&1
touch /tmp/ov-hand7-real-marker
```

The agent line uses an exact match on the label (`$3 == ...`) because the
rig's label shares the prefix `uk.co.techize.overseer`; a `grep` would match
both. A file that does not exist makes its `.before` file hold the error, and
Z1 compares the same error. (`/tmp/ov-hand7-*` is outside `/tmp/ov-hand7/`, so
`rm -rf /tmp/ov-hand7` at the end does not remove them before Z1.)

### Step 1: state folder, binary, prefix

Build from this branch's worktree before it is merged, or from the merged
`main` checkout after (`cd` to whichever you are testing). Build once with the
real environment:

```
export CARGO_HOME=${CARGO_HOME:-$HOME/.cargo}
export RUSTUP_HOME=${RUSTUP_HOME:-$HOME/.rustup}
cd ~/overseer/.claude/worktrees/phase-7   # or the main checkout once merged
cargo build --release -p overseer
export SRC=$PWD
export OVERSEER_HOME=/tmp/ov-hand7
export OVERSEER_CONFIG=$OVERSEER_HOME/config.toml
export OVERSEER_TARGETS=$OVERSEER_HOME/targets.toml
export RH=$OVERSEER_HOME/home          # the rig's HOME
export OV=$OVERSEER_HOME/Applications/Overseer.app/Contents/MacOS/overseer
export V=$OVERSEER_HOME/vault          # the test vault
export R=$OVERSEER_HOME/demo-api       # the throwaway repo
export WT=$OVERSEER_HOME/worktrees     # worktree_root
export W=$OVERSEER_HOME/work
iso() { env HOME=$RH SHELL=/bin/sh CLAUDE_CONFIG_DIR=$OVERSEER_HOME/claude-config "$@"; }
mkdir -p $OVERSEER_HOME/stubs $RH $OVERSEER_HOME/claude-config $WT $W
printf 'export PATH=/tmp/ov-hand7/stubs:$PATH\n' > $RH/.profile
export PATH=$OVERSEER_HOME/stubs:$PATH
```

`iso` is the isolated prefix, a function so it works in zsh and bash alike
(a variable holding `HOME=... SHELL=...` would not be split into words by zsh):
write `iso <command>` or `iso VAR=value <command>`, for example `iso make install`. The rows below spell that as "with the prefix". The
install itself is row I1. The app is not installed yet, so `$OV` does not exist
until I1 has run; scripted rows before it do not use `ctl`.

The daemon reads the work list and starts sessions with the login shell's
environment, so the temp `HOME`'s `.profile` puts the stubs first and every
start uses `SHELL=/bin/sh`.

### Step 2: stubs, repo and vault

The stub `bd` serves three beads and logs its calls; the stub `acli` (Jira)
serves three tickets; the stub `claude` logs its arguments and sleeps. The
default session command is `claude`, so a session started without `-- cmd` is
a stub `claude` and is resumable; a session started with `-- sleep 3600` or
`-- cat` is not (only `claude` resumes).

```
cat > $OVERSEER_HOME/stubs/bd <<'STUB'
#!/bin/sh
{ echo "argv: $*"; } >> /tmp/ov-hand7/bd.log
if [ "$1" = list ]; then
  echo '[{"id":"ov-3","title":"DEMO-12 wire the widget","status":"open","priority":1},{"id":"ov-4","title":"Standalone chore","status":"open","priority":2},{"id":"ov-5","title":"DEMO-13 tidy the docs","status":"open","priority":2}]'
else echo "bd stub ok: $*"; fi
STUB
cat > $OVERSEER_HOME/stubs/acli <<'STUB'
#!/bin/sh
cat /tmp/ov-hand7/jira.json
STUB
cat > $OVERSEER_HOME/jira.json <<'EOF2'
{"issues":[
  {"key":"DEMO-12","fields":{"summary":"Widget wiring","status":{"name":"In Progress"}}},
  {"key":"DEMO-13","fields":{"summary":"Docs tidy","status":{"name":"To Do"}}},
  {"key":"DEMO-14","fields":{"summary":"Release notes","status":{"name":"Done"}}}
]}
EOF2
cat > $OVERSEER_HOME/stubs/claude <<'STUB'
#!/bin/sh
{ echo "--- OVERSEER_SLOT=$OVERSEER_SLOT HOME=$HOME cwd=$(pwd)"; for a in "$@"; do echo "$a"; done; } >> /tmp/ov-hand7/claude-stub.log
exec sleep 3600
STUB
chmod +x $OVERSEER_HOME/stubs/bd $OVERSEER_HOME/stubs/acli $OVERSEER_HOME/stubs/claude
: > $OVERSEER_HOME/bd.log; : > $OVERSEER_HOME/claude-stub.log
```

The throwaway repo and the test vault (no git remote; the writer is off, so
nothing here commits):

```
git init -q -b main $R && git -C $R config user.name demo && git -C $R config user.email demo@example.test
printf '# demo-api\n' > $R/README.md && git -C $R add -A && git -C $R commit -q -m seed
mkdir -p $V/Sprints/sprint-26-01/DEMO-12-demo-lane
printf '# demo sprint\n' > $V/Sprints/sprint-26-01/SPRINT-DASHBOARD.md
printf 'old history\n' > $OVERSEER_HOME/CURRENT_SESSION.md
```

The rig's date today is Saturday 14 November 2026 (`date '+%A, %B %d, %Y'`);
rows H1 and L6 work from the real date, so check it first.

The Rust-home stubs (rows I2 and I3 only; nothing is downloaded). The stub
`curl` logs its call and prints a script that creates a stub `cargo`; the stub
`cargo` logs its arguments and copies the prebuilt binary (named in
`$RUSTH/src-binary`, written here with the real path) into `$CARGO_TARGET_DIR`:

```
export RUSTH=/tmp/ov-hand7-rust
mkdir -p $RUSTH/home $RUSTH/fakebin $RUSTH/cargo
printf '%s\n' "$SRC/target/release/overseer" > $RUSTH/src-binary
cat > $RUSTH/fakebin/curl <<'STUB'
#!/bin/sh
echo "curl stub: $*" >> /tmp/ov-hand7-rust/curl.log
cat <<'INNER'
#!/bin/sh
mkdir -p /tmp/ov-hand7-rust/cargo/bin
cat > /tmp/ov-hand7-rust/cargo/bin/cargo <<'CARGO'
#!/bin/sh
echo "cargo $*" >> /tmp/ov-hand7-rust/cargo.log
mkdir -p "$CARGO_TARGET_DIR/release"
cp "$(cat /tmp/ov-hand7-rust/src-binary)" "$CARGO_TARGET_DIR/release/overseer"
CARGO
chmod +x /tmp/ov-hand7-rust/cargo/bin/cargo
INNER
STUB
chmod +x $RUSTH/fakebin/curl
```

### Step 3: config

```
cat > $OVERSEER_HOME/config.toml <<EOF2
# rig config, hand-written: this comment line must survive every save
brain_dir = "$V"
beads_dir = "$V"
brain_writer = false
current_session = "$OVERSEER_HOME/CURRENT_SESSION.md"
bd_command = ["$OVERSEER_HOME/stubs/bd"]
jira_command = ["$OVERSEER_HOME/stubs/acli", "{jql}"]
ticket_pattern = 'DEMO-\d+'
beads_poll_secs = 5
jira_poll_secs = 5
worktree_root = "$WT"

[unknown_table]
keep = "me"
EOF2
cp $OVERSEER_CONFIG $OVERSEER_HOME/config.toml.orig
```

`targets.toml` does not exist, so the compiled-in patterns are used. The
`config.toml.orig` copy is what rows S2 to S7 restore between runs
(`cp $OVERSEER_HOME/config.toml.orig $OVERSEER_CONFIG`, then restart the daemon
with `$OV ctl restart --yes` so it re-reads the file).

### Step 4: helpers the rows use

```
# Frees every claim and every queue place on the targets the Locks rows use.
lreset() {
  for t in argo:uat/root repo:demo-api:main; do
    for n in S1 S2 S3 S4 S5; do $OV ctl release $n $t >/dev/null 2>&1; done
  done
  for n in 1 2 3 4 5 6; do $OV ctl release S$n argo:uat/root >/dev/null 2>&1; done
  for k in DEMO-12 DEMO-13 ov-3 ov-4 ov-5; do $OV ctl release $k >/dev/null 2>&1; done
}
# Stops every session (the stub sessions only live in the rig).
sreset() { for n in 0 1 2 3 4 5 6; do $OV ctl stop $n >/dev/null 2>&1; done; }
# Records the daemon's pid and build for the rows that compare them.
dinfo() { $OV ctl version; }
```

`lreset` loops because releasing a holder hands the target to the next waiter,
so each round frees one more; run it twice when S1, S4 and S5 are all queued.
For the Locks rows, the stub sessions S1 and S3 are started like this (the
default command, so they are the stub `claude`; their label is the text after
`--label`):

```
$OV ctl start 1 --cwd $W --label "demo lane"
$OV ctl start 3 --cwd $W --label docs
```

### Step 5: start the app, isolated

After row I1 has installed the rig, start the app like this (never with `open`):

```
iso $OV &
```

The app connects to the rig's daemon (or starts it through the rig agent, row
I4). Scripted rows that need only the daemon can use `$OV ctl status` with
`OVERSEER_HOME` exported, which starts the daemon through the rig agent when the
agent's plist exists.

Pre-flight after I1: `$OV ctl items` shows `beads  ok  3 items` and the Jira
source ok with 3 items; if it shows anything else the daemon is not using the
stub `PATH`: stop and fix.

### Step 6: finish

Run row U8 (uninstall), then row Z1, then
`rm -rf /tmp/ov-hand7 /tmp/ov-hand7-rust /tmp/ov-hand7-fake /tmp/ov-hand7-real-*`.
Quit the app first (it holds a connection to the rig's daemon).

## Install and upgrade

Run I1 to I5 and U1 to U8 in order: each starts from the state the one before
left. "With the prefix" means `iso` in front of the command, run from
`$SRC` (the checkout).

| ID | Check | Steps | Pass | Result |
|----|-------|-------|------|--------|
| I1 | A fresh install into the rig | With the prefix, and with the prebuilt binary so no build runs: `cd $SRC; iso OVERSEER_BUILD=rig-a OVERSEER_INSTALL_BINARY=$SRC/target/release/overseer make install`. Then `LABEL=$(basename $OVERSEER_HOME/agent/*.plist .plist); echo $LABEL`, `ls -d $OVERSEER_HOME/Applications/Overseer.app`, `ls -l $OVERSEER_HOME/bin/overseer`, `codesign -dv $OVERSEER_HOME/Applications/Overseer.app 2>&1 \| head -3`, `$OV ctl version`, `plutil -p $OVERSEER_HOME/agent/$LABEL.plist`, `launchctl print gui/$(id -u)/$LABEL \| grep -E 'state\|program'`, and `launchctl list \| awk '$3 == "uk.co.techize.overseer"' \| diff - /tmp/ov-hand7-real-agent.before` | The install prints `bundled <app> (0.1.0, build rig-a)` and `installed <plist> as uk.co.techize.overseer-home-<8 hex> (runs <exe>)`, then `Started the daemon (build rig-a).` and ends with `Overseer is installed.`, `  App:    $OVERSEER_HOME/Applications/Overseer.app`, `  CLI:    $OVERSEER_HOME/bin/overseer (add $OVERSEER_HOME/bin to PATH if it is not there)`, `  Daemon: LaunchAgent uk.co.techize.overseer-home-<8 hex>, logs in $OVERSEER_HOME/logs` and `Run it with OVERSEER_HOME set: OVERSEER_HOME=/tmp/ov-hand7 /tmp/ov-hand7/Applications/Overseer.app/Contents/MacOS/overseer`. The app and the link are under `$OVERSEER_HOME`, not under `~/Applications` or `~/.local/bin`; `codesign` shows a signature (ad hoc is fine). `$LABEL` has the form `uk.co.techize.overseer-home-` plus 8 hex digits. `ctl version` prints `app     0.1.0 build rig-a` and `daemon  0.1.0 build rig-a  pid <n>  since HH:MM`, with no notice line. The plist's `EnvironmentVariables` holds `PATH`, `HOME` (`/tmp/ov-hand7/home`), `SHELL` (`/bin/sh`), `OVERSEER_HOME`, `OVERSEER_CONFIG`, `OVERSEER_TARGETS` and `CLAUDE_CONFIG_DIR` (`plutil -p` lists keys sorted, so the order shown is not the file order). `launchctl print` shows `state = running`. The final `diff` prints nothing: the real agent line is unchanged. | pending |
| I2 | Rust missing, with no terminal: the installer stops and says how, or installs on request (nothing is downloaded) | Use a second home so the rig above stays as it is; write its stubs with the block in step 2 ("The Rust-home stubs"). A PATH with no cargo: `NOCARGO=$RUSTH/fakebin:/usr/bin:/bin:/usr/sbin:/sbin` (check `PATH=$NOCARGO command -v cargo` prints nothing). Then from `$SRC`: (a) `env -i PATH=$NOCARGO HOME=$RUSTH/home SHELL=/bin/sh OVERSEER_HOME=$RUSTH CARGO_HOME=$RUSTH/cargo OVERSEER_CURL=$RUSTH/fakebin/curl CARGO_TARGET_DIR=$RUSTH/target OVERSEER_BUILD=rig-r make install </dev/null; echo exit=$?`; (b) the same line with `OVERSEER_INSTALL_RUST=1` added (the stub cargo copies the binary from `$RUSTH/src-binary`, so `env -i` needs no `SRC`) | (a) stops before installing anything and prints `Rust is not installed. To install it for your user (no admin rights needed), run:`, then `  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \| sh -s -- -y --no-modify-path --profile minimal --default-toolchain none`, then `then run make install again, or run OVERSEER_INSTALL_RUST=1 make install to let it do this.`; exit is not 0; `$RUSTH/curl.log` does not exist and `ls $RUSTH/Applications` fails. (b) prints `Rust is not installed. Installing it for your user with rustup (no admin rights needed).`; `$RUSTH/curl.log` has one line naming `https://sh.rustup.rs` (the stub, so nothing was downloaded); `$RUSTH/cargo.log` has a line `cargo build --release -p overseer`; the install finishes with `Overseer is installed.` and, after the summary, `Add ~/.cargo/bin to your PATH to run cargo yourself.`. The real `~/.cargo` and `~/.rustup` are unchanged. | pending |
| I3 | Rust missing, on a real terminal: the y/N prompt (terminal) | A hand check, because no test drives a tty. In a real Terminal tab (a tty on stdin), run (a) with `rm -rf $RUSTH/cargo $RUSTH/curl.log $RUSTH/cargo.log` first, then `env -i PATH=$NOCARGO HOME=$RUSTH/home SHELL=/bin/sh OVERSEER_HOME=$RUSTH CARGO_HOME=$RUSTH/cargo OVERSEER_CURL=$RUSTH/fakebin/curl CARGO_TARGET_DIR=$RUSTH/target OVERSEER_BUILD=rig-r make install` and answer `N` (or just Enter); then (b) the same line again and answer `y` | (a) The line `Rust is not installed. Install it for your user with rustup now? This downloads and runs https://sh.rustup.rs (no admin rights needed). [y/N] ` is printed with the cursor after it (no newline), and the default is No. After `N` or Enter it prints `Rust was not installed. To install it yourself, run:`, the `  curl --proto '=https' ...` line from I2, and `then run make install again.`; exit 1; `$RUSTH/curl.log` does not exist. (b) After `y` it prints `Rust is not installed. Installing it for your user with rustup (no admin rights needed).`, runs the stub curl and stub cargo (`$RUSTH/cargo.log` shows `cargo build --release -p overseer`) and finishes with `Overseer is installed.` Typing `yes` or `Y` also proceeds; any other answer stops as in (a). No admin password is asked. | pending |
| I4 | Autostart goes through the rig's agent, and the session environment is the rig's | `$OV ctl shutdown`, wait two seconds, then `$OV ctl status` with no prefix (only `OVERSEER_HOME` exported, as in your tab; the daemon it starts must still get the rig's `HOME` from the agent's plist). Then `launchctl print gui/$(id -u)/$LABEL \| grep 'state ='`. Start a session: `$OV ctl start 2 --cwd $W --label chk`, wait two seconds, `grep -- '--- OVERSEER_SLOT=2' $OVERSEER_HOME/claude-stub.log \| tail -1`. Finally `$OV ctl stop 2` | The `ctl shutdown` exits the daemon with code 0, so the agent does not respawn it. The bare `ctl status` starts the daemon (it does not fail with a connection error) and prints a slot list; `launchctl print` shows `state = running` for the rig label. The stub log line for the session ends `OVERSEER_SLOT=2 HOME=/tmp/ov-hand7/home cwd=/tmp/ov-hand7/work` (macOS may print `/private/tmp/...` for the cwd): the session got the rig's `HOME`, not your real one (the phase 6 hazard of an auto-started daemon running with the caller's environment is gone). | pending |
| I5 | Reinstalling the same build changes nothing | `cd $SRC; iso OVERSEER_BUILD=rig-a OVERSEER_INSTALL_BINARY=$SRC/target/release/overseer make install`; then `$OV ctl version` | The install prints `The daemon already runs build rig-a; nothing to restart.`; the daemon's pid in `ctl version` is the same as in I4; no restart question is asked. | pending |
| U1 | An upgrade with no session running restarts without asking | Make sure no session runs: `sreset; $OV ctl status` shows no `running` slot. `iso OVERSEER_BUILD=rig-b OVERSEER_INSTALL_BINARY=$SRC/target/release/overseer make install </dev/null`; `$OV ctl version` | It prints `Restarted the daemon (build rig-a to rig-b).` and does not print the warning or ask anything (stdin is `/dev/null`). `ctl version` shows app and daemon both `build rig-b` and a new pid. | pending |
| U2 | An upgrade with sessions running and no terminal keeps the daemon, and says so | Start two stub sessions: `$OV ctl start 1 --cwd $W --label "demo lane"; $OV ctl start 3 --cwd $W --label docs`. `iso OVERSEER_BUILD=rig-c OVERSEER_INSTALL_BINARY=$SRC/target/release/overseer make install </dev/null`; `$OV ctl version`; `$OV ctl status`. Then start the app (step 5) | The install prints `The daemon (build rig-b) has 2 running sessions: S1 demo lane, S3 docs. Restarting it ends them; each comes back as resumable, and r on its card resumes it.` and then `The daemon keeps running build rig-b. Run overseer ctl restart when you are ready to finish the upgrade.` It does not ask, and exits 0 (the install is done, the daemon is old). `ctl status` still lists S1 and S3 as `running`, with their old pids. `ctl version` prints `app     0.1.0 build rig-c`, `daemon  0.1.0 build rig-b  pid <n>  since HH:MM` and the notice `The daemon runs build rig-b, this app build rig-c. Restart the daemon to finish the upgrade (overseer ctl restart, or Settings).` The app's status line shows that same notice when it connects (GUI), and the About block in Settings (row S1) shows it too. | pending |
| U3 | The question on a terminal, then the forced upgrade | Repeat U2's install line in a real Terminal tab (a tty), with S1 and S3 still running: it ends with `Restart the daemon now? [y/N] ` after the warning; answer `n`. Then `iso OVERSEER_BUILD=rig-c OVERSEER_INSTALL_BINARY=$SRC/target/release/overseer make install YES=1`; `$OV ctl version`; `$OV ctl status`. In the app, look at the board and press `r` on S1's card | On `n` the install prints `The daemon keeps running build rig-b. Run overseer ctl restart when you are ready to finish the upgrade.` and S1 and S3 keep running. With `YES=1` it prints the warning, does not ask, and prints `Restarted the daemon (build rig-b to rig-c).` `ctl version` shows build `rig-c` on both lines and no notice. `ctl status` shows `S1  resumable` and `S3  resumable`. The app reconnects by itself; the board shows S1 and S3 as resumable, and `r` on S1's card starts it again (`ctl status` shows `S1  running`). | pending |
| U4 | `ctl restart` with a session running | With S1 running (U3 left it so) and a terminal: `$OV ctl restart`, answer `n`. Again with `</dev/null`. Then `$OV ctl restart --yes` | On a terminal it prints `The daemon (build rig-c) has 1 running session: S1 demo lane. Restarting it ends them; each comes back as resumable, and r on its card resumes it.` and `Restart the daemon now? [y/N] `; on `n` it prints `Kept the daemon (build rig-c) because 1 running session would end: S1 demo lane. Run overseer ctl restart --yes to restart it anyway.` With `</dev/null` it prints the same kept line without asking. The launchd job is not touched on the kept path (the pid in `ctl version` is unchanged). `--yes` prints `Restarted the daemon (build rig-c to rig-c).` and S1 is `resumable`. | pending |
| U5 | `--no-restart` and the agent's own start | `$OV ctl start 1 --cwd $W --label "demo lane"`, then `iso OVERSEER_BUILD=rig-d OVERSEER_INSTALL_BINARY=$SRC/target/release/overseer make install NO_RESTART=1 YES=1`; `$OV ctl version`. Then `sreset`, `$OV ctl shutdown`, wait two seconds, and run `iso OVERSEER_BUILD=rig-d OVERSEER_INSTALL_BINARY=$SRC/target/release/overseer make install NO_RESTART=1` | The first install keeps the daemon even with `YES=1` (`--no-restart` wins) and prints `The daemon keeps running build rig-c. Run overseer ctl restart when you are ready to finish the upgrade.`; `ctl version` shows daemon `rig-c`, app `rig-d` and the notice. The second install has no daemon to keep, so it starts one: `Started the daemon (build rig-d).` and `ctl version` shows both builds `rig-d`. (`--no-restart` keeps a running daemon, with or without sessions; with no daemon running `install-agent` still starts one.) | pending |
| U6 | A daemon that cannot list its sessions: the restart is refused (fails closed) | Set up by hand with a fake home and a socket that accepts and hangs up: `export FH=/tmp/ov-hand7-fake; mkdir -p $FH` and write `$FH/hang.py` (a heredoc: `cat > $FH/hang.py <<'PY'`, the lines `import os, socket`, `p = "/tmp/ov-hand7-fake/overseer.sock"`, `if os.path.exists(p): os.unlink(p)`, `s = socket.socket(socket.AF_UNIX)`, `s.bind(p)`, `s.listen(5)`, `while True:`, `    c, _ = s.accept()`, `    c.close()` and `PY`), then `python3 $FH/hang.py &` (note its job number). Then `OVERSEER_HOME=$FH $OV ctl restart </dev/null`. Stop the fake with `kill %<n>; rm -rf $FH` | It prints `The daemon (build unknown) did not list its sessions, so some may be running. Restarting it ends any that are; each comes back as resumable, and r on its card resumes it.` and `Kept the daemon (build unknown) because its sessions could not be listed. Run overseer ctl restart --yes to restart it anyway.` It never touches launchctl or any real daemon (`launchctl list \| awk '$3 == "uk.co.techize.overseer"'` is unchanged). The same unlisted wording is used by the Settings restart question (row S10). | pending |
| U7 | An upgrade that changes nothing but the agent file | `$OV ctl shutdown`, then `iso $OV install-agent` with no daemon running; then run the same `install-agent` again; then `ls $OVERSEER_HOME/agent` | The first run prints `installed <plist> as <label> (runs <exe>)` and starts the daemon (`Started the daemon (build rig-d).`); the second prints `The daemon already runs build rig-d; nothing to restart.` (same plist text, same build). There is exactly one plist in `$OVERSEER_HOME/agent`. | pending |
| U8 | `make uninstall` removes the rig and leaves the state | `cd $SRC; iso make uninstall` (first quit the app; then, if I2 or I3 installed the Rust home, `cd $SRC; env HOME=$RUSTH/home SHELL=/bin/sh OVERSEER_HOME=$RUSTH make uninstall` as well, or its agent stays loaded); then `ls $OVERSEER_HOME/Applications $OVERSEER_HOME/bin $OVERSEER_HOME/agent`, `launchctl list \| grep -c overseer-home-`, `ls $OVERSEER_HOME/overseer.db $OVERSEER_HOME/logs` | It prints `removed <plist>`, `removed $OVERSEER_HOME/bin/overseer`, `removed $OVERSEER_HOME/Applications/Overseer.app` and `Overseer is uninstalled. State and logs in /tmp/ov-hand7 are left in place.` The three `ls` of the app, link and agent folders show nothing left; `launchctl list \| grep -c overseer-home-` prints `0` (with the Rust home uninstalled too); `overseer.db` and `logs` are still there. The real app, link and agent are untouched (row Z1). With no app binary in a custom home it would say `No app binary; remove $OVERSEER_HOME/agent by hand.` (not run here). | pending |

For the rows from L1 on, reinstall the rig first (row I1 again, build `rig-a`;
the daemon starts through the rig agent), then start the app (step 5). The
rows start their own stub sessions.

## The Locks view (GUI)

The board strip, `l`, Cmd+Shift+L, the rows and the guarded force release. All
claims below are made with `ctl claim <slot> <target> [reason]`. `Jonathan` in
a relay is the person at the keyboard.

Setup used by L2 to L6 (run it again, after `lreset`, whenever a row says
"set up"):

```
lreset; sreset
$OV ctl start 1 --cwd $W --label "demo lane"
$OV ctl start 3 --cwd $W --label docs
$OV ctl start 4 --cwd $W --label sync
$OV ctl start 5 --cwd $W --label misc
$OV ctl assign 1 DEMO-12
$OV ctl claim 1 argo:uat/root
$OV ctl claim 4 argo:uat/root sync; echo exit=$?
$OV ctl claim 5 argo:uat/root; echo exit=$?
$OV ctl claim 3 repo:demo-api:main push
$OV ctl locks
```

The two queued claims exit 2 (queued, not an error) and print `argo:uat/root is
held by S1 since HH:MM for DEMO-12. You are number 1 in the queue; Overseer will
tell you here when you hold it.` (and number 2 for S5). `ctl locks` shows a
line for `argo:uat/root` (`S1  claim  since HH:MM  for DEMO-12`, then
`  waiting: S4 since HH:MM` and S5), one for `repo:demo-api:main` (S3, `for
push`), and `relay` lines for the queue notices.

| ID | Check | Steps | Pass | Result |
|----|-------|-------|------|--------|
| L1 | Opening and leaving the view | In the app, on the board with the setup above, press `l`. Press Esc. Click the stub session S3's card to open its terminal, press Cmd+Shift+L (a session started with `-- cat` instead of the default command would show a typed `l`; here the screen gets no `l`). Press Esc. Press `l` again, then Cmd+1. Press `l`, then Cmd+Esc. Open Settings (Cmd+,) and press Cmd+Shift+L there | `l` on the board opens a card titled `Locks`, with the summary `2 held, 2 waiting`. Esc goes back to where you came from (the board). Cmd+Shift+L works from a session terminal and from every screen except the new-session dialog, the picker, the plan view and Settings (in Settings it does nothing). The stub session's terminal does not receive an `l`. Cmd+1 from the view goes to S1's session. Cmd+Esc goes straight to the board. The footer reads `Up and Down pick a target · Enter or f releases it · Esc goes back`. | pending |
| L2 | The rows, and what they look like (hand check) | Set up (see above) and open the view with `l`. Add a waiter-only target by hand: `$OV ctl shutdown`; `sqlite3 $OVERSEER_HOME/overseer.db "insert into waits (target, slot, via, reason, item, at) values ('repo:demo-api:hotfix', 4, 'tool', 'urgent', null, strftime('%s','now'))"` (check the column names with `sqlite3 $OVERSEER_HOME/overseer.db .schema` first and adapt; if the table does not accept it, skip this part and say so in Result); then `$OV ctl status` to restart the daemon, and reopen the view. Look at the card, press Down and Up through the rows | Rows are sorted and each shows the target in semibold, then its holder (`S1 demo lane`; `S3 docs`), then a line such as `claim  since HH:MM (2h 5m)  for DEMO-12` (the age uses `12m`, `2h 5m` or `3d 4h` forms; the first row says `for DEMO-12` because S1 owns only DEMO-12, and the S3 row says `for push`, its reason). Under `argo:uat/root` the waiters are indented: `1. S4 since HH:MM, for sync` and `2. S5 since HH:MM` (no reason, no trailing comma). A waiter-only row shows the holder as `nobody`. The selected row has an accent border; the unselected rows do not; the text is readable and nothing overlaps or is cut off, in light and in dark mode. The orange `held long` appears only in row L6. | pending |
| L3 | The guarded force release | Set up. In the view, select `argo:uat/root` with Up and Down, press Enter | A confirm line appears: `Release S1's argo:uat/root? S4 is next in the queue. Enter / Esc` (for a target nobody waits on: `Release S3's repo:demo-api:main? Nobody is waiting. Enter / Esc`). Esc closes it with nothing released (`$OV ctl locks` is unchanged). Enter again, then Enter to confirm: the status line shows `Released argo:uat/root. S4 holds it now.`; the row now shows S4 as the holder with S5 first in the queue. `$OV ctl locks` shows the claim held by S4, and hand-off relay lines: `relay  S1  ... Jonathan released argo:uat/root, which you held.` and `relay  S4  ... Jonathan released argo:uat/root. You now hold it.` (either `pending (reason)` or `dropped: session ended`, depending on whether the stub session accepted it; relays to a slot with no session are queued as already dropped). `f` does the same as Enter. A waiter-only row (L2) shows `Nobody holds repo:demo-api:hotfix; nothing to release` instead of a confirm. | pending |
| L4 | A confirm for a target that changed hands | Set up. In the view select `argo:uat/root`, press Enter (the confirm shows). In the terminal: `$OV ctl force-release argo:uat/root` (this hands it to S4). Then press Enter in the app | The confirm closes with `argo:uat/root changed hands; look again` and nothing more is released: `$OV ctl locks` still shows S4 holding `argo:uat/root` with S5 waiting. The selection stays on that target. | pending |
| L5 | The board strip | Set up, then Esc to the board. Read the strip, click it. Then `lreset` and look again | With claims the strip reads `Locks: 2 held, 2 waiting (l)` (the summary of the view, then `(l)`); clicking it opens the Locks view. After `lreset` it reads `Locks: none held (l)`, and the view says `Nothing is held and nobody is waiting.` | pending |
| L6 | `held long` after the idle limit | Set up (so S1 holds `argo:uat/root`). `$OV ctl settings set lock_idle_mins 1`; `$OV ctl restart --yes`; set up again (the restart ended the sessions); wait more than two minutes with the view open, then press Down and Up (the view redraws on events and key presses, so ages refresh then), look at the board, run `$OV ctl locks`. Then `$OV ctl settings reset lock_idle_mins; $OV ctl restart --yes` | After the wait the row shows `held long` in orange after the age text (`claim  since HH:MM (2m)  for DEMO-12  held long`); the board shows the pill `argo:uat/root · held long`; `ctl locks` shows the line ending `held long`. A target held for under a minute has no marker. After the reset and restart the limit is back to its default and the marker is gone. | pending |

## Settings (GUI and ctl)

Restore the rig config before each row that edits it:
`cp $OVERSEER_HOME/config.toml.orig $OVERSEER_CONFIG; $OV ctl restart --yes`
(it ends any session; restart the stubs). Open Settings with Cmd+, (or the app
menu item `Settings…`); it is a modal card titled `Settings` with the groups
Work items, Sprint, Locks and worktrees, 2ndBrain, Commands, Terminal and
Editor, then `Files` and `About`.

| ID | Check | Steps | Pass | Result |
|----|-------|-------|------|--------|
| S1 | Opening, values, marks and the read-only rows (hand check) | Press Cmd+, on the board. Close it (Esc), open a session's terminal, press Cmd+, again. Close it, then open it from the app menu (`Settings…`). While it loads watch the first frame. Compare each value with `$OV ctl settings` and `cat $OVERSEER_CONFIG` | It opens from the board, from a session and from the menu; at first it may show `Reading the settings from the daemon…`. Each row has the key, a value field (a `true`/`false` toggle for the boolean `push` (`brain_writer` is read-only, see below)), then a mark: `from the file` for keys in `config.toml` (`brain_dir`, `beads_dir`, `jira_command`, `ticket_pattern`, `beads_poll_secs`, `jira_poll_secs`, `worktree_root`, ...) and `default` for the rest. A `Reset` link shows only on rows the file sets. Defaults shown: `terminal.font_family` `Menlo`, `terminal.font_size` `13`, `editor.name` `Visual Studio Code`. Read-only rows show their text and a muted note instead of a field: `brain_writer` (`Turning the writer on has a setup checklist, so change it in config.toml.`), `jira_command`, `bd_command`, `sync_command` (`A typo turns this source off, so change it in config.toml.`) and `terminal.ansi` (`The 16 colours are set in config.toml.`). Values match `ctl settings`. Layout is tidy: the card scrolls, no row is cut off or overlapping, the footer buttons `Restart the daemon`, `Close` and `Save` sit at the bottom with the hint `Cmd+S save · Tab next field · Esc close`. `Files` shows the `config.toml` path with an `Open` button, the `targets.toml` path (no `Open` while the file does not exist), and the note below it. `About` shows `App build rig-a` and `Daemon build rig-a, pid <n>, running since HH:MM`, with no notice line when the builds match. | pending |
| S2 | Saving keeps the rest of the file, and the daemon keys wait for a restart | Restore. In Settings change `jql` to `project = DEMO ORDER BY rank` and `lock_idle_mins` to `45`, press Cmd+S. `diff $OVERSEER_HOME/config.toml.orig $OVERSEER_CONFIG`. `$OV ctl settings \| grep -B1 -A1 restart`. Then press Cmd+S again with no new edit | The note under the footer reads `Saved. The daemon uses the new jql and lock_idle_mins after it restarts.` `diff` shows only those two keys added (or changed): the comment line, the `[unknown_table]` table, the key order and the other values are exactly as they were. `ctl settings` shows `  jql = ... (from the file)  restart needed` and the same for `lock_idle_mins`; the rows in the view show `restart needed` in orange. The second Cmd+S says `Nothing to save.` | pending |
| S3 | Terminal and editor keys apply at once | Restore, start `$OV ctl start 2 --cwd $W --label view -- cat`, open its terminal in the app, and leave it open behind. In Settings set `terminal.font_size` to `20` and `terminal.background` to `#203040`, Save, Close. Then set `editor.name` to `TextEdit`, Save. In the terminal type the literal text `/tmp/ov-hand7/config.toml` (the session runs `cat`, which echoes the typed line back; it would not expand a variable) and Cmd+click that path | No restart is asked for: the note reads `Saved.` and the open terminal's text becomes larger and its background `#203040` straight away. The Cmd+click opens the path in TextEdit (`open -a TextEdit <path>`), not in Visual Studio Code. With `editor.name` back to `Visual Studio Code` a path with a line number would open with `code -g`. | pending |
| S4 | Bad values are refused with the exact words and the file is untouched | Restore, `shasum $OVERSEER_CONFIG > /tmp/ov-hand7-cfg.sum`. In Settings try, one at a time with Save each time: `jira_poll_secs` = `2`, `lock_idle_mins` = `0`, `terminal.font_size` = `abc`, `terminal.background` = `red`, `sprint_start` = `2026-13-45`, `ticket_pattern` = `(`, `jql` empty, `backup_mins` = `abc`. Then `shasum -c /tmp/ov-hand7-cfg.sum`. Then `$OV ctl settings set jira_poll_secs 2; $OV ctl settings set nosuch 1; $OV ctl settings set brain_writer true` | The refusals read: `jira_poll_secs must be a whole number from 5 to 3600`; `lock_idle_mins must be a whole number from 1 to 10080`; `terminal.font_size must be a number from 4 to 96`; `terminal.background must be a colour like #1e1e2e`; `sprint_start must be a date like 2026-11-09, or empty`; `ticket_pattern is not a valid pattern: error: unclosed group`; `jql cannot be empty`; `backup_mins must be a whole number from 0 to 240`. `shared_files` and `journal_path` take any text (a bare word is a one-item list), so they are not refusal checks. `shasum -c` prints `OK`: the file is unchanged. On the command line the first line is the same `jira_poll_secs` text, then `no setting nosuch`, then `brain_writer is read-only here; edit config.toml`. A read-only row cannot be edited in the view either. | pending |
| S5 | A hand edit while the view is open | Restore, open Settings, change `lock_idle_mins` to `30` (do not save). In another tab `printf '# edited by hand\n' >> $OVERSEER_CONFIG`. Press Cmd+S. Then also `printf 'x = [\n' >> $OVERSEER_CONFIG` and Close, reopen Settings | Save refuses: `config.toml changed on disk since Settings read it. Close and reopen Settings to see the new text.` and the file keeps your hand edit (no `lock_idle_mins = 30`). With the unparseable line, a Save says `config.toml does not parse, so Settings will not change it:` followed by the parser's message, and the file is not touched. Remove the line afterwards (restore). | pending |
| S6 | A symlinked config.toml stays a link | Restore, then `mv $OVERSEER_CONFIG $OVERSEER_HOME/real-config.toml; ln -s $OVERSEER_HOME/real-config.toml $OVERSEER_CONFIG; $OV ctl restart --yes`. In Settings change `lock_idle_mins` to `45`, Save. `ls -l $OVERSEER_CONFIG; grep lock_idle $OVERSEER_HOME/real-config.toml` | The save works; `$OVERSEER_CONFIG` is still a symlink to `real-config.toml` and the target holds `lock_idle_mins = 45`. Afterwards `rm $OVERSEER_CONFIG; mv $OVERSEER_HOME/real-config.toml $OVERSEER_CONFIG`. | pending |
| S7 | Reset removes the key | Restore, then `$OV ctl settings set lock_idle_mins 45`. In Settings click `Reset` on the `lock_idle_mins` row, then Save (or use `$OV ctl settings reset lock_idle_mins`) | The row goes back to its default value with the mark `default`, no `Reset` link; after the save `grep lock_idle $OVERSEER_CONFIG` finds nothing and the rest of the file is as in `config.toml.orig`; the daemon shows `restart needed` until it restarts. | pending |
| S8 | `targets.toml`: missing, valid, broken, and Open | (a) With no file, open Settings and `$OV ctl settings`. (b) `printf '[[pattern]]\nname = "pushes"\ncommand = '"'"'^git\\s+push\\b'"'"'\ntarget = "custom:pushes/{repo}"\n' > $OVERSEER_TARGETS; $OV ctl restart --yes`, reopen. (c) Replace it with `printf '[[pattern]]\nname = "broken"\ncommand = '"'"'(git'"'"'\ntarget = "custom:x"\n' > $OVERSEER_TARGETS; $OV ctl restart --yes`, reopen. (d) Click `Open` beside `targets.toml`. Remove the file afterwards | (a) The note reads `6 built-in patterns; /tmp/ov-hand7/targets.toml does not exist` and the `targets.toml` line has no `Open` button; `ctl settings` prints the same after `targets.toml:`. (b) `7 patterns in use, read from /tmp/ov-hand7/targets.toml at daemon start`, with an `Open` button. (c) The note area shows, in red, the load error starting with `/tmp/ov-hand7/targets.toml: pattern "broken": bad command regex:` and `ctl settings` prints a `targets.toml error:` line (the note line itself may still say `N patterns in use`; that wording is the code's, record it if it reads oddly). (d) `Open` opens `/tmp/ov-hand7/targets.toml` in the editor (`open -t`); if that fails the status reads `Could not open <path>: <error>`. The same `Open` beside `config.toml` opens the rig file. | pending |
| S9 | Restart the daemon from Settings | Restore, make S1 `demo lane` and S3 `docs` running. Make a pending edit (S2) first so `restart needed` shows. In Settings click `Restart the daemon` | A question appears: `Restart the daemon? 2 running sessions end and come back as resumable: S1 demo lane, S3 docs.` with `Cancel` and `Restart`; Esc cancels, Enter confirms. (With one session: `Restart the daemon? 1 running session ends and comes back as resumable: S1 demo lane.`; with none: `Restart the daemon? No session is running.`) After `Restart` the note reads `Restarting the daemon`, then the app reconnects on its own, the status line reads `Restarted the daemon (build rig-a to rig-a).`, the `restart needed` marks are gone, `ctl status` shows S1 and S3 `resumable`, and the board agrees. | pending |
| S10 | The restart question when the daemon cannot list its sessions | Open Settings. In the terminal `$OV ctl shutdown`. Click `Restart the daemon`. Press `Cancel`. Then `$OV ctl status` to bring the daemon back | The app shows `Lost the connection to the daemon. Reconnecting…` in the status line, and the question reads `Restart the daemon? Its sessions could not be listed, so some may be running. Each one that is ends and comes back as resumable.` (the fail-closed wording, the same case as U6 for the CLI). `Cancel` closes the question and does nothing else. | pending |
| S11 | Close and Esc with unsaved edits ask first | Open Settings, change `lock_idle_mins` to `45`, press Esc. Press Esc again. Open again, change it, click `Close`, click `Close` again. Open again, change it and press Cmd+S, then Close | The first Esc does not close: a note says `Esc again closes without saving`. The second Esc closes without saving (the file is unchanged). With the `Close` button the first click says `Close again closes without saving` and the second closes. After a save, Close and Esc close at once with nothing asked. | pending |
| S12 | Layout and clicks (hand check) | In the open view: press Tab through the fields (it moves to the next field in order), click into a text field and type, click a `true`/`false` toggle (the `push` row), click `Reset`, press Cmd+S, click `Save`, click `Close`, use the scroll wheel over the card | Tab moves focus field by field and never into a read-only row; typing edits the focused field only; the toggle flips on click and marks the view dirty; `Save` and Cmd+S do the same thing and show `Saving…` in the hint while the daemon writes; the card scrolls; clicks hit the control under the pointer, not one beside it; the focus ring or caret is visible. If the daemon is too old to show settings, the card says `This daemon is older than the app and cannot show its settings. Restart it to edit them here.` (not run here, see Known limitations). | pending |
| S13 | `ctl settings` | `$OV ctl settings`, `$OV ctl settings --json \| head`, `$OV ctl settings set lock_idle_mins 45; $OV ctl settings`, `$OV ctl settings reset lock_idle_mins` | The first prints `config.toml: /tmp/ov-hand7/config.toml`, then each group's keys as `  <key> = <value>  (from the file)` or `(default)`, `  read-only` on the read-only keys and `  restart needed` after a daemon-key change, then the `targets.toml:` line. `--json` is valid JSON. `set` then `reset` change only that key in the file (`diff` against `config.toml.orig` is empty after the reset). | pending |

## The board header

| ID | Check | Steps | Pass | Result |
|----|-------|-------|------|--------|
| H1 | The sprint name and day | (a) Restore, restart, look at the board header. (b) `$OV ctl settings set sprint_start $(date -v-mon +%F)`; `$OV ctl settings set sprint_days 10`; `$OV ctl restart --yes`. (c) `$OV ctl settings set sprint_name "sprint-demo"`; restart. (d) `$OV ctl settings set sprint_start 2026-11-23`; restart. (e) `$OV ctl settings set sprint_start 2026-10-26`; restart. (f) `ctl settings set` refuses a bad date (S4), so the log line is reachable only by a hand edit: `{ printf 'sprint_start = "20-11-2026"\n'; cat $OVERSEER_HOME/config.toml.orig; } > $OVERSEER_CONFIG`; `$OV ctl restart --yes`; read `$OVERSEER_HOME/logs/daemon.log \| tail`. Also `$OV ctl items --json \| grep -o '"sprint":[^}]*'`. Restore afterwards | (a) The header reads `Overseer` and the sprint text `sprint-26-01` (the newest `Sprints/sprint-*` folder). (b) With the start on the last Monday (`2026-11-09`, and today Saturday the 14th) it reads `sprint-26-01, day 5 of 10` (a weekend shows the last weekday reached; a start on a weekend begins on the next Monday). (c) `sprint_name` overrides the folder: `sprint-demo, day 5 of 10`. (d) A start in the future reads `sprint-demo, starts Mon 23 Nov`. (e) A sprint that has ended reads `sprint-demo, ended Fri 6 Nov` (the Friday ending the tenth working day; adjust the dates if your sprint days are not 10). (f) A bad date logs `bad sprint_start "20-11-2026": want YYYY-MM-DD; the header shows the sprint name alone` and the header shows the name alone. `ctl items --json` carries a `sprint` field with the same text. | pending |

## Known flake to watch

| ID | Check | Steps | Pass | Result |
|----|-------|-------|------|--------|
| X1 | The full suite, three times | In a fresh Terminal tab (or a subshell) with the rig left behind: `CLEAN_PATH=$(printf '%s' "$PATH" \| tr ':' '\n' \| grep -v ov-hand7 \| paste -sd: -)`, then run each cargo command as `env -u OVERSEER_CONFIG -u OVERSEER_TARGETS -u OVERSEER_SLOT -u OV -u V -u R -u WT -u W -u RH -u OVERSEER_LAUNCHCTL PATH="$CLEAN_PATH" OVERSEER_HOME=$(mktemp -d) cargo test --workspace`, three times, each with a fresh `OVERSEER_HOME`. The real `CARGO_HOME` and `RUSTUP_HOME` stay set | Three clean runs: `test result: ok` for every crate, no failures. The known flake is the 200 ms limits in `hook.rs` and the server test `permission_prompt_never_gets_a_relay`; one failure there on a loaded Mac that passes on a rerun is recorded, anything else is a finding. | pending |

## Nothing leaked

| ID | Check | Steps | Pass | Result |
|----|-------|-------|------|--------|
| Z1 | The real vault, `CURRENT_SESSION.md`, `~/Repos/.worktrees`, the installed app, its agent and its config are untouched, and no rig agent is left | After every other row, after U8, before the cleanup (do not edit the real vault during the run): `git -C ~/Repos/2ndBrain status --short \| diff - /tmp/ov-hand7-real-status.before`; `git -C ~/Repos/2ndBrain log -3 --format='%h %s' \| diff - /tmp/ov-hand7-real-log.before`; `diff -q <(cp -L ~/.claude/CURRENT_SESSION.md /dev/stdout) /tmp/ov-hand7-real-cs.before`; `ls -laR ~/Repos/.worktrees 2>&1 \| diff - /tmp/ov-hand7-real-wt.before`; `launchctl list 2>/dev/null \| awk '$3 == "uk.co.techize.overseer"' \| diff - /tmp/ov-hand7-real-agent.before`; `find ~/Library/Application\ Support/Overseer -type f 2>/dev/null \| sort \| diff - /tmp/ov-hand7-real-state.before`; `find ~/Library/Application\ Support/Overseer -type f -newer /tmp/ov-hand7-real-marker 2>/dev/null`; `shasum ~/Applications/Overseer.app/Contents/MacOS/overseer ~/Applications/Overseer.app/Contents/Info.plist 2>&1 \| diff - /tmp/ov-hand7-real-app.before`; `readlink ~/.local/bin/overseer 2>&1 \| diff - /tmp/ov-hand7-real-link.before`; `shasum ~/Library/LaunchAgents/uk.co.techize.overseer.plist 2>&1 \| diff - /tmp/ov-hand7-real-plist.before`; `shasum ~/.config/overseer/config.toml ~/.config/overseer/targets.toml 2>&1 \| diff - /tmp/ov-hand7-real-cfg.before`; `launchctl list \| grep -c overseer-home-`; `ls ~/Library/LaunchAgents \| grep -c overseer-home` | Every `diff` prints nothing; the `find -newer` prints nothing; the two counts print `0` (no rig agent loaded, nothing named `overseer-home` under the real `~/Library/LaunchAgents`). The real agent line, app, link, plist and `~/.config/overseer` files are exactly as in Step 0. | pending |

## Known limitations

- **Daemon settings apply only after a restart.** The view and `ctl settings`
  mark them `restart needed`; the terminal and editor keys apply at once
  (S3). The restart ends the running sessions, so the app asks first (S9).
- **`brain_writer` and the command lists are read-only in the view.**
  `brain_writer`, `jira_command`, `bd_command` and `sync_command` are edited in
  `config.toml` by hand (a typo turns a source off, and the writer has a setup
  checklist).
- **`terminal.ansi` is set in the file only.** The 16 colours are not in the
  Settings view (spec 7.9 says the terminal theme is configurable; the colours
  beyond font, size, background and foreground are not).
- **The Locks view cannot remove a waiter.** It releases a holder (the next
  waiter takes over); a waiter leaves by releasing from its own session or by
  its session ending.
- **A waiter-only target is not reachable through normal paths.** L2 builds one
  by writing the database with the daemon stopped.
- **The sprint day comes from `sprint_start`, not Jira.** Without it the
  header shows the name alone; weekdays are counted, no holidays.
- **An isolated home's agent is not loaded at login.** Only the default home's
  agent is installed as a login item; a custom `OVERSEER_HOME` agent is for
  tests and is removed by `make uninstall`.
- **`launchctl kickstart -k` reruns the loaded plist**, not a newer one on
  disk; the install runs `install-agent` which reloads it, but a hand
  `kickstart` does not.
- **The old-daemon path is not hand-checked.** A daemon older than the
  `DaemonInfo` reply (`daemon  running, build unknown (older than this app)`,
  and Settings' `This daemon is older than the app and cannot show its
  settings. Restart it to edit them here.`) needs an old build to set up;
  the unit tests cover it, no row here does.
- **The two keep messages differ.** `install-agent` ends with `Run overseer ctl
  restart when you are ready to finish the upgrade.`; `ctl restart` itself
  points to `--yes`.
- **Hand-off relays to a slot with no session** are queued as already dropped,
  and delivered relays are hidden in `ctl locks`.
- **The Locks view's ages refresh on events and key presses**, not on a timer.

## Sign-off

Phase 7 is done when every Result above reads as passed. Record the date and
any failures here.

- Signed off by: (Jonathan, once the rows pass)
- Date:
- Still pending: every row
- Findings:
