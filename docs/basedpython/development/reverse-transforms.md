# reverse transforms

basedpython can convert standard Python source back into basedpython syntax. inverse of normal transpilation

## usage

```sh
by transpile --reverse file.py
```

## what it does

reverse transforms detect patterns in standard Python that correspond to basedpython idioms and rewrite them back. enables round-tripping: a Python file passed through `--reverse` then transpiled forward should produce code with the same AST as the original

after the rewrites run, any `from … import …` line whose bindings are no longer referenced (e.g. `from typing import Callable` after `Callable[...]` was rewritten to the arrow form) is pruned from the output so the reversed source isn't carrying dead imports

## design

each reverse transform lives in `src/reverse_transforms/` and mirrors the forward transform of the same name in `src/transforms/`. they share the visitor-based approach: walk the AST, detect the lowered shape, emit text edits to rewrite back to the basedpython surface form

### edit only the syntax you own

a rewrite must be emitted as edits over the punctuation it is replacing, not as one replacement covering the whole expression. `tuple[int, str]` → `(int, str)` is two edits — `tuple[` becomes `(`, `]` becomes `)` — and the elements are never touched

rendering a whole expression from raw source looks simpler but breaks two ways. the transforms run against one shared source, and edits are applied first-wins on overlap: a wide replacement either loses to the narrow edits inside it or *wins and silently undoes them*, so `tuple[LiteralString, ...]` comes out `(*: LiteralString)` with the `literal str` rewrite gone. it also re-flows the text, collapsing a multi-line union or tuple onto one line

so recursion is a walk that emits its own edits per node, not a function returning rendered text. when a transform genuinely needs to wrap what it did not rewrite — `TypeIs[…]` → `name is (…)` — the parenthesis goes in the boundary edit, leaving the inside alone

forward transforms drive lowering; reverse transforms drive the inverse rewrite. the two directions stay paired — a new forward transform should be accompanied by a reverse transform unless the lowering is intentionally lossy or unobservable in the produced Python
