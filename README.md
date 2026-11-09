# Overseer

A macOS app (GPUI GUI plus a background daemon) for hosting Claude Code sessions.
The daemon owns up to seven Claude Code sessions (S0 to S6). The app shows them,
and quitting or closing the app never stops a session.

## Install (no admin rights needed)

You need the Xcode Command Line Tools (`xcode-select --install`) and a Rust
toolchain installed for your user. If `cargo` is missing, `make install` prints
the rustup command to run. The full Xcode app is not needed: GPUI is built with
`runtime_shaders`.

```
git clone <repo> ~/overseer
cd ~/overseer
make install
```

`make install`:

1. builds the release binary (`cargo build --release -p overseer`),
2. assembles `~/Applications/Overseer.app` with `scripts/bundle.sh` and signs it
   ad hoc (`codesign --sign -`),
3. links `~/.local/bin/overseer` to the binary inside the app,
4. installs and starts the LaunchAgent `uk.co.techize.overseer`
   (`~/Library/LaunchAgents/uk.co.techize.overseer.plist`), which runs the daemon.

Running it again is safe: it replaces the app and reloads the agent.

The agent records the `PATH` of the shell you ran `make install` from, so the
daemon finds `claude` (for example in `/opt/homebrew/bin`). If you move `claude`
somewhere new, run `make install` again.

Open Overseer from `~/Applications` in Finder or with
`open ~/Applications/Overseer.app`. Put `~/.local/bin` on your `PATH` to use the
`overseer` command.

## Upgrade

```
git pull && make install
```

Warning: this restarts the daemon, and every running session ends. Save your
work in each Claude session first. Resuming sessions after a restart arrives in
phase 2.

## Uninstall

```
make uninstall
```

This stops the daemon (ending any sessions), removes the LaunchAgent, the
`~/.local/bin/overseer` link and `~/Applications/Overseer.app`. It leaves the
state dir `~/Library/Application Support/Overseer/` and the logs in place;
delete those by hand if you want them gone.

## Daemon

- `overseer ctl status` lists the slots. Any `overseer ctl` or `overseer attach` command (or opening the
  app) starts the daemon if it is not running.
- `overseer ctl shutdown` stops the daemon and every session. The daemon then
  stays down until the next `overseer ctl` or `overseer attach` command or app launch starts it again.
  launchd only restarts it by itself if it crashes.
- Logs live in `~/Library/Logs/Overseer/`: `daemon.log` (the daemon's own log),
  `daemon.out.log` and `daemon.err.log` (its stdout and stderr under launchd)
  and `gui.log` (the app). Set `RUST_LOG` (for example `RUST_LOG=debug`) to change
  the level; the default is `info`.

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
