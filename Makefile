# arec — macOS system audio + microphone recorder
#
#   make            build the release binary (same as `make build`)
#   make help       list all targets
#
# Override install location with PREFIX, e.g. `make install PREFIX=/usr/local`.

BIN      := arec
PREFIX   ?= $(HOME)/.local
BINDIR   := $(PREFIX)/bin
CARGO    ?= cargo
RELEASE  := target/release/$(BIN)
DEBUG    := target/debug/$(BIN)
SOURCES  := $(wildcard src/*.rs) Cargo.toml Cargo.lock build.rs Info.plist

# Extra arguments for `make run`, e.g. `make run ARGS=--help`.
ARGS ?=

.DEFAULT_GOAL := build
.PHONY: help build debug run run-debug test lint fmt fmt-check check install uninstall clean

help: ## Show this help
	@echo "Usage: make [target] [PREFIX=...] [ARGS=...]"
	@echo
	@grep -E '^[a-zA-Z_-]+:.*## ' $(MAKEFILE_LIST) | \
		awk 'BEGIN {FS = ":.*## "}; {printf "  \033[36m%-11s\033[0m %s\n", $$1, $$2}'

build: $(RELEASE) ## Build the optimized release binary

$(RELEASE): $(SOURCES)
	$(CARGO) build --release

debug: ## Build the debug binary
	$(CARGO) build

run: $(RELEASE) ## Run the release binary (pass flags via ARGS=...)
	./$(RELEASE) $(ARGS)

run-debug: ## Run the debug binary with diagnostics (AREC_DEBUG=1)
	$(CARGO) build
	AREC_DEBUG=1 ./$(DEBUG) $(ARGS)

test: ## Run unit tests
	$(CARGO) test

lint: ## Run clippy with warnings treated as errors
	$(CARGO) clippy --all-targets -- -D warnings

fmt: ## Format the code
	$(CARGO) fmt

fmt-check: ## Check formatting without modifying files
	$(CARGO) fmt --check

check: fmt-check lint test ## Run all checks (format, lint, tests)

install: $(RELEASE) ## Install to PREFIX/bin (default: ~/.local/bin)
	@mkdir -p "$(BINDIR)"
	install -m 755 $(RELEASE) "$(BINDIR)/$(BIN)"
	@echo "Installed $(BINDIR)/$(BIN)"
	@case ":$$PATH:" in *":$(BINDIR):"*) ;; \
		*) echo "Note: $(BINDIR) is not on your PATH" ;; esac

uninstall: ## Remove the installed binary from PREFIX/bin
	rm -f "$(BINDIR)/$(BIN)"

clean: ## Remove build artifacts
	$(CARGO) clean
