#!/bin/bash
# Download and run TLC (TLA+ model checker)
# Usage: ./run-tlc.sh SPEC.tla [-config SPEC.cfg]

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TLC_JAR="$SCRIPT_DIR/tla2tools.jar"
TLC_URL="https://github.com/tlaplus/tlaplus/releases/download/v1.8.0/tla2tools.jar"

# Download TLC if not present
if [ ! -f "$TLC_JAR" ]; then
    echo "Downloading TLA+ tools..."
    curl -L -o "$TLC_JAR" "$TLC_URL"
    echo "Downloaded to $TLC_JAR"
fi

# Check arguments
if [ $# -lt 1 ]; then
    echo "Usage: $0 SPEC.tla [-config SPEC.cfg]"
    echo "Example: $0 MVCC.tla -config MVCC.cfg"
    exit 1
fi

SPEC="$1"
shift

# Run TLC
echo "Running TLC on $SPEC..."
cd "$SCRIPT_DIR"
java -XX:+UseParallelGC -Xmx4g -jar "$TLC_JAR" "$SPEC" "$@"
