#!/usr/bin/env bash
# Smoke tests for the highlight / motion / offscreen paths that tests/diff skips
# (tests/diff runs with matchparen disabled). Builds + deploys the .so, then
# drives a headless nvim asserting observable behavior - once with the regex
# engine and once with the treesitter engine. Exits nonzero on any failed check
# (the driver quits with :cq).
set -euo pipefail
cd "$(dirname "$0")/../.."
REPO=$(pwd)
SITE=${SITE:-/mnt/data/local/share/nvim/site}

echo "== build =="
cargo build --release
cp target/release/libmatchup_rs.so lua/matchup_rs.so

run_smoke() { # $1 = g:ts, $2 = label, $3 = g:matchup_treesitter_enabled
  echo "== smoke ($2) =="
  nvim --headless --clean -u NONE \
    --cmd "set noswapfile" \
    --cmd "let g:matchup_treesitter_enabled = $3" \
    --cmd "let g:repo = '$REPO'" \
    --cmd "let g:site = '$SITE'" \
    --cmd "let g:ts = $1" \
    -c "luafile $REPO/tests/smoke/driver.lua"
}

run_smoke 0 "regex engine" "v:false"
run_smoke 1 "treesitter engine" "v:true"
