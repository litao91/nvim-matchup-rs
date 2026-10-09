#!/usr/bin/env bash
# Smoke tests for the highlight / motion / offscreen paths that tests/diff skips
# (tests/diff runs with matchparen disabled). Builds + deploys the .so, then
# drives a headless nvim asserting observable behavior. Exits nonzero on any
# failed check (the driver quits with :cq).
set -euo pipefail
cd "$(dirname "$0")/../.."
REPO=$(pwd)
SITE=${SITE:-/mnt/data/local/share/nvim/site}

echo "== build =="
cargo build --release
cp target/release/libmatchup_rs.so lua/matchup_rs.so

echo "== smoke =="
nvim --headless --clean -u NONE \
  --cmd "set noswapfile" \
  --cmd "let g:matchup_treesitter_enabled = v:false" \
  --cmd "let g:repo = '$REPO'" \
  --cmd "let g:site = '$SITE'" \
  -c "luafile $REPO/tests/smoke/driver.lua"
