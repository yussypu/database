#!/bin/bash
#
# Smoke test for the benchmark harness.
# Runs all 7 workloads against all 4 backends (28 combinations).
#
# Usage:
#   ./scripts/bench_smoke.sh
#
# Exit code: 0 if all tests pass, 1 if any fail.

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# Build the bench binary
echo "Building bench harness..."
cargo build --release --package crackeddb-bench

# Run smoke tests
echo ""
echo "Running smoke tests (28 combinations)..."
echo ""

"$REPO_ROOT/target/release/bench" smoke --out-dir /tmp/bench_smoke

echo ""
echo "All smoke tests passed!"
