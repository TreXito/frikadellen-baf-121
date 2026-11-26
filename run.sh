#!/bin/bash

# Frikadellen BAF Launcher Script
# This script helps users run the Frikadellen BAF application

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

# Check if the binary exists
if [ -f "target/release/frikadellen_baf" ]; then
    echo "Running frikadellen_baf..."
    exec ./target/release/frikadellen_baf "$@"
elif [ -f "frikadellen_baf" ]; then
    echo "Running frikadellen_baf..."
    exec ./frikadellen_baf "$@"
else
    echo "Binary not found. Building from source..."
    if ! command -v cargo &> /dev/null; then
        echo "Error: cargo is not installed. Please install Rust from https://rustup.rs/"
        exit 1
    fi
    
    echo "Building release binary..."
    cargo build --release
    
    echo "Build complete. Running frikadellen_baf..."
    exec ./target/release/frikadellen_baf "$@"
fi
