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
    total = 0
    for k in keys:
        if k not in rs or k not in og:
            continue
        total += 1
        for op in ('cur', 'nxt', 'prv', 'mat', 'sur'):
            a, b = rs[k].get(op), og[k].get(op)
            if a != b:
                mism[op] += 1
                if len(examples[op]) < 8:
                    examples[op].append((k, a, b))

    print(f'positions compared: {total}')
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
