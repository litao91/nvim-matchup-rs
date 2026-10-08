#!/usr/bin/env bash
# Correctness diff harness: run both engines over the sample files at every
# cursor position and compare the JSON dumps.
set -euo pipefail
cd "$(dirname "$0")/../.."
REPO=$(pwd)
ORIG=${ORIG:-/mnt/data/repos/vim-matchup}
SITE=${SITE:-/mnt/data/local/share/nvim/site}
TS=${TS:-0}
SUFFIX=""
TS_CMD="let g:matchup_treesitter_enabled = v:false"
if [ "$TS" = "1" ]; then
  SUFFIX="_ts"
  TS_CMD="let g:matchup_treesitter_enabled = 1"
fi
OUT_RS=${OUT_RS:-/tmp/diff_rs$SUFFIX.jsonl}
OUT_ORIG=${OUT_ORIG:-/tmp/diff_orig$SUFFIX.jsonl}

NVIM_FLAGS=(--headless --clean -u NONE
  --cmd "set noswapfile"
  --cmd "$TS_CMD"
  --cmd "filetype plugin on")

echo "== rust engine =="
timeout 900 nvim "${NVIM_FLAGS[@]}" \
  --cmd "set rtp+=$REPO,$REPO/after,$SITE" \
  --cmd "let g:engine = 'rs'" \
  --cmd "let g:repo = '$REPO'" \
  --cmd "let g:diff_out = '$OUT_RS'" \
  -c "luafile $REPO/tests/diff/driver.lua"

echo "== original engine =="
timeout 900 nvim "${NVIM_FLAGS[@]}" \
  --cmd "set rtp+=$ORIG,$ORIG/after,$SITE" \
  --cmd "let g:engine = 'orig'" \
  --cmd "let g:repo = '$REPO'" \
  --cmd "let g:diff_out = '$OUT_ORIG'" \
  -c "luafile $REPO/tests/diff/driver.lua"

echo "== compare =="
python3 tests/diff/compare.py "$OUT_RS" "$OUT_ORIG"
