#!/bin/bash
# what a module-level instance costs, measured rather than estimated
#
# the interpreted definition runs first and the whole module body runs against it, so a
# value the body builds is built by the *twin* and the compiled type then replaces the
# name. `isinstance(value, TheClass)` is False from there on. this is not a rung — it
# exercises nothing the five sweeps do not — it is the census the decline on that defect
# has to be re-costed against, because the population it is a fraction of moves with
# every commit that changes what gets emitted.
#
# one pass over the corpus, because two sweeps at once invent differences. per module:
#   - compile it, and take the classes whose *name* the compiled module replaces
#   - import the compiled leg and ask which module-level values their class disowns,
#     `shallow` as the module dict alone and `deep` one level into its containers
#   - read the source for the names the module *body* reaches, which is what every
#     candidate static rule is a subset of
#
# usage: instancecensus.sh SP BY PY OUT [MODULE...]
SP="$1"; BY="$2"; PY="$3"; OUT="$4"; shift 4
# shellcheck source=scripts/native-sweeps/sweeplib.sh
. "$(dirname "$0")/sweeplib.sh"
LIB=$(sweep_lib "$PY")
sweep_begin instances || exit 1

cat > "$SWEEP_ROOT/probe.py" <<'PYEOF'
import importlib
import os
import signal
import sys

# the staging says what to import: `m` for a top-level module, `pkg.m` for a package
# member, which is the only name its relative imports resolve against
MOD = os.environ['SWEEP_MOD']
# `by compile` names a module after its file, and an emitted class takes its
# `__module__` from the last component of that name — so it answers `m` where an
# interpreted one answers `pkg.m`. this census reads a compiled leg only, and both
# spellings mean "defined by the module under test"
SELF = (MOD, MOD.rpartition('.')[2])


def _ring(signum, frame):
    raise TimeoutError('timed out')


signal.signal(signal.SIGALRM, _ring)
try:
    signal.alarm(int(os.environ['SWEEP_IMPORT_BOUND']))
    try:
        m = importlib.import_module(MOD)
    finally:
        signal.alarm(0)
except BaseException as error:
    print('IMPORT-FAILED %s' % type(error).__name__)
    raise SystemExit(0)

shallow = set()
deep = set()
counts = [0, 0]


def note(value, bucket, slot):
    kind = type(value)
    if getattr(kind, '__module__', None) not in SELF:
        return
    claimed = getattr(m, kind.__name__, None)
    if not isinstance(claimed, type):
        return
    try:
        if not isinstance(value, claimed):
            bucket.add(kind.__name__)
            counts[slot] += 1
    except BaseException:
        pass


for name in sorted(vars(m)):
    value = vars(m)[name]
    if isinstance(value, type):
        continue
    note(value, shallow, 0)
    note(value, deep, 1)
    # a body keeps its instances in a list or a dict as readily as under a name of
    # their own, and the shallow answer is exactly what `isosurface` already reports —
    # so the difference between the two is how much that rung cannot see
    if isinstance(value, (list, tuple, set, frozenset)):
        for item in list(value)[:200]:
            note(item, deep, 1)
    elif isinstance(value, dict):
        for item in list(value.values())[:200]:
            note(item, deep, 1)

print('FLIP-SHALLOW %s' % ','.join(sorted(shallow)))
print('FLIP-DEEP %s' % ','.join(sorted(deep)))
print('FLIP-COUNTS %d %d' % (counts[0], counts[1]))

# which of the classes the module init was compiled to rebind it actually rebound. a
# module that takes the layout guard's whole-module bailout compiles every one of them
# and installs none, so what the emitted C promises is an upper bound on what a given
# import does. 3.13's compiler writes `__firstlineno__` into a class statement's
# namespace and a spec has no code object to write one from, so its absence is the
# compiled type
installed = [name for name in sys.argv[1:]
             if isinstance(getattr(m, name, None), type)
             and '__firstlineno__' not in vars(getattr(m, name))]
print('INSTALLED %s' % ','.join(sorted(installed)))
PYEOF

for b in $(sweep_modules "$LIB" "$@"); do
  f="$LIB/$b"
  [ -f "$f" ] || continue
  d="$SWEEP_ROOT/w"; sweep_stage "$d" "$LIB" "$b"
  sweep_compile "$b" "$d" "$PY" "$BY"
  if ! sweep_built "$d"; then printf '%s\tno-artifact\n' "$b" >> "$OUT"; continue; fi
  # the swap this defect is about, read off the emitted C: every class whose name the
  # module init rebinds to a compiled type
  #
  # read *before* `sweep_place`: a package member's C sits inside the package's place in
  # the output tree, and `sweep_place` lays the twin's copy of the package over the top
  emitted=$(grep -oE 'PyDict_SetItemString\(dict, "[^"]+", By_[A-Za-z0-9_]+_OBJ\)' \
    "$(sweep_out_dir "$d")/m.c" \
    | sed -E 's/.*dict, "([^"]+)".*/\1/' | LC_ALL=C sort -u | paste -sd, -)
  sweep_place "$d"
  cp "$SWEEP_ROOT/probe.py" "$SWEEP_RUN_C/probe.py"
  # the comma list becomes one argument per class; an array says that, where a bare
  # command substitution only word-splits by accident
  names=()
  [ -n "$emitted" ] && IFS=',' read -r -a names <<< "$emitted"
  # through `sweep_capture` rather than a command substitution around the probe: the probe
  # imports the module and builds its classes, so it starts — and leaks — whatever a
  # constructor starts, and a pipe one of those still holds never reaches end of file. the
  # reasons are with the helper, in `sweeplib.sh`
  sweep_capture "$SWEEP_RUN_C" "$PY" probe.py "${names[@]}"; out=$SWEEP_CAPTURE_TEXT
  case "$out" in
    IMPORT-FAILED*) printf '%s\timport-failed\temitted=%s\n' "$b" "$emitted" >> "$OUT"; continue ;;
  esac
  printf '%s\tok\temitted=%s\tinstalled=%s\tshallow=%s\tdeep=%s\tvalues=%s\n' "$b" "$emitted" \
    "$(echo "$out" | sed -n 's/^INSTALLED //p')" \
    "$(echo "$out" | sed -n 's/^FLIP-SHALLOW //p')" \
    "$(echo "$out" | sed -n 's/^FLIP-DEEP //p')" \
    "$(echo "$out" | sed -n 's/^FLIP-COUNTS //p')" >> "$OUT"
done

# the analysis below reports every figure as a fraction of a population it reads back out
# of `$OUT`, so a walk that stopped early would come out as a smaller corpus rather than
# as an unfinished one — `0 of 0 emitted` where the answer was `0 of 1`. so it does not
# run at all until the run has been shown to be complete
sweep_end || exit 1

"$PY" - "$OUT" "$LIB" <<'PYEOF'
"""the defect, and what each candidate rule for declining it would cost

the sources are re-parsed rather than recompiled: the census already settled which
classes are emitted, and every rule is a question about the module body's syntax
"""

import ast
import sys

census, lib = sys.argv[1:3]

rows = []
for line in open(census):
    parts = line.rstrip('\n').split('\t')
    if len(parts) < 2 or parts[1] == 'no-artifact':
        continue
    row = {'module': parts[0], 'status': parts[1]}
    for part in parts[2:]:
        key, _, value = part.partition('=')
        row[key] = value
    rows.append(row)


def names(row, key):
    return set(filter(None, row.get(key, '').split(',')))


class Body(ast.NodeVisitor):
    """the names the module body reaches, split by how it reaches them

    the body is every statement that runs at import: the module's own statements and the
    compound statements they nest in, but not a function or class body, which runs later
    """

    def __init__(self):
        self.named = set()
        self.called = set()
        self.decorating = set()
        self.meta = set()

    def walk(self, statements):
        for statement in statements:
            if isinstance(statement, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef)):
                for node in statement.decorator_list:
                    target = node.func if isinstance(node, ast.Call) else node
                    if isinstance(target, ast.Name):
                        self.decorating.add(target.id)
                    self.expressions([node])
                if isinstance(statement, ast.ClassDef):
                    self.expressions(list(statement.bases))
                    for keyword in statement.keywords:
                        if isinstance(keyword.value, ast.Name):
                            self.meta.add(keyword.value.id)
                        self.expressions([keyword.value])
                continue
            for _, value in ast.iter_fields(statement):
                items = value if isinstance(value, list) else [value]
                nested = [item for item in items if isinstance(item, ast.stmt)]
                rest = [item for item in items
                        if isinstance(item, ast.AST) and not isinstance(item, ast.stmt)]
                self.walk(nested)
                self.expressions(rest)

    def expressions(self, nodes):
        for node in nodes:
            for inner in ast.walk(node):
                if isinstance(inner, ast.Name) and isinstance(inner.ctx, ast.Load):
                    self.named.add(inner.id)
                if isinstance(inner, ast.Call) and isinstance(inner.func, ast.Name):
                    self.called.add(inner.func.id)


RULES = ['named', 'called', 'called+decorating', 'called+decorating+meta']
cost = dict.fromkeys(RULES, 0)
cost_installed = dict.fromkeys(RULES, 0)
missed = {rule: [] for rule in RULES}
emitted_total = 0
installed_total = 0
sealed = 0
flip_values = [0, 0]
flip_modules = set()

for row in rows:
    emitted = names(row, 'emitted')
    installed = names(row, 'installed')
    emitted_total += len(emitted)
    installed_total += len(installed)
    counts = (row.get('values') or '0 0').split() or ['0', '0']
    flip_values[0] += int(counts[0])
    flip_values[1] += int(counts[1])
    if names(row, 'deep'):
        flip_modules.add(row['module'])
    try:
        tree = ast.parse(open('%s/%s' % (lib, row['module']), 'rb').read())
    except (SyntaxError, OSError):
        continue
    body = Body()
    body.walk(tree.body)
    sets = {
        'named': body.named,
        'called': body.called,
        'called+decorating': body.called | body.decorating,
        'called+decorating+meta': body.called | body.decorating | body.meta,
    }
    for rule in RULES:
        hit = emitted & sets[rule]
        cost[rule] += len(hit)
        cost_installed[rule] += len(installed & sets[rule])
        missed[rule].extend('%s:%s' % (row['module'], cls)
                            for cls in sorted(names(row, 'deep') - hit))
    # `mutable_type` in codegen: any decorator, any base, or a base of another class
    # here. what is left is sealed — immutable and not a base type — which is what an
    # earlier swap would have to give up
    declared = {}
    used_as_base = set()
    for node in ast.walk(tree):
        if isinstance(node, ast.ClassDef):
            declared[node.name] = node
            used_as_base.update(base.id for base in node.bases if isinstance(base, ast.Name))
    for name in emitted:
        node = declared.get(name)
        if node is not None and not (node.decorator_list or node.bases or name in used_as_base):
            sealed += 1

built = [row for row in rows if row['status'] in ('ok', 'import-failed')]
ok = [row for row in rows if row['status'] == 'ok']
print()
print('built %d   importable %d' % (len(built), len(ok)))
print('classes the compiler rebinds a name to: %d emitted, %d of them sealed'
      % (emitted_total, sealed))
print('classes an import actually stands a compiled type under: %d' % installed_total)
print('values their class disowns: %d under a name of their own, %d one level in, '
      'across %d modules' % (flip_values[0], flip_values[1], len(flip_modules)))
print()
for rule in RULES:
    print('%-24s declines %4d of %d emitted (%.1f%%), %4d of %d installed (%.1f%%); '
          'leaves wrong %s'
          % (rule, cost[rule], emitted_total, 100.0 * cost[rule] / max(emitted_total, 1),
             cost_installed[rule], installed_total,
             100.0 * cost_installed[rule] / max(installed_total, 1),
             ' '.join(missed[rule]) or 'nothing the census can see'))
PYEOF
