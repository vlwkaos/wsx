"""Every failure of one scenario run, not only the first. ref: docs/tasks.md "Scenarios"

`sectioned(scope, path, names, start=..., end=...)` compiles the journey functions `names` of the
script at `path` again, with each top-level section of the function body (or of its first `try`
body) inside its own guard. A section that fails prints its traceback and one `FAILED section`
line, and the run goes on with the next section. A later section that reads a name a failed or
skipped section assigns is skipped and named in one `SKIPPED section` line, because it would read
that section's broken state. A failure after the first is marked "after section <x> failed": it
can be damage the first failure left in the shared store. `report()` prints every line again at
the end and answers whether any section failed.

`start='mark'` splits where a `mark('<name>')` call starts a section (test-task.py);
`end='ok'` splits after each `ok('<label>')` call (test-web-client.py): a section is named by the
`ok` that ends it, and a step that fails inside it (a browser check in an `if`) is well before that
`ok`; the script's `ok` calls `passed(label)`, so a FAILED line also names the last step that passed.
"""
import ast
import os
import sys
import traceback


class RunOver(BaseException):
    """A run past its time limit: no section guard catches it, so the run ends at once."""


failed = []    # the FAILED lines
skipped = []   # the SKIPPED lines
_labels = []   # section index -> label
_needs = []    # section index -> {earlier index: names it reads from that section}
_broken = {}   # section index -> 'failed' or 'skipped'
_first = []    # the label of the first failed section
_passed = []   # the label of the last step that passed (`passed`)


def _call(stmt, name):
    """The call `name(...)` when `stmt` is exactly that statement."""
    if name and isinstance(stmt, ast.Expr) and isinstance(stmt.value, ast.Call) \
            and isinstance(stmt.value.func, ast.Name) and stmt.value.func.id == name:
        return stmt.value
    return None


def _label(call, fallback):
    arg = call.args[0] if call is not None and call.args else None
    if isinstance(arg, ast.Constant) and isinstance(arg.value, str):
        return arg.value[:80]
    return fallback


_SCOPES = (ast.FunctionDef, ast.AsyncFunctionDef, ast.Lambda, ast.ClassDef,
           ast.ListComp, ast.SetComp, ast.DictComp, ast.GeneratorExp)


def _reads(stmts):
    """The names a section reads before it assigns them itself, and the names it assigns, in
    evaluation order (an assignment's value before its target)."""
    reads, stores = set(), set()

    def visit(node, top, bound=frozenset()):
        if isinstance(node, ast.Name):
            if isinstance(node.ctx, ast.Load) and node.id not in stores and node.id not in bound:
                reads.add(node.id)
            elif isinstance(node.ctx, ast.Store) and top:
                stores.add(node.id)
            return
        if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef)) and top:
            stores.add(node.name)
        if isinstance(node, (ast.Assign, ast.AnnAssign, ast.AugAssign, ast.NamedExpr)):
            if isinstance(node, ast.AugAssign) and isinstance(node.target, ast.Name) \
                    and node.target.id not in stores:
                reads.add(node.target.id)
            targets = node.targets if isinstance(node, ast.Assign) else [node.target]
            children = ([node.value] if node.value is not None else []) + targets
        elif isinstance(node, (ast.For, ast.AsyncFor)):
            children = [node.iter, node.target] + node.body + node.orelse
        else:
            children = list(ast.iter_child_nodes(node))
        inner = top and not isinstance(node, _SCOPES)
        if isinstance(node, _SCOPES):
            # ^ A nested function's or comprehension's own names are not the section's reads.
            bound = bound | {n.id for n in ast.walk(node) if isinstance(n, ast.Name) and isinstance(n.ctx, ast.Store)} \
                | {n.arg for n in ast.walk(node) if isinstance(n, ast.arg)}
        for child in children:
            visit(child, inner, bound)

    for stmt in stmts:
        visit(stmt, True)
    return reads, stores


def _split(body, start, end):
    groups, current = [], []
    for stmt in body:
        if _call(stmt, start) and current:
            groups.append(current)
            current = []
        current.append(stmt)
        if _call(stmt, end):
            groups.append(current)
            current = []
    if current:
        groups.append(current)
    return groups


def _guard(index, stmts):
    guard = ast.parse('if SECTIONS.enter(%d):\n    try:\n        pass\n'
                      '    except Exception as SECTION_ERROR:\n        SECTIONS.fail(%d, SECTION_ERROR)\n'
                      % (index, index)).body[0]
    # ^ The guard takes the section's first line; the section's own nodes keep theirs.
    for node in ast.walk(guard):
        if hasattr(node, 'lineno'):
            ast.copy_location(node, stmts[0])
    guard.body[0].body = stmts
    return guard


def sectioned(scope, path, names, start=None, end=None):
    """Compile the functions `names` of `path` into `scope` again, one guard per section."""
    with open(path) as source:
        tree = ast.parse(source.read(), path)
    found = [node for node in tree.body if isinstance(node, ast.FunctionDef) and node.name in names]
    assert len(found) == len(names), 'sectioned: %s not all found in %s' % (names, path)
    for function in found:
        tries = [stmt for stmt in function.body if isinstance(stmt, ast.Try)]
        holder = tries[0] if end and tries else function
        producer = {}
        body = holder.body
        # ^ A docstring stays the function's own.
        guards = [body[0]] if isinstance(body[0], ast.Expr) and isinstance(body[0].value, ast.Constant) else []
        for stmts in _split(body[len(guards):], start, end):
            index = len(_labels)
            boundary = _call(stmts[0], start) if start else _call(stmts[-1], end)
            _labels.append('%s %s' % (function.name, _label(boundary, 'at line %d' % stmts[0].lineno)))
            reads, stores = _reads(stmts)
            needs = {}
            for name in reads & set(producer):
                needs.setdefault(producer[name], set()).add(name)
            _needs.append(needs)
            producer.update((name, index) for name in stores)
            guards.append(_guard(index, stmts))
        holder.body = guards
    module = ast.fix_missing_locations(ast.Module(body=found, type_ignores=[]))
    scope['SECTIONS'] = sys.modules[__name__]
    exec(compile(module, path, 'exec'), scope)


def passed(label):
    """A step of the script passed: a FAILED line after it says so."""
    _passed[:] = [label[:80]]


def enter(index):
    broken = [(earlier, names) for earlier, names in sorted(_needs[index].items()) if earlier in _broken]
    if not broken:
        return True
    earlier, names = broken[0]
    _broken[index] = 'skipped'
    line = 'SKIPPED section %s: it reads %s of section %s, which %s' % (
        _labels[index], ', '.join(sorted(names)), _labels[earlier], _broken[earlier])
    skipped.append(line)
    print(line, file=sys.stderr, flush=True)
    return False


def fail(index, error):
    traceback.print_exception(type(error), error, error.__traceback__, file=sys.stderr)
    frames = traceback.extract_tb(error.__traceback__)
    function = _labels[index].split(' ', 1)[0]
    own = [frame for frame in frames if frame.name == function][:1] or frames[:1]
    where = '%s:%d' % (os.path.basename(own[0].filename), own[0].lineno) if own else '?'
    inner = frames[-1] if frames else None
    if inner is not None and own and inner is not own[0]:
        where += ' (in %s, %s:%d)' % (inner.name, os.path.basename(inner.filename), inner.lineno)
    # A check script's whole output is the message: its first line is info, the cause a later FAIL: line.
    lines = str(error).strip().splitlines() or ['']
    text = next((line for line in lines if line.startswith('FAIL:')), lines[0])[:240]
    first = type(error).__name__ + (': ' + text if text else '')
    after = ' (after section %s failed)' % _first[0] if _first else ''
    if not _first:
        _first.append(_labels[index])
    _broken[index] = 'failed'
    last = ' [last step that passed: %s]' % _passed[0] if _passed else ''
    line = 'FAILED section %s at %s: %s%s%s' % (_labels[index], where, first, after, last)
    failed.append(line)
    print(line, file=sys.stderr, flush=True)


def report():
    """Print every failed and skipped section once more; True when any section failed."""
    if failed or skipped:
        print('SECTIONS: %d failed, %d skipped' % (len(failed), len(skipped)), file=sys.stderr)
        for line in failed + skipped:
            print(line, file=sys.stderr)
        sys.stderr.flush()
    return bool(failed)
