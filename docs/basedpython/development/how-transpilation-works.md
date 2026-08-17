# how transpilation works

basedpython transpiles `.by` source files into standard Python. the pipeline is
deterministic — a single AST-rewrite pass followed by a few text normalization
phases and a final verification step, no convergence loop

## pipeline

```text
source (.by)
  │
  ├─ phase 0  AST rewrite (ast_driver::run_against_source)
  │     ├─ strip use-site variance keywords (`out`/`in`) up front
  │     ├─ parse via ruff_python_parser (the unified parser accepts `.by` syntax)
  │     ├─ build a SemanticModel — the project db when available (cross-module
  │     │  type info), else a single-file in-memory db. a pre-pass that rewrote
  │     │  the source doesn't cost the project: the db is rebuilt over the
  │     │  rewritten text (see "the phase-0 database" below)
  │     ├─ run AstPasses: mutate the AST in place (coalesce, cast, typeof,
  │     │  sentinel, mutable-defaults, …)
  │     ├─ run TypeAwarePasses: read the SemanticModel and emit text edits
  │     │  (intersection, callable, generics, literal-types, anon-NT, …)
  │     └─ splice it together: re-render changed statements, apply text edits
  │        (ruff-style first-wins overlap skip), emit hoisted class defs, prepend
  │        required imports, append `__all__` epilogue
  │
  ├─ phase 1  lowering preamble
  │     └─ optionally prepend `from __future__ import annotations`
  │        (`inject_future_annotations`, off by default)
  │
  ├─ phase 2  import redirect
  │     └─ rewrite `from typing import X` to `from typing_extensions import X`
  │        when X is not yet stdlib at the configured min Python version
  │
  ├─ phase 2b  anon-named-tuple cleanup
  │     └─ re-run `anon_named_tuple` on the post-lowering output to catch anon-NT
  │        spans copied verbatim by other transforms (e.g. the PEP-695 polyfill).
  │        bounded to a few iterations
  │
  ├─ phase 2c  lazy-import marking
  │     └─ lower imports to the `lazy` keyword (3.15+) or a runtime polyfill
  │
  ├─ phase 2d  version polyfill
  │     └─ rewrite syntax the target python cannot parse. today that is the
  │        `match` statement, lowered to an `if`/`elif` chain for a target below
  │        3.10. it runs over the *finished* python rather than over `.by`, so a
  │        `match` an earlier lowering generated is lowered by the same code as
  │        one the author wrote
  │
  └─ phase 3  syntax verification
        ├─ parse the final output as `.py` — any parse error aborts with a
        │  source-annotated diagnostic (the span is mapped back to `.by`)
        ├─ scan the AST for leftover basedpython-only flags
        │  (`is_anon_named_tuple`, `is_anon_named_tuple_value`, `is_typeof`).
        │  a leftover flag means a transform failed to lower its construct; the
        │  pipeline aborts rather than emit syntactically-valid-but-wrong Python
        └─ parse it again *as the target version* and report any construct that
           version cannot parse. the first check asks whether the output is
           python at all; this one asks whether it is python the declared floor
           can run, so syntax no polyfill covers is a diagnostic instead of a
           `SyntaxError` at import time in generated code
```

entry points in `crates/by_transforms/src/lib.rs`:

- `transpile(source, config) -> Result<String, String>` — single-file (stdin,
    tests); type-aware passes see only this file
- `transpile_typed(db, file, config, rebuild)` — uses an existing project db so
    type-aware passes resolve cross-module types (the CLI path for
    `by transpile`, `by build` and `by run`)
- `transpile_typed_with_map(db, file, config, rebuild)` — also returns a line
    table for traceback rewriting and diagnostic mapping

## the phase-0 database

three pre-passes run before phase 0 and can rewrite the source: erased-union
reification, context-sensitive name qualification, and enum lowering. phase 0
re-parses its input and binds `inferred_type` to those exact node identities, so
its database has to hold the *rewritten* text, not the project file's

that would leave phase 0 with a single-file in-memory db — no search paths, no
sibling modules — and every `TypeAwarePass` silently declining to lower, so an
enum declared anywhere in a file would break a lookup elsewhere in it. instead
the caller supplies a `RebuildProject`: a way to build a second database over the
same project, into which the rewritten source is served for this one file via
`File::source_text_override`. the project's metadata, search paths and sibling
files stay; only the file being transpiled reads differently

the rebuilt database must be a *new* one rather than a clone — salsa handles
cloned from one database share storage, so the override would be visible through
the caller's own db. a caller with no project (a bare unit test) passes `None`
and gets the single-file db: correct, just blind past the file

## passes

`ast_driver` runs two kinds of pass against the parsed module:

- **`AstPass`** — mutates the AST in place via the
    [`Transformer`](https://docs.rs/ruff_python_ast) protocol. the driver tracks
    which top-level statements changed and re-renders them through
    `ruff_python_codegen` (basedpython mode)
- **`TypeAwarePass`** — reads the shared `SemanticModel` and emits sub-statement
    text edits keyed by `TextRange`. it never mutates the AST, because
    `inferred_type` binds to the exact parsed node identities

order matters: passes that target the same offset rely on a fixed sequence
(e.g. `type_is` before `identity_swap`, `coalesce` before `none_chain`). the
ordered lists live in `run_against_source`. a complete list of transforms is at
`crates/by_transforms/src/transforms/mod.rs`; each module's `///` docs describe
the rewrite it performs

## splicing

after the passes run, the driver assembles the output in one pass:

1. whole-statement replacements for mutated statements (re-rendered) and hoisted
    statements (synthesized class defs inserted before the statement that needs
    them)
1. sub-statement text edits, applied with ruff-style first-wins overlap skip —
    a wider edit wins over a narrower one nested inside it
1. `required_imports` prepended (deduped, `from`-imports merged)
1. `__all__` epilogue appended for `export`/`public` modifiers

## source maps

`transpile_typed_with_map` returns a line table (`output line → .by line`,
`None` for generated lines), composed from the phase-0 table plus the count of
generated preamble lines. it powers `by run`'s traceback rewriting and the
`.by` source span on transpiler-error diagnostics. the table is line-level only;
the byte-accurate, bidirectional design is in
[sourcemaps](sourcemaps.md)

## reverse transforms

basedpython also supports reverse transpilation — converting standard Python
back into basedpython syntax. see [reverse transforms](reverse-transforms.md)
for details
