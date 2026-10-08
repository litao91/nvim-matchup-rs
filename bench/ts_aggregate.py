#!/usr/bin/env python3
"""Aggregate treesitter benchmark JSONs into a markdown section."""
import glob
import json
import os
import sys

OPS = ['ts_current', 'ts_matching', 'ts_highlight']


def main():
    outdir = sys.argv[1]
    data = {}
    cold = {}
    for path in sorted(glob.glob(os.path.join(outdir, '*.json'))):
        with open(path) as fh:
            d = json.load(fh)
        data[(d['engine'], d['file'])] = {r['op']: r for r in d['results']}
        cold[(d['engine'], d['file'])] = d.get('cold_ms')

    files = sorted({f for (_, f) in data})
    out = []
    out.append('## Treesitter engine (pure-Rust port vs vim.treesitter)')
    out.append('')
    out.append('Both engines run with `g:matchup_treesitter_enabled = '
               'v:true`, offscreen rendering disabled, in a fresh headless '
               'process per engine and file (the original\'s uuid-LRU cache '
               'degrades its own results in long sessions). The Rust side '
               'loads grammar parsers from the runtimepath via `libloading` '
               'and parses with the `tree-sitter` crate - no '
               '`vim.treesitter` API calls. `cold first call` includes the '
               'initial full parse (Rust) vs nvim\'s incremental '
               'LanguageTree parse (original); warm ops dominate interactive '
               'use.')
    out.append('')
    for f in files:
        rs = data.get(('rs', f), {})
        og = data.get(('orig', f), {})
        if not rs or not og:
            continue
        out.append(f'### {f}')
        out.append('')
        cr, co = cold.get(('rs', f)), cold.get(('orig', f))
        if cr is not None and co is not None:
            out.append(f'cold first call: orig {co:,.1f} ms, rust {cr:,.1f} ms')
            out.append('')
        out.append('| op | orig median (ms) | rust median (ms) | speedup '
                   '| orig p95 (ms) | rust p95 (ms) |')
        out.append('|---|---:|---:|---:|---:|---:|')
        for op in OPS:
            a, b = og.get(op), rs.get(op)
            if not a or not b:
                continue
            sp = a['median'] / b['median'] if b['median'] > 0 else float('inf')
            out.append(
                f"| {op} | {a['median']:,.2f} | {b['median']:,.2f} "
                f"| {sp:,.1f}x | {a['p95']:,.2f} | {b['p95']:,.2f} |")
        out.append('')
    print('\n'.join(out))


if __name__ == '__main__':
    sys.exit(main())
