.PHONY: build test lint

build:
	cargo build --workspace

test:
	cargo test --workspace

lint:
	cargo fmt --check
	cargo clippy --workspace -- -D warnings
