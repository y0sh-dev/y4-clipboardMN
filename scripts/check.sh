#!/usr/bin/env bash
set -euo pipefail

echo "==> [1/2] Running tests (cargo test, all workspace members)..."
cargo test --workspace --all-targets

echo "==> [2/2] Running strict linter (cargo clippy, all workspace members)..."
cargo clippy --workspace --all-targets -- -D warnings

echo "==> ALL CHECKS PASSED SUCCESSFULLY <=="

