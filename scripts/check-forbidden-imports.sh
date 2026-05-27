#!/bin/bash
# Check for forbidden imports that break deterministic simulation.
#
# This script MUST be run in CI on every PR. Any crate above `runtime/`
# that uses these imports breaks the deterministic simulation foundation.
#
# Forbidden patterns:
# - std::time::Instant::now() or SystemTime::now()
# - std::fs (except in runtime/src/real.rs)
# - std::thread::spawn or std::thread::sleep (except in runtime/)
# - rand::random or similar
# - tokio::time or tokio::fs directly

set -e

RED='\033[0;31m'
GREEN='\033[0;32m'
NC='\033[0m' # No Color

ERRORS=0

echo "Checking for forbidden imports that break deterministic simulation..."
echo ""

# Crates that must NOT use these imports
CHECKED_CRATES="storage learned mvcc kv sim"

for crate in $CHECKED_CRATES; do
    crate_dir="crates/$crate/src"

    if [ ! -d "$crate_dir" ]; then
        continue
    fi

    # Check for std::time::Instant::now() or SystemTime::now()
    if grep -rn "Instant::now()" "$crate_dir" 2>/dev/null | grep -v "//"; then
        echo -e "${RED}ERROR: $crate uses Instant::now() - use env.now() instead${NC}"
        ERRORS=$((ERRORS + 1))
    fi

    if grep -rn "SystemTime::now()" "$crate_dir" 2>/dev/null | grep -v "//"; then
        echo -e "${RED}ERROR: $crate uses SystemTime::now() - use env.now() instead${NC}"
        ERRORS=$((ERRORS + 1))
    fi

    # Check for std::fs usage
    if grep -rn "std::fs::" "$crate_dir" 2>/dev/null | grep -v "//"; then
        echo -e "${RED}ERROR: $crate uses std::fs - use env file operations instead${NC}"
        ERRORS=$((ERRORS + 1))
    fi

    if grep -rn "use std::fs" "$crate_dir" 2>/dev/null | grep -v "//"; then
        echo -e "${RED}ERROR: $crate imports std::fs - use env file operations instead${NC}"
        ERRORS=$((ERRORS + 1))
    fi

    # Check for std::thread::spawn
    if grep -rn "std::thread::spawn" "$crate_dir" 2>/dev/null | grep -v "//"; then
        echo -e "${RED}ERROR: $crate uses std::thread::spawn - use env.spawn() instead${NC}"
        ERRORS=$((ERRORS + 1))
    fi

    if grep -rn "thread::spawn" "$crate_dir" 2>/dev/null | grep -v "//" | grep -v "std::thread::spawn"; then
        echo -e "${RED}ERROR: $crate uses thread::spawn - use env.spawn() instead${NC}"
        ERRORS=$((ERRORS + 1))
    fi

    # Check for std::thread::sleep
    if grep -rn "thread::sleep" "$crate_dir" 2>/dev/null | grep -v "//"; then
        echo -e "${RED}ERROR: $crate uses thread::sleep - use env.sleep() instead${NC}"
        ERRORS=$((ERRORS + 1))
    fi

    # Check for rand crate usage
    if grep -rn "rand::random" "$crate_dir" 2>/dev/null | grep -v "//"; then
        echo -e "${RED}ERROR: $crate uses rand::random - use env.rand_u64() instead${NC}"
        ERRORS=$((ERRORS + 1))
    fi

    if grep -rn "rand::thread_rng" "$crate_dir" 2>/dev/null | grep -v "//"; then
        echo -e "${RED}ERROR: $crate uses rand::thread_rng - use env.rand_u64() instead${NC}"
        ERRORS=$((ERRORS + 1))
    fi

    if grep -rn "rand::Rng" "$crate_dir" 2>/dev/null | grep -v "//"; then
        echo -e "${RED}ERROR: $crate uses rand::Rng - use env.rand_u64() instead${NC}"
        ERRORS=$((ERRORS + 1))
    fi

    # Check for tokio direct usage
    if grep -rn "tokio::time::" "$crate_dir" 2>/dev/null | grep -v "//"; then
        echo -e "${RED}ERROR: $crate uses tokio::time - use env methods instead${NC}"
        ERRORS=$((ERRORS + 1))
    fi

    if grep -rn "tokio::fs::" "$crate_dir" 2>/dev/null | grep -v "//"; then
        echo -e "${RED}ERROR: $crate uses tokio::fs - use env methods instead${NC}"
        ERRORS=$((ERRORS + 1))
    fi

    # Check for std::sync::Mutex (should use parking_lot per ADR-002)
    if grep -rn "std::sync::Mutex" "$crate_dir" 2>/dev/null | grep -v "//"; then
        echo -e "${RED}ERROR: $crate uses std::sync::Mutex - use parking_lot::Mutex instead (ADR-002)${NC}"
        ERRORS=$((ERRORS + 1))
    fi

    if grep -rn "std::sync::RwLock" "$crate_dir" 2>/dev/null | grep -v "//"; then
        echo -e "${RED}ERROR: $crate uses std::sync::RwLock - use parking_lot::RwLock instead (ADR-002)${NC}"
        ERRORS=$((ERRORS + 1))
    fi
done

echo ""

if [ $ERRORS -gt 0 ]; then
    echo -e "${RED}Found $ERRORS forbidden import(s)!${NC}"
    echo ""
    echo "The deterministic simulation requires all nondeterminism to go through the Env trait."
    echo "Please refactor to use env.now(), env.sleep(), env.spawn(), env.rand_*(), and env file operations."
    exit 1
else
    echo -e "${GREEN}No forbidden imports found.${NC}"
    exit 0
fi
