# the benchmark suite

```sh
cargo build --release --bin by
uv run --no-project --python 3.13 python scripts/native-bench/bench.py
```

the suite lives in `scripts/native-bench/`: `bench.py` stages and builds,
`timer.py` times, `programs/` holds the benchmarks and `programs.toml` says what
each is for and what it is allowed to leave interpreted

it needs a **release** build — a debug `by` is too slow to measure anything with
— and it needs `uv`, which fetches both the interpreter and mypyc

## the one thing to know

**the ratio is the measurement.** absolute times on this suite move by a factor
of four with machine load: cpython's own `dot` has read 47ms and 209ms on the
same laptop in the same week. no number here is reported from one build's clock
alone

and **every row carries its own error bar**. each benchmark is compiled *twice*,
independently, by the same compiler, and both builds are timed. two builds of one
program are one program — literally: the generated C is byte-identical once the
module name is normalised out — so whatever ratio the suite reports between them
is noise it invented. that is the `noise` column, and nothing smaller than a
row's own noise is a finding

## how a run works

four builds of each benchmark, staged under four module names of their own:

| build     | what it is                     |
| --------- | ------------------------------ |
| `cpython` | the source, interpreted        |
| `by`      | `by compile`                   |
| `control` | `by compile` again, separately |
| `mypyc`   | mypyc, for scale               |

the distinct names are what makes the method possible. an extension module's
init hook is found by name, so two of them cannot answer to `mandel` — under
`mandel_by` and `mandel_control` they can, and all four builds then live in one
process at once

and because they live in one process they can be **interleaved**. a run is a
sequence of rounds, and in each round every build is timed once, in an order
that rotates so none of them is always the one paying for a cold cache. a load
spike therefore lands inside the same round as everything else, and mostly
cancels when the round is turned into a quotient

## the statistics

the **median**, throughout — never the mean, never the minimum

- the mean is moved by the one round that hit a scheduler
- the minimum is an extreme-value statistic. its expectation depends on how many
    rounds were run and on how quiet the machine happened to be, so a minimum is
    not comparable with another minimum, which is the only thing this suite is for

a ratio is **paired**: it is the median of the per-round quotients, not the
quotient of the two medians. dividing one build's median by another's would let
a spike that landed on only one of them through unchallenged

the interval on each ratio is the distribution-free one for a median, taken from
the binomial tail. it is exact, assumes nothing about the shape of the noise,
needs no resampling and no random numbers — so two readings of the same data
agree. below nine rounds that interval degenerates to the range of everything
seen, so nine is a floor the harness enforces rather than a default

one sample is many calls, and how many is calibrated per benchmark: enough that
the fastest build's sample is well clear of the clock, few enough that the
slowest build's round does not dominate the run. every build runs the same
number, so the pairing stays exact

## what it refuses

each of these is a way this suite has actually produced a wrong number, and each
is now a refusal rather than a warning:

| refusal                                              | the failure it closes                                                                       |
| ---------------------------------------------------- | ------------------------------------------------------------------------------------------- |
| the artefact must be a real extension module         | a broken shim made `by compile` fail silently and the previous `.so` was timed in its place |
| …under this run's own root, newer than this build    | the run's root is a fresh temporary directory, so a stale one cannot be reached at all      |
| an interpreted build must **not** be an extension    | the mirror image, which would make the baseline a compiled one                              |
| every build must return cpython's answer             | a build that got a different answer is not a faster build                                   |
| the decline count must match `programs.toml` exactly | a benchmark can decline and quietly measure interpreted code while still posting a number   |
| an unknown name, an unlisted program, an empty run   | a harness that matches nothing looks exactly like a harness that found nothing wrong        |

the decline check is the one worth dwelling on. `by compile` never fails on code
it cannot lower — that function runs from its interpreted definition instead — so
a "compiled" number can silently be an interpreted one. `programs.toml` records
the expected count per benchmark and the run fails when it moves **in either
direction**: a compiler that started compiling something is as much a change as
one that stopped. improving the compiler edits that file in the same commit,
which makes it a readable ledger of what the backend has learned to take

the refusals are exercised rather than asserted:

```sh
uv run --no-project --python 3.13 python scripts/native-bench/bench.py --self-check
```

which builds a leg that is wrong in each of those ways and proves each one is
turned away

## what it measures

one axis per group, and a benchmark earns its place by being the only one on its
axis — or by being half of a pair whose *difference* is the axis:

| group      | benchmarks                               | the question                                         |
| ---------- | ---------------------------------------- | ---------------------------------------------------- |
| `float`    | `mandel`, `mandel_inline`                | scalar float work; the pair isolates calls           |
| `int`      | `loops`, `bigint`                        | tagged integers, and leaving the word                |
| `dispatch` | `calls`, `methods`, `recurse`, `inherit` | call, method, depth, and an override                 |
| `memory`   | `alloc`, `fields`, `objects`             | allocation and field access, apart and together      |
| `list`     | `dot`, `prefix`, `sieve`                 | indexed reads, growth, indexed writes                |
| `tuple`    | `tuples`                                 | pack, unpack, and a two-value return                 |
| `set`      | `sets`                                   | membership as the whole operation                    |
| `dict`     | `dictget`, `dicthist`                    | lookup-and-update, and the histogram miss            |
| `str`      | `words`, `chars`, `strops`, `keybuild`   | build, scan, methods, and key construction           |
| `boxing`   | `generic`, `generic_mono`                | the pair isolates what a type parameter costs        |
| `control`  | `excs`                                   | a raise that is caught, and a `try` that never fires |
| `frames`   | `gen`, `coro`                            | the two halves of the resumable-frame lowering       |

the pairs are read as pairs. `generic` alone says almost nothing; against
`generic_mono`, which is the same program with the call monomorphised by hand,
it says exactly what the type parameter costs. same for `mandel` against
`mandel_inline`, and for `objects` against `alloc`, `fields` and `methods`

adding one is three files: the program, an entry in `programs.toml`, and a line
in the table above. the entry has to say what it measures that nothing else does

## comparing two runs

```sh
… bench.py --json today.json
… bench.py --baseline today.json
```

the quantity compared between runs is the **speedup**, not the time. absolute
times are not comparable across machines, across interpreters, or across a
Tuesday, and this suite exists because they were compared anyway

a change is called one only when it clears the bar, and the bar is whichever is
larger: the noise the two runs measured for themselves, or
`--regression-threshold`. a regression exits non-zero, so it is detected rather
than eyeballed. a baseline from a different interpreter or a different host is
compared anyway but says so first, and a row either run found too noisy is
skipped rather than given a very wide bar

**the noise column does not bound this.** the control is two builds in one
process at one moment, so it bounds the noise *within* a run. between two runs
there is more: a different process, a different heap layout, a different machine
mood. measured rather than assumed — two runs of an **unchanged** compiler,
forty minutes apart, agreed within 3% on 25 of 27 rows and disagreed by 8.1% and
8.4% on the other two, while both of those rows reported a ±0.8% floor for
themselves. so the default bar is 10%, which is what that evidence supports, and
not the within-run floor, which would have called both of them improvements

the consequence is worth stating plainly: on a machine doing other work this
suite sees a 10% change and does not see a 5% one. for smaller than that, use a
machine that does nothing else, drop `--regression-threshold` to match the floor
it then reports, and confirm anything it flags by running it again

## the half that can run anywhere

```sh
… bench.py --verify-only
… bench.py --self-check
```

`--verify-only` does everything the suite does except look at a clock: it builds
all four ways, proves each artefact, checks that every build gives cpython's
answer, and checks the decline ledger. none of that depends on how busy the
machine is, so it is the half that belongs on a shared runner — and it is where
most of the *correctness* value is. it catches a benchmark that stopped
compiling, a compiled build that started answering differently, and a compiler
that quietly began declining something it used to take

the timing half wants a machine of its own. a shared runner cannot hold a 3%
noise floor, and this suite's whole premise is that a number it cannot stand
behind should not be printed as if it could. run the table on a fixed machine,
keep the `--json`, and gate on `--baseline` against the previous one

## reading a bad run

a row whose noise exceeds `--noise-limit` (10% by default) is marked `!` and
does not count as measured. the run then exits non-zero even if everything else
looks fine, because a table that is *mostly* trustworthy is the kind nobody
remembers to check before quoting one of its rows

that limit is not fastidiousness. pairing cancels the *noise* a busy machine
adds, but it does not cancel the **bias**: under preemption the longer sample
loses more, and the interpreted build's sample is the long one, so contention
pushes the reported speedup *up*. a run at load 95 on a 16-core laptop read
`fields` at 23.8x where two quiet runs both put it near 12x, and the two
supposedly identical builds disagreed by 74% in the same breath. widening the
bar does not fix a bias, so a row that noisy is skipped rather than compared

between those two extremes the floor is simply reported. a `±6%` floor means
the run can see a 2x difference and cannot see a 10% one, which is often all
that was wanted
