#!/usr/bin/env python3
"""kun: runs upstream's GC tests with elise as the collector.

The tests the `gc_engine()` helper's engine runs (`tests/all/main.rs`) take
elise as their collector with `WASMTIME_TEST_COLLECTOR=elise`, which checks
the heap after every GC, and `elise-stress`, which GCs at every allocation
too. Those elise can't pass yet are listed in
`kun/elise-expected-failures.txt`, each with why; this runs the modules
both ways, and fails if a test outside the list fails, or one on it passes
(take it off the list).

    python3 kun/elise_tests.py [extra cargo test filters...]
"""

import os
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
LIST = ROOT / 'kun' / 'elise-expected-failures.txt'
# The modules whose tests use `gc_engine()` or `gc_store()`.
MODULES = ['structs::', 'arrays::', 'gc::', 'exnrefs::', 'eqref::']
RESULT = re.compile(r'^test (\S+)(?: - should panic)? \.\.\. (ok|FAILED|ignored)', re.M)


def expected():
    names = {}
    for line in LIST.read_text(encoding='utf-8').splitlines():
        line = line.strip()
        if not line or line.startswith('#'):
            continue
        name, _, why = line.partition('#')
        names[name.strip()] = why.strip()
    return names


def run(collector, filters):
    """Whether the run with `collector` failed."""
    env = dict(os.environ, WASMTIME_TEST_COLLECTOR=collector)
    command = ['cargo', 'test', '--features', 'gc-elise', '--test', 'all', '--', *filters]
    run = subprocess.run(command, cwd=ROOT, env=env, capture_output=True, text=True,
                         encoding='utf-8', errors='replace')
    output = run.stdout + run.stderr
    results = dict(RESULT.findall(output))
    if not results:
        print(output)
        print('no test ran (did the binary crash?)')
        return True
    listed = expected()
    unexpected = sorted(n for n, r in results.items() if r == 'FAILED' and n not in listed)
    fixed = sorted(n for n, r in results.items() if r == 'ok' and n in listed)
    failed = sum(r == 'FAILED' for r in results.values())
    passed = sum(r == 'ok' for r in results.values())
    print(f'{collector}: {passed} passed, {failed} failed ({failed - len(unexpected)} expected)')
    for name in unexpected:
        print(f'unexpected failure: {name}')
    for name in fixed:
        print(f'passes now, take it off the list: {name}')
    if 'test result:' not in output:
        print(output[-4000:])
        print('the test binary didn\'t finish (crashed?)')
        return True
    return bool(unexpected or fixed)


def main():
    filters = sys.argv[1:] or MODULES
    failed = False
    for collector in ['elise', 'elise-stress']:
        failed |= run(collector, filters)
    sys.exit(1 if failed else 0)


if __name__ == '__main__':
    main()
