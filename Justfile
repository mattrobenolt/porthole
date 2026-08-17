# porthole tasks. just is a command runner, not a build system.

# Run the full local CI battery.
ci: fmt lint test smokes

# Format check.
fmt:
    cargo fmt --check

# Clippy, both feature sets.
lint:
    cargo clippy
    cargo clippy --no-default-features

# Unit tests.
test:
    cargo test

# The system rigs.
smokes:
    bash scripts/smoke.sh
    bash scripts/smoke-tunnel.sh
    bash scripts/smoke-e2e.sh

# Audit GitHub Actions workflows.
lint-actions:
    zizmor --format=plain --min-confidence=medium .github

# Verify every action reference is SHA-pinned with a version comment.
pinact-check:
    pinact run --check --verify --min-age 3

# Pin/update action SHAs and version comments.
pinact:
    pinact run --verify --min-age 3
