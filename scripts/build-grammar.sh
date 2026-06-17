#!/usr/bin/env bash
# Build a tree-sitter grammar to a .wasm that is ABI-compatible with the
# `tree-sitter` crate this project links against.
#
# The grammar .wasm must be built with a tree-sitter CLI whose version matches
# the runtime crate version, otherwise loading fails with errors like
# "failed to parse dylink section". This script installs a matching CLI and
# builds the grammar via the tree-sitter toolchain (which fetches a wasi-sdk on
# first run).
#
# Usage:
#   scripts/build-grammar.sh <npm-grammar-package> [output-dir]
#
# Examples:
#   scripts/build-grammar.sh tree-sitter-rust ./grammars
#   scripts/build-grammar.sh tree-sitter-python ./grammars
#
# Requires: npm, and either a local emscripten or Docker (the CLI uses one to
# compile the grammar to wasm).
set -euo pipefail

PKG="${1:?usage: build-grammar.sh <npm-grammar-package> [output-dir]}"
OUT_DIR="${2:-./grammars}"

# Keep this in sync with the `tree-sitter` version in the crate Cargo.toml files.
TS_CLI_VERSION="0.26"

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT_DIR="$(mkdir -p "$OUT_DIR" && cd "$OUT_DIR" && pwd)"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

echo "==> installing tree-sitter-cli@^${TS_CLI_VERSION} and ${PKG}"
cd "$WORK"
npm init -y >/dev/null 2>&1
npm install --no-save "tree-sitter-cli@^${TS_CLI_VERSION}" "${PKG}" >/dev/null 2>&1

CLI="$WORK/node_modules/.bin/tree-sitter"
GRAMMAR_DIR="$WORK/node_modules/${PKG}"
OUT_WASM="$OUT_DIR/${PKG}.wasm"

echo "==> tree-sitter $("$CLI" --version)"
echo "==> building ${PKG} -> ${OUT_WASM}"
"$CLI" build --wasm "$GRAMMAR_DIR" -o "$OUT_WASM"

echo "==> done: ${OUT_WASM}"
