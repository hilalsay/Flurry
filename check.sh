#!/usr/bin/env bash
# CI gate: formatting, lints, tests. Run before every commit.
set -euo pipefail
cd "$(dirname "$0")"
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
