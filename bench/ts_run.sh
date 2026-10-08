#!/usr/bin/env bash
# Treesitter-engine benchmark: fresh nvim process per (engine, file) so the
# original's history-dependent uuid-LRU degradation does not skew results.
set -euo pipefail
cd "$(dirname "$0")/.."
REPO=$(pwd)
ORIG=${ORIG:-/mnt/data/repos/vim-matchup}
SITE=${SITE:-/mnt/data/local/share/nvim/site}
FILES=${FILES:-"nested_vim_10k.vim nested_lua_10k.lua nested_c_10k.c"}
ITERS=${ITERS:-15}
OUTDIR=${OUTDIR:-/tmp/tsbench}
mkdir -p "$OUTDIR"

for f in $FILES; do
  for e in rs orig; do
    if [ "$e" = "rs" ]; then
      RTP="$REPO,$REPO/after,$SITE"
    else
      RTP="$ORIG,$ORIG/after,$SITE"
    fi
    echo "== $e / $f =="
    timeout 900 nvim --headless --clean -u NONE \
      --cmd "set noswapfile" \
      --cmd "let g:matchup_treesitter_enabled = v:true" \
      --cmd "filetype plugin on" \
      --cmd "set rtp+=$RTP" \
      --cmd "let g:engine = '$e'" \
      --cmd "let g:repo = '$REPO'" \
      --cmd "let g:bench_file = '$f'" \
      --cmd "let g:bench_iters = $ITERS" \
      --cmd "let g:bench_out = '$OUTDIR/${e}_${f}.json'" \
      -c "luafile $REPO/bench/ts_driver.lua"
  done
done

echo "== aggregate =="
python3 bench/ts_aggregate.py "$OUTDIR"
