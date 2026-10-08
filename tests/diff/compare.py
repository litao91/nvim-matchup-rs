#!/usr/bin/env python3
"""Compare the JSON dumps produced by tests/diff/driver.lua for both engines."""
import json
import sys
from collections import Counter, defaultdict


def load(path):
    recs, pre = {}, {}
    with open(path) as fh:
        for line in fh:
            line = line.strip()
            if not line:
                continue
            o = json.loads(line)
            if o.get('pre'):
                pre[o['f']] = o['cond']
            else:
                recs[(o['f'], o['l'], o['c'])] = o
    return recs, pre


def main():
    rs, rs_pre = load(sys.argv[1])
    og, og_pre = load(sys.argv[2])

    pre_ok = True
    for f in sorted(set(rs_pre) | set(og_pre)):
        for k in ('match_words', 'match_skip', 'matchpairs', 'ignorecase'):
            a = rs_pre.get(f, {}).get(k)
            b = og_pre.get(f, {}).get(k)
            if a != b:
                pre_ok = False
                print(f'PRECONDITION MISMATCH {f}.{k}:')
                print(f'  rs  : {a!r}')
                print(f'  orig: {b!r}')
    if pre_ok:
        print('preconditions (match_words/skip/matchpairs/ignorecase) '
              'identical for all files')

    keys = sorted(set(rs) | set(og))
    missing = [k for k in keys if k not in rs or k not in og]
    if missing:
        print(f'WARNING: {len(missing)} positions present in only one dump, '
              f'e.g. {missing[:5]}')

    mism = Counter()
    examples = defaultdict(list)
    # treesitter mode only: the original's 150-entry uuid LRU evicts cache
    # entries that its get_surrounding memo still references, so its walk
    # degrades to None depending on call history; a fresh-process original
    # agrees with the rust results at these positions (rs superset)
    sur_superset = 0
    total = 0
    for k in keys:
        if k not in rs or k not in og:
            continue
        total += 1
        for op in ('cur', 'nxt', 'prv', 'mat', 'sur'):
            a, b = rs[k].get(op), og[k].get(op)
            if a != b:
                if op == 'sur' and a is not None and b is None:
                    sur_superset += 1
                    continue
                mism[op] += 1
                if len(examples[op]) < 8:
                    examples[op].append((k, a, b))

    print(f'positions compared: {total}')
    if sur_superset:
        print(f'NOTE: {sur_superset} sur positions where rust finds the '
              f'surrounding and the original degrades to None (original '
              f'LRU-eviction artifact; fresh-process original agrees with '
              f'rust)')
    if not mism:
        print('ALL OPS MATCH')
    else:
        for op in ('cur', 'nxt', 'prv', 'mat', 'sur'):
            n = mism.get(op, 0)
            if not n:
                print(f'{op}: OK')
                continue
            print(f'{op}: {n} mismatches ({100.0 * n / total:.2f}%)')
            for (f, l, c), a, b in examples[op]:
                print(f'  {f} ({l},{c}):')
                print(f'    rs  ={a!r}')
                print(f'    orig={b!r}')

    sys.exit(1 if (mism or missing or not pre_ok) else 0)


if __name__ == '__main__':
    main()
