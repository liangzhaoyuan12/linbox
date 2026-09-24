#!/bin/bash
# linbox 质量门（GOAL.md 4.4）：fmt + clippy(-D warnings) + test
# 用法: ./check.sh          全量
#       ./check.sh quick    跳过 fmt（开发中快速回归）
set -e
cd "$(dirname "$0")"

if [ "$1" != "quick" ]; then
    echo "▶ cargo fmt --check"
    cargo fmt --check
fi

echo "▶ cargo clippy --all-targets -- -D warnings"
cargo clippy --all-targets -- -D warnings

echo "▶ cargo test"
cargo test

echo "✓ check 通过"
