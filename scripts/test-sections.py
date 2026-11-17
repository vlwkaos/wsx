#!/usr/bin/env python3
"""scripts/sections.py names a failure by its section and by the last step that passed before it.
Run: python3 -B scripts/test-sections.py. A section that ends at an `ok` is named by that `ok`, so a
browser check in an `if` that fails well before it must not read as the section's own step
(the `post levels` line of the web scenario on CI, #482)."""
import sys
import tempfile
from pathlib import Path

import sections

ROOT = Path(__file__).resolve().parent.parent
SOURCE = '''
def early():
    try:
        assert 1 == 2, 'FAIL: before any step passed\\nsecond line'
        ok('never early')
    finally:
        pass


def journey():
    try:
        ok('first step')
        if True:
            ok('inner step')
            assert 1 == 2, 'FAIL: To check on a phone: the sheet does not open in one motion\\nframes follow'
            ok('never inner')
        ok('post levels')
    finally:
        pass
'''

work = ROOT / '.work'
work.mkdir(exist_ok=True)
with tempfile.TemporaryDirectory(dir=work) as folder:
    script = Path(folder) / 'journey.py'
    script.write_text(SOURCE)
    scope = {'ok': sections.passed}
    sections.sectioned(scope, str(script), ('early', 'journey'), end='ok')
    for name in ('early', 'journey'):
        scope[name]()

early, journey = sections.failed
# ^ A failure before any step passed names none.
assert early.startswith('FAILED section early never early at journey.py:') and 'last step that passed' not in early, early
# ^ The line shows the first FAIL line only (#488), the section that ends at `post levels`, and the step that passed last.
assert journey.startswith('FAILED section journey post levels at journey.py:'), journey
assert journey.endswith('AssertionError: FAIL: To check on a phone: the sheet does not open in one motion [last step that passed: inner step]'), journey
assert 'frames follow' not in journey, journey
print('ok: sections name the last step that passed')
sys.exit(0)
