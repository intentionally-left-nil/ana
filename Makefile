.DEFAULT_GOAL := help

CARGO ?= cargo

.PHONY: help
help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | sort | awk 'BEGIN {FS = ":.*?## "}; {printf "\033[36m%-16s\033[0m %s\n", $$1, $$2}'

.PHONY: build
build: ## Build all workspace crates
	$(CARGO) build --workspace --all-targets

.PHONY: test
test: ## Run all tests
	$(CARGO) test --workspace --all-targets

.PHONY: fmt
fmt: ## Format code
	$(CARGO) fmt --all

.PHONY: fmt-check
fmt-check: ## Check formatting without making changes
	$(CARGO) fmt --all -- --check

.PHONY: clippy
clippy: ## Run clippy lints, treating warnings as errors
	$(CARGO) clippy --workspace --all-targets -- -D warnings

.PHONY: lint
lint: fmt-check clippy ## Run all lint checks (fmt-check + clippy)

.PHONY: check
check: ## Type-check the workspace without building artifacts
	$(CARGO) check --workspace --all-targets

.PHONY: doc
doc: ## Build documentation
	$(CARGO) doc --workspace --no-deps

.PHONY: clean
clean: ## Remove build artifacts
	$(CARGO) clean

.PHONY: ci
ci: lint test ## Run everything CI runs (lint + test)
