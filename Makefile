.PHONY: build test lint install uninstall

APP := $(HOME)/Applications/Overseer.app
APP_BIN := $(APP)/Contents/MacOS/overseer
LINK := $(HOME)/.local/bin/overseer
PLIST := $(HOME)/Library/LaunchAgents/uk.co.techize.overseer.plist

build:
	cargo build --workspace

test:
	cargo test --workspace

lint:
	cargo fmt --check
	cargo clippy --workspace -- -D warnings

install:
	@command -v cargo >/dev/null 2>&1 || { \
		echo "cargo was not found. Install Rust for your user (no admin rights needed):"; \
		echo "  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"; \
		echo "then open a new terminal and run make install again."; \
		exit 1; }
	cargo build --release -p overseer
	scripts/bundle.sh target/release/overseer "$(APP)"
	mkdir -p "$(HOME)/.local/bin"
	ln -sfn "$(APP_BIN)" "$(LINK)"
	"$(APP_BIN)" install-agent
	@echo
	@echo "Overseer is installed."
	@echo "  App:    $(APP)"
	@echo "  CLI:    $(LINK) (add ~/.local/bin to PATH if it is not there)"
	@echo "  Daemon: LaunchAgent uk.co.techize.overseer, logs in ~/Library/Logs/Overseer"
	@echo "Open Overseer from ~/Applications in Finder, or run: open ~/Applications/Overseer.app"

uninstall:
	@if [ -x "$(APP_BIN)" ]; then \
		"$(APP_BIN)" uninstall-agent; \
	else \
		launchctl bootout "gui/$$(id -u)/uk.co.techize.overseer" 2>/dev/null || true; \
		rm -f "$(PLIST)"; \
	fi
	@if [ -L "$(LINK)" ]; then rm -f "$(LINK)"; echo "removed $(LINK)"; fi
	@if [ -d "$(APP)" ]; then rm -r "$(APP)"; echo "removed $(APP)"; fi
	@echo "Overseer is uninstalled. State in ~/Library/Application Support/Overseer and logs in ~/Library/Logs/Overseer are left in place."
