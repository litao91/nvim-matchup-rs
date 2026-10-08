#!/usr/bin/env bash
# Benchmark runner: two separate headless nvim instances (one per engine),
# identical fixtures and op sequences. Writes /tmp/bench_{rs,orig}.json.
set -euo pipefail
cd "$(dirname "$0")/.."
REPO=$(pwd)
ORIG=${ORIG:-/mnt/data/repos/vim-matchup}
OUT_RS=${OUT_RS:-/tmp/bench_rs.json}
OUT_ORIG=${OUT_ORIG:-/tmp/bench_orig.json}

NVIM_FLAGS=(--headless --clean -u NONE
  --cmd "set noswapfile"
  --cmd "let g:matchup_treesitter_enabled = 0"
  --cmd "filetype plugin on")

echo "== rust engine =="
timeout 3600 nvim "${NVIM_FLAGS[@]}" \
  --cmd "set rtp+=$REPO" \
  --cmd "let g:engine = 'rs'" \
  --cmd "let g:repo = '$REPO'" \
  --cmd "let g:bench_out = '$OUT_RS'" \
  -c "luafile $REPO/bench/driver.lua"

echo "== original engine =="
timeout 3600 nvim "${NVIM_FLAGS[@]}" \
  --cmd "set rtp+=$ORIG" \
  --cmd "let g:engine = 'orig'" \
  --cmd "let g:repo = '$REPO'" \
  --cmd "let g:bench_out = '$OUT_ORIG'" \
  -c "luafile $REPO/bench/driver.lua"

echo "== aggregate =="
python3 bench/aggregate.py "$OUT_RS" "$OUT_ORIG" > bench/RESULTS.md
cat bench/RESULTS.md
