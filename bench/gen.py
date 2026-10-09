#!/usr/bin/env python3
"""Generate benchmark fixtures into bench/files/ (gitignored).

Synthetic nested vim/C/lua files at ~2k/10k/50k lines plus one real
large vimscript file (concatenated nvim runtime syntax files). Each
synthetic file is one outer block containing S sibling nested blocks
followed by a cursor-target line, so that:

- get_matching at the outer delim scans the whole file (depth counting),
- get_surrounding from the target line walks back past every sibling,
- highlight/motion exercise the same scans end to end.

Also writes bench/files/positions.json:
  { "<file>": {"ft", "lines", "iters", "outer":[l,c], "middle":[l,c],
               "deep":[l,c]} }
"""
import json
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
OUT = os.path.join(HERE, 'files')
RUNTIME = '/mnt/data/local/share/nvim/runtime'

SIZES = [
    ('2k', 2_000, 50),
    ('10k', 10_000, 30),
    # 50k iters kept low: the original engine's surrounding_deep is ~1-89s per
    # call there, so 15 iters alone can exceed an hour; 6 (3 for the real corpus)
    # gives a stable median while keeping a full run bounded.
    ('50k', 50_000, 6),
]


def gen_vim(target):
    per_sibling_fixed = 6  # if/while/try + 3 closers
    s = max(8, target // 100)
    filler = max(1, (target - 5) // s - per_sibling_fixed)
    lines = ['function! Outer() abort', '  for i0 in range(3)']
    mid_sibling = s // 2
    middle = None
    for k in range(s):
        base = len(lines)
        lines.append('    if i0 == %d' % k)
        lines.append('      while 1')
        lines.append('        try')
        if k == mid_sibling:
            middle = (base + 1, 5)  # on the "if" keyword
        for f in range(filler):
            lines.append('          echo "filler %d %d"' % (k, f))
        lines.append('        endtry')
        lines.append('      endwhile')
        lines.append('    endif')
    deep = (len(lines) + 1, 3)
    lines.append('  echo "target"')
    lines.append('  endfor')
    lines.append('endfunction')
    return lines, (1, 1), middle, deep


def gen_c(target):
    per_sibling_fixed = 6
    s = max(8, target // 100)
    filler = max(1, (target - 4) // s - per_sibling_fixed)
    lines = ['int outer(int n) {']
    mid_sibling = s // 2
    middle = None
    for k in range(s):
        base = len(lines)
        lines.append('  if (n == %d) {' % k)
        lines.append('    for (int i = 0; i < 2; i++) {')
        lines.append('      while (n > 0) {')
        if k == mid_sibling:
            middle = (base + 1, 3)
        for f in range(filler):
            lines.append('        int x%d = %d;' % (k, f))
        lines.append('      }')
        lines.append('    }')
        lines.append('  }')
    deep = (len(lines) + 1, 3)
    lines.append('  n = 0;')
    lines.append('  return n;')
    lines.append('}')
    return lines, (1, 1), middle, deep


def gen_lua(target):
    per_sibling_fixed = 6
    s = max(8, target // 100)
    filler = max(1, (target - 4) // s - per_sibling_fixed)
    lines = ['local function outer(n)']
    mid_sibling = s // 2
    middle = None
    for k in range(s):
        base = len(lines)
        lines.append('  if n == %d then' % k)
        lines.append('    while n > 0 do')
        lines.append('      for i = 1, 2 do')
        if k == mid_sibling:
            middle = (base + 1, 3)
        for f in range(filler):
            lines.append('        local x%d = %d' % (k, f))
        lines.append('      end')
        lines.append('    end')
        lines.append('  end')
    deep = (len(lines) + 1, 3)
    lines.append('  local target = 1')
    lines.append('  return target')
    lines.append('end')
    return lines, (1, 1), middle, deep


def gen_real_vim(target):
    """Concatenate runtime syntax files up to `target` lines."""
    lines = []
    d = os.path.join(RUNTIME, 'syntax')
    for name in sorted(os.listdir(d)):
        if not name.endswith('.vim'):
            continue
        with open(os.path.join(d, name), errors='replace') as fh:
            for line in fh:
                lines.append(line.rstrip('\n'))
                if len(lines) >= target:
                    break
        if len(lines) >= target:
            break
    # positions on real delims
    import re
    opener = re.compile(r'^\s*(?:function!?|if|for|while|try)\b')
    first = mid = None
    n = len(lines)
    for i, line in enumerate(lines):
        if opener.match(line):
            if first is None:
                first = (i + 1, len(line) - len(line.lstrip()) + 1)
            if mid is None and i + 1 >= n // 2:
                mid = (i + 1, len(line) - len(line.lstrip()) + 1)
        if first and mid:
            break
    deep = (n * 3 // 4, 1)
    return lines, first or (1, 1), mid or first or (1, 1), deep


GENS = {
    'vim': ('nested_vim_%s.vim', gen_vim),
    'c': ('nested_c_%s.c', gen_c),
    'lua': ('nested_lua_%s.lua', gen_lua),
}


def main():
    os.makedirs(OUT, exist_ok=True)
    positions = {}
    for ft, (namefmt, gen) in GENS.items():
        for label, target, iters in SIZES:
            lines, outer, middle, deep = gen(target)
            name = namefmt % label
            with open(os.path.join(OUT, name), 'w') as fh:
                fh.write('\n'.join(lines) + '\n')
            positions[name] = {
                'ft': ft, 'lines': len(lines), 'iters': iters,
                'outer': outer, 'middle': middle, 'deep': deep,
            }
            print('%-22s %7d lines' % (name, len(lines)))
    lines, outer, middle, deep = gen_real_vim(50_000)
    name = 'real_vim_50k.vim'
    with open(os.path.join(OUT, name), 'w') as fh:
        fh.write('\n'.join(lines) + '\n')
    positions[name] = {
        'ft': 'vim', 'lines': len(lines), 'iters': 3,
        'outer': outer, 'middle': middle, 'deep': deep,
    }
    print('%-22s %7d lines' % (name, len(lines)))
    with open(os.path.join(OUT, 'positions.json'), 'w') as fh:
        json.dump(positions, fh, indent=1)
    print('wrote', os.path.join(OUT, 'positions.json'))


if __name__ == '__main__':
    sys.exit(main())
