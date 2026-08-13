# sourcemaps — design plan

status: **plan**. only the slice `by run` needs is built today (a forward,
line-level table). this document is the design for the full bidirectional,
column-accurate sourcemap and the work to get there. nothing here is
implemented beyond what is noted under "what exists today"

## why

a `.by` file is transpiled to python before anything runs or is analyzed
downstream. every tool that reports a *position* therefore risks pointing at
generated python instead of the `.by` the user wrote:

- **runtime tracebacks** — frames land in generated `.py`, with line numbers
    shifted by the preamble and surface syntax replaced by its lowering
- **type-checker diagnostics on the `.py`** — if a downstream tool checks the
    generated output, spans don't map back
- **editor features** — go-to-definition, hover, rename, inlay hints driven off
    the generated python need to round-trip to `.by` coordinates
- **reverse transpile** — `.py` → `.by` has the same problem mirrored

a correct sourcemap is the single primitive all of these share

## what exists today

- `source_map::line_table(source, edits)` — builds a forward, **line-level**
    table (output line → input line, `None` for generated lines) for one
    edit-application pass
- `transpile_typed_with_map` composes a whole-pipeline line table by lifting the
    phase-0 table over the count of generated preamble lines (valid because phase
    1 applies no body edits and later phases only prepend). consumed by `by run`
    to rewrite tracebacks and by `by transpile` to map a transpiler-invalid-output
    span back to its `.by` line for a source-annotated diagnostic
- `_by_sourcemap.py` carries two tables keyed by the generated `.py` path:
    `SOURCEMAP`, the `.by` path and its line table, and `DIGESTS`, the sha-256 of
    both files that entry describes. `by build` writes it into `out/` beside the
    python it describes, and `by run` into the temporary tree it executes — where
    it lives only as long as that run. see [staleness](#staleness) below

### limitations to remove

- **line granularity** — a column in `int & str` can't map to the column in
    `Intersection[int, str]`. fine for traceback frames, useless for hover/rename
- **forward only** — no `.by` → `.py` direction (needed by editors that start
    from a `.by` position)
- **prepend-only assumption** — the composition trick assumes later phases never
    insert mid-body. true today; brittle as a contract
- **untested arithmetic** — offset math has no property tests

## staleness

built, unlike the rest of this document

a map describes a *pair* of files, and the lines it reports are only true while
both are still the ones it was built from. an editor that saves the `.by` after
the transpile leaves the map describing a pair that no longer exists — and it
goes on resolving lines with total confidence. that is a wrong answer rather
than a missing one, which is the failure a sourcemap exists to prevent

so every entry carries a digest of both sides, and a consumer must recompute
them from disk before it trusts a line:

```python
DIGESTS = {
    "/tmp/.tmpXXXX/demo.py": {
        "by": "sha256:…",  # the .by bytes the transpiler read
        "py": "sha256:…",  # the python bytes it wrote
    },
}
```

three things that shape pins down:

- **the bytes that were read and written** — both digests are taken in the
    transpiling pass, over the source text it ran on and the python it produced.
    hashing the files again afterwards would only be a second chance for them to
    have changed
- **raw bytes** — so a consumer recomputes with
    `sha256(open(path, "rb").read())` and needs to know nothing about encoding or
    line endings
- **the algorithm named in the value** — it can be changed later without
    breaking readers, and a reader that meets one it does not know refuses the
    entry instead of comparing hex it could never have produced

`DIGESTS` is a separate top-level name rather than a third element of the
`SOURCEMAP` tuple: that tuple has consumers (the traceback shim, the pycharm
plugin), a positional change would break every one of them, and a name they
never read is invisible to them

the traceback shim is the first consumer, and it shows what refusing looks like:
when either digest disagrees it leaves the frame in the generated python and
writes a note saying which file no longer matches. a frame pointing at
`out/main.py` is a worse answer, but a frame quoting a `.by` that has been
rewritten since is a false one

both tables are keyed by the generated path exactly as the map spells it. the
shim resolves symlinks before matching a frame's filename against it — a
temporary tree under `/tmp` on macOS is reached by a different path than the one
python reports — so it carries the original key alongside the resolved one. a
consumer that normalises keys has to do the same, or its digest lookup misses
and every entry reads as stale

## goals

1. **byte-accurate** — map arbitrary `.by` byte offsets ↔ `.py` byte offsets,
    not just lines
1. **bidirectional** — `by_to_py` and `py_to_by`, both total (return the nearest
    enclosing mapped span when a position is inside generated code)
1. **whole-pipeline** — composes across every phase, including the lazy-import
    preamble and any mid-body hoist, with no "only prepend" assumption
1. **reverse-aware** — the reverse pipeline produces a map of the same shape
1. **tested** — composition and round-trip are property-tested

## design

### representation: a segment list

model a single transformation step as an ordered list of **segments**, each
either copied or generated:

```text
Segment {
    in:  Option<Range<TextSize>>,   // source bytes, None = generated
    out: Range<TextSize>,           // output bytes
}
```

a step's map is the list of segments covering the whole output in order. copied
segments carry the byte range they came from; generated segments (preamble,
hoisted classes, replacement text with no 1:1 origin) carry `None`. this
subsumes both line and column mapping — line lookups are derived by counting
newlines within a segment

this is the same shape as standard sourcemap "mappings", specialized to a single
file pair and kept as byte ranges rather than line/column until rendered

### per-phase production

each phase already knows its edits; each produces a segment list as a byproduct:

- **phase 0 (ast_driver)** — the splice applies a sorted, non-overlapping edit
    list via `replace_range`. walking that list yields segments directly: copied
    runs between edits, generated runs for replacement text, plus generated
    segments for the prepended imports and any `ctx.hoisted` insertions (whose
    insertion offset is known)
- **phase 1 (lowering)** — currently a preamble prepend only; one generated
    segment for the preamble, identity for the body
- **import-redirect / lazy** — within-line text replacements (`typing` →
    `typing_extensions`, `lazy` prefixes) become copied/generated segment pairs;
    prepended polyfill preambles are generated segments
- **anon-NT cleanup** — prepend + occasional body edit; same treatment

### composition

compose two steps `A: mid→src` and `B: out→mid` into `out→src` by walking `B`'s
segments and, for each copied segment, intersecting its `mid` range against `A`'s
segments to resolve back to `src` (splitting where `A` changes origin). generated
segments stay generated. this is associative, so the pipeline map is a fold over
the per-phase maps. line-level `line_table` becomes a derived view of the
composed byte map, letting `by run` keep working unchanged

### lookup

- `py_to_by(offset)` — binary search the output ranges; return the mapped `.by`
    offset, or the start of the nearest enclosing copied segment when inside
    generated code
- `by_to_py(offset)` — the same against an index built on the `in` ranges; when a
    `.by` offset expands to multiple output spans (rare), return the first

### reverse transpile

the reverse pipeline builds a map of the same type; consumers use one API for
both directions

## consumers and rollout

phase the work behind real consumers so nothing speculative ships:

1. **`by run` tracebacks (done, line-level)** — already shipping; migrate it to
    read the byte map's derived line view when step 2 lands, then delete the
    standalone line table
1. **byte map + composition + property tests** — the core; no API beyond what a
    consumer needs
1. **LSP position mapping** — go-to-def / hover / diagnostics on `.by` via the
    generated python; the first consumer that needs columns and both directions
1. **reverse-direction map** — when an editor feature consumes `.py` → `.by`

## testing

- **round-trip** — `py_to_by(by_to_py(p)) == p` for every copied position
- **composition** — `compose(a, b)` equals mapping through `a` then `b`, checked
    on randomized edit lists
- **golden** — a handful of representative `.by` files (lazy imports,
    intersections, hoisted anon-NT classes) with asserted maps at known offsets
- **traceback integration** — the existing `by run` e2e test, extended to column
    assertions once columns land
