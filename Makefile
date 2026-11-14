.PHONY: build test lint install uninstall

build:
	cargo build --workspace

test:
	cargo test --workspace

lint:
	cargo fmt --check
	cargo clippy --workspace -- -D warnings

# make install YES=1 restarts the daemon without asking; NO_RESTART=1 keeps it.
install:
	YES="$(YES)" NO_RESTART="$(NO_RESTART)" sh scripts/install.sh

uninstall:
	sh scripts/uninstall.sh
