.PHONY: help build release release-fast install install-fast uninstall \
        test test-integration test-all test-coverage test-phase0-gate \
        test-phase1-gate test-phase3-gate test-phase4-gate test-phase5-gate \
        check check-deps check-file-size clippy doc fmt fmt-check ci ci-quick init doctor clean \
        loc watch bench

CARGO := cargo
CLI_CRATE := crates/brehon-cli
NATIVE_AGENT_CRATE := crates/brehon-native-agent
BIN := brehon
NATIVE_AGENT_BIN := brehon-native-agent

# ==============================================================================
# Help
# ==============================================================================

help: ## Show this help menu
	@printf "\033[1mBrehon — Multi-agent orchestration platform\033[0m\n"
	@printf "\n"
	@printf "\033[36mQuick Start:\033[0m\n"
	@printf "  make build        # Build debug binary\n"
	@printf "  make test         # Run unit tests\n"
	@printf "  make ci-quick     # Quick CI check\n"
	@printf "\n"
	@printf "\033[36mUsage:\033[0m\n"
	@printf "  make <target>\n"
	@printf "\n"
	@printf "\033[36mBuild:\033[0m\n"
	@printf "  build            — Build in debug mode\n"
	@printf "  release          — Build optimized release binary (full LTO)\n"
	@printf "  release-fast     — Build release with thin LTO (faster compile)\n"
	@printf "\n"
	@printf "\033[36mInstall:\033[0m\n"
	@printf "  install          — Install binaries to ~/.cargo/bin\n"
	@printf "  install-fast     — Install with thin LTO profile\n"
	@printf "  uninstall        — Remove installed binaries\n"
	@printf "\n"
	@printf "\033[36mTesting:\033[0m\n"
	@printf "  test             — Run unit tests\n"
	@printf "  test-integration — Run integration tests\n"
	@printf "  test-all         — Run all tests (unit + integration)\n"
	@printf "  test-coverage    — Run tests with coverage report\n"
	@printf "  test-phase0-gate — Run the Phase 0 stability gate harness\n"
	@printf "  test-phase1-gate — Run the Phase 1 stability gate harness\n"
	@printf "  test-phase3-gate — Run the Phase 3 stability gate harness\n"
	@printf "  test-phase4-gate — Run the Phase 4 stability gate harness\n"
	@printf "  test-phase5-gate — Run the Phase 5 stability gate harness\n"
	@printf "\n"
	@printf "\033[36mQuality:\033[0m\n"
	@printf "  check            — Fast compilation check\n"
	@printf "  check-deps       — Check Brehon crate dependency boundaries\n"
	@printf "  check-file-size  — Check Rust file-size guard baseline\n"
	@printf "  clippy           — Run clippy with warnings as errors\n"
	@printf "  doc              — Build documentation\n"
	@printf "  fmt              — Format source code\n"
	@printf "  fmt-check        — Check code formatting\n"
	@printf "\n"
	@printf "\033[36mCI:\033[0m\n"
	@printf "  ci               — Run full CI pipeline with guard checks\n"
	@printf "  ci-quick         — Run quick CI with guard checks\n"
	@printf "\n"
	@printf "\033[36mDevelopment:\033[0m\n"
	@printf "  init             — Initialize new Brehon project\n"
	@printf "  doctor           — Run diagnostics\n"
	@printf "  watch            — Watch for changes and rebuild/test\n"
	@printf "  bench            — Run benchmarks\n"
	@printf "\n"
	@printf "\033[36mMaintenance:\033[0m\n"
	@printf "  clean            — Remove build artifacts\n"
	@printf "  loc              — Count lines of code\n"
	@printf "\n"
	@printf "\033[33mRun 'make <target>' to execute.\033[0m\n"

# ==============================================================================
# Build
# ==============================================================================

## build          — Build in debug mode
build:
	$(CARGO) build --package brehon-cli --package brehon-native-agent

## release        — Build optimized release binary (full LTO)
release:
	$(CARGO) build --package brehon-cli --package brehon-native-agent --release

## release-fast   — Build release with thin LTO (faster compile)
release-fast:
	$(CARGO) build --package brehon-cli --package brehon-native-agent --profile release-fast

# ==============================================================================
# Install
# ==============================================================================

## install        — Install binaries to ~/.cargo/bin
install:
	$(CARGO) install --locked --path $(CLI_CRATE)
	$(CARGO) install --locked --path $(NATIVE_AGENT_CRATE)

## install-fast   — Install with thin LTO profile
install-fast:
	$(CARGO) install --locked --path $(CLI_CRATE) --profile release-fast
	$(CARGO) install --locked --path $(NATIVE_AGENT_CRATE) --profile release-fast

## uninstall      — Remove installed binaries
uninstall:
	-$(CARGO) uninstall $(BIN)
	-$(CARGO) uninstall $(NATIVE_AGENT_BIN)

# ==============================================================================
# Testing
# ==============================================================================

## test           — Run unit tests
test:
	$(CARGO) test --workspace --lib

## test-integration — Run integration tests
test-integration:
	$(CARGO) test --workspace --test '*' -- --test-threads=1

## test-all       — Run all tests (unit + integration)
test-all: test test-integration

## test-coverage  — Run tests with coverage report
test-coverage:
	$(CARGO) llvm-cov --workspace --lcov --output-path lcov.info

## test-phase0-gate — Run the Phase 0 stability gate harness
test-phase0-gate:
	./scripts/phase0_stability_gate.sh

## test-phase1-gate — Run the Phase 1 stability gate harness
test-phase1-gate:
	./scripts/phase1_stability_gate.sh

## test-phase3-gate — Run the Phase 3 stability gate harness
test-phase3-gate:
	./scripts/phase3_stability_gate.sh

## test-phase4-gate — Run the Phase 4 stability gate harness
test-phase4-gate:
	./scripts/phase4_stability_gate.sh

## test-phase5-gate — Run the Phase 5 stability gate harness
test-phase5-gate:
	./scripts/phase5_stability_gate.sh

# ==============================================================================
# Quality
# ==============================================================================

## check          — Fast compilation check
check:
	$(CARGO) check --workspace

## check-deps     — Check Brehon crate dependency boundaries
check-deps:
	./scripts/check-dependency-boundaries.sh

## check-file-size — Check Rust file-size guard baseline
check-file-size:
	./scripts/check-file-size.sh

## clippy         — Run clippy with warnings as errors
clippy:
	$(CARGO) clippy --workspace -- -D warnings

## doc            — Build documentation
doc:
	RUSTDOCFLAGS="-D warnings" $(CARGO) doc --workspace --no-deps

## fmt            — Format source code
fmt:
	$(CARGO) fmt --all

## fmt-check      — Check code formatting
fmt-check:
	$(CARGO) fmt --all -- --check

# ==============================================================================
# CI
# ==============================================================================

## ci             — Run full CI pipeline (fmt, guard checks, check, clippy, doc, test)
ci: fmt-check check-deps check-file-size check clippy doc test

## ci-quick       — Run quick CI (guard checks, check, clippy, test)
ci-quick: check-deps check-file-size check clippy test

# ==============================================================================
# Development
# ==============================================================================

## init           — Initialize new Brehon project
init:
	$(CARGO) run --package brehon-cli -- init

## doctor         — Run diagnostics
doctor:
	$(CARGO) run --package brehon-cli -- doctor

## watch          — Watch for changes and rebuild/test
watch:
	$(CARGO) watch -x build -x test

## bench          — Run benchmarks
bench:
	$(CARGO) bench

# ==============================================================================
# Maintenance
# ==============================================================================

## clean          — Remove build artifacts
clean:
	$(CARGO) clean
	rm -f lcov.info

## loc            — Count lines of code
loc:
	@tokei --type=Rust 2>/dev/null || find . -name '*.rs' -not -path './target/*' | xargs wc -l
