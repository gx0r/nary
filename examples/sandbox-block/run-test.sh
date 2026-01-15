#!/bin/bash
# Test that the nary sandbox blocks access to sensitive files

set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
NARY_BIN="${NARY_BIN:-$SCRIPT_DIR/../../target/release/nary}"

# Check nary binary exists
if [ ! -x "$NARY_BIN" ]; then
    echo "Error: nary binary not found at $NARY_BIN"
    echo "Build with: cargo build --release"
    exit 1
fi

# Get the sandbox profile from nary itself (single source of truth)
PROFILE=$("$NARY_BIN" sandbox-profile --project "$SCRIPT_DIR")

echo "Running sandbox test..."
echo ""

# Run test script inside sandbox
sandbox-exec -p "$PROFILE" node "$SCRIPT_DIR/test-sandbox.js"
