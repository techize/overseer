# Overseer

A macOS app (GPUI GUI plus a background daemon) for hosting Claude Code sessions.
The daemon owns up to seven Claude Code sessions (S0 to S6). The app shows them,
and quitting or closing the app never stops a session.

## Install (no admin rights needed)

You need the Xcode Command Line Tools (`xcode-select --install`). The full
Xcode app is not needed: GPUI is built with `runtime_shaders`.

```
git clone <repo> ~/overseer
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

With `OVERSEER_HOME` set, the install gets its own agent: the label is
`uk.co.techize.overseer-home-<hash of the path>` and its plist lives in
`$OVERSEER_HOME/agent`, not in `~/Library/LaunchAgents`, so it never replaces
or stops the default agent. Its logs go to `$OVERSEER_HOME/logs`. Such an agent
is not loaded at login.

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
agent instead, and leaves `$OVERSEER_HOME` in place.

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
