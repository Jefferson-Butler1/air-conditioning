.PHONY: help build check install

help: ## Show this help
	@grep -E '^[a-zA-Z0-9_-]+:.*##' $(MAKEFILE_LIST) | awk -F ':.*## ' '{printf "  \033[36m%-10s\033[0m %s\n", $$1, $$2}'

build: ## Build the release binary
	cargo build --release

check: ## Lint, format check, and test
	cargo clippy --all-targets -- -D warnings
	cargo fmt --check
	cargo test

install: ## Install to ~/.cargo/bin
	cargo install --path . --locked
