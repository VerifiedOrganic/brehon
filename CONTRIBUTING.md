# Contributing to Brehon

First, the honest framing: **Brehon is a solo project that I built around my
own workflow and have been running on real work daily for months.** I'm happy
people are interested enough to look at the code, and I'm open to
contributions, but it's worth knowing what you're walking into before you
spend time on a PR.

**What's likely to land easily:**

- Bug fixes with a failing test that becomes a passing test.
- Adapter fixes, especially for Junie (which I haven't used) and for the
  adapters I've put less mileage on. I've run every adapter except Junie
  at some point, but my day-to-day driving tends to favor a subset, so
  patches from people who lean on the others — with a repro — are
  genuinely valuable.
- Documentation corrections, typos, broken examples.
- Targeted performance fixes with a benchmark to back them up.
- Small, contained features that don't change the shape of the system.

**What's likely to bounce, or at least get a long discussion first:**

- Anything that changes the lane / panel / scoring model. Those shapes are
  what they are because I've tried five versions of this and these are the
  ones that survived. I'm not closed to changing them, but the bar is high
  and the discussion comes before the code.
- Refactors that "clean up" or restructure crates without a concrete
  problem they're solving. The hexagonal layout is intentional and load-bearing.
- New configuration options that exist to support a workflow other than
  the one Brehon is built around. I'd rather keep the surface small.
- Backwards-compatibility shims for unreleased shapes. This is pre-1.0;
  things move.

**Response times will be uneven.** I have a day job and this isn't it. If a
PR sits for a while, it's not personal — ping the issue.

If you're about to spend more than an hour on something non-trivial, open an
issue first and let's talk about whether the change makes sense. That's not
a gate, it's a kindness to your time.

## Code of Conduct

This project follows the [Rust Code of Conduct](https://www.rust-lang.org/policies/code-of-conduct). By participating in this project, you agree to uphold this code. Please report unacceptable behavior to the project maintainers.

## Getting Started

### Prerequisites

- **Rust 1.70+** (stable toolchain recommended)
- **git** for version control
- A basic understanding of async Rust and the tokio runtime

### Fork and Clone

1. Fork the repository on GitHub
2. Clone your fork locally:
   ```bash
   git clone https://github.com/YOUR_USERNAME/brehon.git
   cd brehon
   ```
3. Add the upstream repository:
   ```bash
   git remote add upstream https://github.com/original/brehon.git
   ```

### Build

```bash
cargo build
```

Run tests:

```bash
cargo test
cargo test --workspace
```

## Development Workflow

### Create a Branch

Create a feature branch from `main`:

```bash
git checkout -b feature/your-feature-name
```

Use descriptive branch names:
- `feature/add-mcp-tool` for new features
- `fix/review-deadlock` for bug fixes
- `refactor/storage-layer` for refactoring
- `docs/api-reference` for documentation

### Make Changes

1. Write code following the [Code Style](#code-style) guidelines
2. Ensure all tests pass
3. Add tests for new functionality
4. Update documentation if needed

### Run Tests

```bash
# Run all tests
cargo test --workspace

# Run tests for a specific crate
cargo test -p brehon-orchestrator

# Run ignored tests (ACP conformance tests require real agents)
cargo test --workspace -- --include-ignored
```

### Submit a Pull Request

1. Push your branch to your fork
2. Open a pull request against `main`
3. Link to any relevant issues
4. Wait for review

## Code Style

### Formatting

Follow `rustfmt` defaults:

```bash
cargo fmt
```

### Linting

Run Clippy with warnings as errors:

```bash
cargo clippy -- -D warnings
```

The CI pipeline enforces this, so run it locally before committing.

### Documentation

Document all public APIs with `///` doc comments:

```rust
/// Appends an event to the store and updates materialized views.
///
/// This operation is atomic - either all events are appended and
/// all views are updated, or nothing is changed.
///
/// # Arguments
///
/// * `events` - The events to append
/// * `views` - The view updates to apply
///
/// # Returns
///
/// The event IDs of the appended events on success.
///
/// # Errors
///
/// Returns an error if the transaction fails or if any event
/// fails validation.
pub async fn append_atomic(
    &self,
    events: Vec<Event>,
    views: Vec<ViewUpdate>,
) -> Result<Vec<EventId>>;
```

## Testing Requirements

### All New Code Needs Tests

Every new feature, bug fix, or refactoring must include tests:

### Unit Tests

Place unit tests in the same file using `#[cfg(test)]`:

```rust
pub fn calculate_score(verdicts: &[Verdict]) -> f64 {
    // implementation
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_calculate_score_empty_verdicts() {
        let scores: Vec<Verdict> = vec![];
        assert!(calculate_score(&scores).is_nan());
    }

    #[test]
    fn test_calculate_score_single_verdict() {
        let verdicts = vec![Verdict::approve(8)];
        assert_eq!(calculate_score(&verdicts), 8.0);
    }
}
```

### Integration Tests

Place integration tests in the `tests/` directory of each crate:

```
brehon-orchestrator/
  src/
    lib.rs
    task.rs
  tests/
    integration_test.rs
    scenario_test.rs
```

### Test Harness

Use `brehon-test-harness` for mocking:

```rust
use brehon_test_harness::{MockAgentGateway, MockEventStore};

#[tokio::test]
async fn test_task_dispatch() {
    let store = MockEventStore::new();
    let gateway = MockAgentGateway::new();
    
    // Configure mock behavior
    gateway.set_response_delay(Duration::from_millis(100));
    
    let orchestrator = Orchestrator::new(store, gateway);
    let result = orchestrator.dispatch_task(task).await;
    
    assert!(result.is_ok());
}
```

## Architecture Guidelines

### Hexagonal Architecture (Ports/Adapters)

Brehon follows hexagonal architecture with strict dependency rules:

```
┌─────────────────────────────────────────────────────────┐
│                    Composition Root                     │
│                      (brehon-cli)                        │
├─────────────────────────────────────────────────────────┤
│                      Adapters                           │
│  (store-fjall, search-tantivy, acp, git, mcp, tui)     │
├─────────────────────────────────────────────────────────┤
│                 Core Domain (Pure)                      │
│  (types, ports, orchestrator, supervisor, review)      │
└─────────────────────────────────────────────────────────┘
```

### Dependency Rules

1. **Core crates** (`brehon-types`, `brehon-ports`, `brehon-orchestrator`, `brehon-supervisor`, `brehon-review`, `brehon-config`) depend ONLY on:
   - `brehon-types`
   - `brehon-ports`

2. **Adapter crates** (`brehon-store-fjall`, `brehon-search-tantivy`, `brehon-acp`, `brehon-git`, `brehon-mcp`, `brehon-tui`) depend on:
   - `brehon-ports`
   - `brehon-types`
   - External libraries (fjall, tantivy, etc.)

3. **No circular dependencies** between any crates

4. **No adapter-to-adapter dependencies**

### Port Traits

Define interfaces in `brehon-ports`:

```rust
#[async_trait]
pub trait EventStore: Send + Sync {
    async fn append(&self, event: Event) -> Result<EventId>;
    async fn query(&self, filter: EventFilter) -> Result<Vec<Event>>;
}

#[async_trait]
pub trait AgentGateway: Send + Sync {
    async fn spawn(&self, spec: SessionSpec) -> Result<SessionId>;
    async fn send_prompt(&self, session: &SessionId, prompt: PromptTurn) -> Result<PromptHandle>;
}
```

### Adapters Implement Port Traits

Implement port traits in adapter crates:

```rust
// In brehon-store-fjall
pub struct FjallEventStore {
    db: fjall::Keyspace,
}

#[async_trait]
impl EventStore for FjallEventStore {
    async fn append(&self, event: Event) -> Result<EventId> {
        // Implementation using fjall
    }
}
```

### Testing with Mocks

The `brehon-test-harness` crate provides mock implementations for all ports:

```rust
use brehon_test_harness::mock::{MockEventStore, MockAgentGateway};

#[tokio::test]
async fn test_with_mocks() {
    let store = MockEventStore::new();
    let gateway = MockAgentGateway::with_behavior(MockBehavior {
        response_delay: Duration::from_millis(10),
        ..Default::default()
    });
}
```

## Commit Messages

Follow [Conventional Commits](https://www.conventionalcommits.org/):

```
<type>(<scope>): <description>

[optional body]

[optional footer]
```

Types:
- `feat`: New feature
- `fix`: Bug fix
- `refactor`: Code refactoring
- `test`: Adding or updating tests
- `docs`: Documentation changes
- `chore`: Maintenance tasks

Examples:

```
feat(orchestrator): add task dependency resolution

Task dependencies are now resolved before dispatch. Circular
dependencies are detected and rejected with a clear error message.

Closes #123
```

```
fix(review): handle reviewer death during panel review

When a reviewer process dies mid-review, the coordinator now
respawns the entire panel to maintain panel affinity.
```

## Pull Request Process

### Before Submitting

1. Rebase on latest main:
   ```bash
   git fetch upstream
   git rebase upstream/main
   ```

2. Run all checks:
   ```bash
   cargo fmt --check
   cargo clippy -- -D warnings
   cargo test --workspace
   ```

3. Ensure commits follow [Conventional Commits](#commit-messages)

### PR Requirements

- Link to relevant issues (e.g., "Closes #123")
- Add tests for new functionality
- Update documentation for API changes
- Keep PRs focused and reasonably sized

### Review Process

1. CI must pass (fmt, clippy, tests). This is non-negotiable.
2. I'll review when I can; see the note up top about response times.
3. Expect feedback. Even good PRs usually get notes — partly taste, partly
   because the system has invariants that aren't always obvious from the
   diff. Treat review comments as a conversation, not a verdict.
4. Merges are squash-and-merge with a rewritten commit message in
   Conventional Commits form (see below).

### Review Score Thresholds

The review system uses score thresholds:
- **8-10**: Approve with minor suggestions
- **6-7**: Non-blocking issues or conditional approval
- **4-5**: Blocking changes required
- **1-3**: Reject — fundamental issues

## Release Process

For maintainers only:

### Version Bumping

1. Update version in `Cargo.toml` files
2. Update `CHANGELOG.md` with changes
3. Create a git tag:
   ```bash
   git tag -a v0.1.0 -m "Release v0.1.0"
   git push origin v0.1.0
   ```

### Publishing

```bash
# Publish crates in dependency order
cargo publish -p brehon-types
cargo publish -p brehon-ports
cargo publish -p brehon-config
cargo publish -p brehon-store-fjall
cargo publish -p brehon-search-tantivy
cargo publish -p brehon-acp
cargo publish -p brehon-git
cargo publish -p brehon-mcp
cargo publish -p brehon-orchestrator
cargo publish -p brehon-supervisor
cargo publish -p brehon-review
cargo publish -p brehon-tui
cargo publish -p brehon-test-harness
cargo publish -p brehon-cli
```

### Crate Dependency Order

1. `brehon-types` — base types
2. `brehon-ports` — port traits
3. `brehon-config` — configuration
4. Storage adapters (`brehon-store-fjall`, `brehon-search-tantivy`)
5. Protocol adapters (`brehon-acp`, `brehon-git`, `brehon-mcp`)
6. Core logic (`brehon-orchestrator`, `brehon-supervisor`, `brehon-review`)
7. Interface (`brehon-tui`)
8. Test harness (`brehon-test-harness`)
9. CLI (`brehon-cli`)

---

## Questions?

Open an issue for:
- Bug reports
- Feature requests
- Documentation improvements
- Questions about the architecture

Thank you for contributing to Brehon!