#!/usr/bin/env python3
"""Aggregate benchmark JSON dumps into bench/RESULTS.md."""
import json
import math
import platform
import subprocess
import sys
from collections import defaultdict

OPS = ['matching_outer', 'matching_middle', 'surrounding_deep',
       'highlight', 'motion_unmatched']


def load(path):
    with open(path) as fh:
        return json.load(fh)


def cpu_model():
    try:
        out = subprocess.run(
            ['sh', '-c',
             "grep -m1 'model name' /proc/cpuinfo | cut -d: -f2"],
            capture_output=True, text=True, timeout=10)
        return out.stdout.strip() or platform.processor()
    except Exception:
        return platform.processor()


def fmt_ms(v):
    return f'{v:,.2f}'


def main():
    rs = load(sys.argv[1])
    og = load(sys.argv[2])

    def index(payload):
        d = {}
        for r in payload['results']:
            d[(r['file'], r['op'])] = r
        return d

    rs_i, og_i = index(rs), index(og)
    files = []
    for (f, _) in rs_i:
        if f not in files:
            files.append(f)
    files.sort()

    nv = rs.get('nvim', {})
    nvim_ver = f"{nv.get('major')}.{nv.get('minor')}.{nv.get('patch')}" \
               + (f"-{nv.get('prerelease')}" if nv.get('prerelease') else '')

    out = []
    out.append('# nvim-matchup-rs benchmarks')
    out.append('')
    out.append('Classic-engine Rust rewrite (nvim-oxi + fancy-regex) vs the '
               'original vim-matchup vimscript engine, measured in two '
               'separate headless nvim instances over identical buffers.')
    out.append('')
    out.append('## Environment')
    out.append('')
    out.append(f'- neovim {nvim_ver} (headless, `--clean -u NONE`)')
    out.append(f'- cpu: {cpu_model()}')
    out.append(f'- os: {platform.system()} {platform.release()}')
    out.append('- rust: release profile (lto, opt-level 3, codegen-units 1)')
    out.append('- treesitter engine disabled on both sides '
               '(`g:matchup_treesitter_enabled = 0`); offscreen rendering '
               'disabled (`g:matchup_matchparen_offscreen = {}`) so the '
               'highlight op measures the core cycle')
    out.append('- syntax highlighting off in headless runs: `synID()` is '
               'empty, so `b:match_skip` syntax filtering is inactive on '
               'both sides; this isolates engine regex/scan performance')
    out.append('')
    out.append('## Method')
    out.append('')
    out.append('- fixtures (`bench/gen.py`): synthetic nested vim/C/lua at '
               '~2k/10k/50k lines - one outer block containing S sibling '
               'nested blocks plus a cursor-target line - and a real 50k-line '
               'vimscript file (concatenated nvim runtime syntax files)')
    out.append('- ops, each one end-to-end call per iteration:')
    out.append('  - `matching_outer` / `matching_middle`: get_current + '
               'get_matching at the outermost / a middle delimiter '
               '(full-file depth scan)')
    out.append('  - `surrounding_deep`: get_surrounding from the target '
               'line inside the outermost block (walks back past every '
               'sibling block)')
    out.append('  - `highlight`: full matchparen cycle incl. extmark writes '
               '(cursor alternates between two positions per iteration)')
    out.append('  - `motion_unmatched`: `[%`-style motion from the target '
               'line (cursor reset each iteration, outside the timed region)')
    out.append('- 1 warm-up iteration, then 50 (2k files) / 30 (10k) / 15 '
               '(50k and real file) timed iterations via `vim.uv.hrtime()`; '
               'median and p95 reported')
    out.append('- raw engine ops run with the timeout budget disabled on '
               'both sides (`matchup#perf#timeout_start(0)`); highlight and '
               'motion use their normal budgets')
    out.append('')

    speedups = defaultdict(list)
    for f in files:
        out.append(f'## {f}')
        out.append('')
        out.append('| op | orig median (ms) | rust median (ms) | speedup '
                   '| orig p95 (ms) | rust p95 (ms) |')
        out.append('|---|---:|---:|---:|---:|---:|')
        for op in OPS:
            a = og_i.get((f, op))
            b = rs_i.get((f, op))
            if not a or not b:
                continue
            sp = a['median'] / b['median'] if b['median'] > 0 else float('inf')
            speedups[op].append(sp)
            out.append(
                f"| {op} | {fmt_ms(a['median'])} | {fmt_ms(b['median'])} "
                f"| {sp:,.1f}x | {fmt_ms(a['p95'])} | {fmt_ms(b['p95'])} |")
        out.append('')

    out.append('## Summary (median speedup, geometric mean over files)')
    out.append('')
    out.append('| op | speedup |')
    out.append('|---|---:|')
    all_sp = []
    for op in OPS:
        sps = [s for s in speedups.get(op, []) if math.isfinite(s)]
        if not sps:
            continue
        gm = math.exp(sum(math.log(s) for s in sps) / len(sps))
        all_sp.extend(sps)
        out.append(f'| {op} | {gm:,.1f}x |')
    if all_sp:
        gm = math.exp(sum(math.log(s) for s in all_sp) / len(all_sp))
        out.append(f'| **overall** | **{gm:,.1f}x** |')
    out.append('')

    rs_times = rs.get('times')
    if rs_times:
        out.append('## Appendix: Rust engine internal timings '
                   '(EMA / last / max, ms)')
        out.append('')
        out.append('```')
        if isinstance(rs_times, dict):
            for k in sorted(rs_times):
                v = rs_times[k]
                if isinstance(v, dict):
                    out.append(f"{k:<44} {v.get('emavg', 0):>9.3f} "
                               f"{v.get('last', 0):>9.3f} "
                               f"{v.get('maximum', 0):>9.3f}")
                else:
                    out.append(f'{k}: {v}')
        else:
            out.append(str(rs_times))
        out.append('```')
        out.append('')

    print('\n'.join(out))


if __name__ == '__main__':
    sys.exit(main())
