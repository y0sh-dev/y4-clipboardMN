#!/usr/bin/env bash
set -euo pipefail

echo "==> [1/3] Checking code formatting (cargo fmt)..."
cargo fmt --all -- --check

echo "==> [2/3] Running tests (cargo test)..."
cargo test --all-targets

echo "==> [3/3] Running strict linter (cargo clippy)..."
cargo clippy --all-targets -- -D warnings

echo "==> ALL CHECKS PASSED SUCCESSFULLY <=="
