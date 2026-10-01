#!/usr/bin/env bash
set -euo pipefail

echo "==> [1/2] Running tests (cargo test)..."
cargo test --all-targets

echo "==> [2/2] Running strict linter (cargo clippy)..."
cargo clippy --all-targets -- -D warnings

echo "==> ALL CHECKS PASSED SUCCESSFULLY <=="

