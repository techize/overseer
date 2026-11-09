# Overseer

A macOS app (GPUI GUI plus a background daemon) for hosting Claude Code sessions.

## Development

Requires a Rust toolchain and the Xcode Command Line Tools. GPUI is built with
`runtime_shaders`, so the full Xcode install (and `xcrun metal`) is not needed.

```
make build
make test
make lint
cargo run -p overseer
```
