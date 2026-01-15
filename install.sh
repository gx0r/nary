#!/bin/bash
set -e

echo "Installing nary..."
cargo install --path nary_bin

echo ""
echo "Installed! To enable backtraces on errors, set:"
echo "  export RUST_BACKTRACE=1"
