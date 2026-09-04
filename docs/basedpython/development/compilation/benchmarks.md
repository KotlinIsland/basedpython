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

## how long a sample is

one sample is **fifty milliseconds**, and each build fills that for itself: it
calls `bench()` until the time is up, and reports the elapsed time divided by the
calls it got through. so the call count differs between builds, differs between
rounds, and is not decided before the run

the size is set by the scheduler rather than by the clock. `perf_counter`
resolves nanoseconds, but a sample the scheduler takes away comes back about a
ten-millisecond quantum late, and *that* is the error a sample has to be long
enough to swamp. at 50ms one such hit is a 20% outlier, which is the sort of
thing a median over 21 rounds absorbs

it used to be one count per benchmark, calibrated once and handed to every build
of it. that is not wrong on a quiet machine — it is what produced most of the
tables anyone has quoted — but it leaves two ways for a busy one to ruin a row:

- **one count cannot serve four builds.** they can be two hundred times apart, so
    a count long enough for the interpreted build left the compiled one timing a
    third of a millisecond. that is fine until something deschedules it, and then
    a single ten-millisecond quantum is thirty times the whole reading:
    `mandel_inline` reported a **±631% floor** for two builds of one program on an
    evening when the machine was at eight times oversubscription
- **the count came from one un-replicated probe**, taken minutes before the
    timing. a machine that got busier in between went on using a count chosen for
    a speed the run no longer had: one `mandel_inline` count was picked against a
    16ms interpreted call and was still in use when that call had become 77ms

so what a duration buys is not resolution, it is **robustness** — see the numbers
further down

⚠️ **scaling a benchmark's own work up does not fix any of this.** it is the
obvious remedy and it is a no-op here, because the count was derived from the
measured speed: ten times the work per call divides the count by ten and the
sample comes out the same length. measured rather than reasoned — `mandel_inline`
with its inner loop scaled tenfold went from 9 calls of 0.32ms to 1 call of
3.26ms, a 2.9ms sample against a 3.3ms one, and `calls` from 9 × 0.37ms to
1 × 3.45ms. what starved the fast builds was never the size of the program, it
was the *spread* between the builds of it, and no amount of scaling changes a
ratio

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

| group      | benchmarks                                                                  | the question                                                         |
| ---------- | --------------------------------------------------------------------------- | -------------------------------------------------------------------- |
| `float`    | `mandel`, `mandel_inline`                                                   | scalar float work; the pair isolates calls                           |
| `int`      | `loops`, `bigint`                                                           | tagged integers, and leaving the word                                |
| `dispatch` | `calls`, `methods`, `recurse`, `inherit`, `props`, `props_ext`, `props_get` | call, method, depth, an override, a property pair, and a lone getter |
| `memory`   | `alloc`, `fields`, `objects`                                                | allocation and field access, apart and together                      |
| `list`     | `dot`, `prefix`, `sieve`                                                    | indexed reads, growth, indexed writes                                |
| `tuple`    | `tuples`, `pairs`                                                           | pack, unpack and a two-value return; the pair isolates the slot      |
| `set`      | `sets`                                                                      | membership as the whole operation                                    |
| `dict`     | `dictget`, `dicthist`                                                       | lookup-and-update, and the histogram miss                            |
| `str`      | `words`, `chars`, `strops`, `keybuild`                                      | build, scan, methods, and key construction                           |
| `boxing`   | `generic`, `generic_mono`                                                   | the pair isolates what a type parameter costs                        |
| `control`  | `excs`                                                                      | a raise that is caught, and a `try` that never fires                 |
| `frames`   | `gen`, `coro`                                                               | the two halves of the resumable-frame lowering                       |

the pairs are read as pairs. `generic` alone says almost nothing; against
`generic_mono`, which is the same program with the call monomorphised by hand,
it says exactly what the type parameter costs. same for `mandel` against
`mandel_inline`, and for `objects` against `alloc`, `fields` and `methods`

`props_ext` is `props` with one line added — a class in the same module that
extends the one holding the pair. that makes the holder a mutable heap type,
because a subclass could override a half, so every read and write goes round the
descriptor protocol instead of calling the half. the difference between the two
rows is what that costs, and `fields` is the floor under both: the same reads and
writes done directly

`props_get` is the third of that set: the same read through a `@property` with no
setter written under it. python folds a group of one into a `property` just as it
folds a pair, so the two rows differ only in whether the source wrote the two
lines under `@v.setter` — which is what makes the difference between them a
measurement of the compiler rather than of the program

`pairs` is `tuples` with the first slot changed from an `int` to an instance the
loop is handed rather than builds. a tagged word is copied and nothing is
retained; a pointer is retained and released, and that is the other half of what
a tuple slot can be. the object is built once outside the loop so the difference
between the two rows is the slot rather than an allocation, which `alloc`
already measures

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

a baseline written before the sample became a duration is comparable only for
what it is: the two runs timed the same programs, but not for the same lengths,
so a `--baseline` across that change carries a method change inside every row.
the run's metadata carries `sample_target` for exactly this reason

every run also records the **sha256 of the compiler it measured**, and a
comparison says whether the two runs' hashes agree. a version string and a git
description are about the checkout rather than about the file on disk, and both
go on saying the same thing while the binary underneath is rebuilt — or is not
rebuilt when it should have been. running one binary against itself is a useful
thing to do deliberately, since it is how the floor is measured, and a
disastrous one to do by accident; from the outside the two look identical, and an
ablation harness here once did the second and read the result as the first

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

each row also records the load **at the moment that row was timed**, and a row
marked `!` prints it. the run-level figure is taken at the two ends and a run is
minutes long, which is not the same thing: one run here started at load 26 and
finished at 193, so its early rows and its late rows were measured on what
amounted to two different machines and the table alone could not say which was
which

## what the sample length was worth

measured the only way that is self-contained: the suite run **twice against one
unchanged binary**, so that every difference between the two tables is noise the
suite invented. same 29 programs, same compiler — asserted by sha256 rather than
by the path that was typed — on a 16-core laptop, in one quiet stretch, one pair
of runs each way

|                                           | shared count  | duration per build |
| ----------------------------------------- | ------------- | ------------------ |
| load across the two runs                  | 121 → 57 → 26 | 22 → 30 → 32       |
| run-to-run change in `vs cpython`, median | 1.96%         | 2.02%              |
| … worst row                               | 50.0%         | **8.0%**           |
| … rows agreeing within 10%                | 25 / 29       | **29 / 29**        |
| within-run floor, median                  | ±3.3%, ±2.6%  | ±2.6%, ±2.8%       |
| … worst                                   | ±61.8%        | **±25.3%**         |

**it did not make the typical row more precise, and it was not expected to.** the
median is unchanged, and on a quiet machine the shared count was never the
problem: `mandel_inline` at a 0.9ms sample read a ±0.9% floor for itself

what it removes is the **tail** — the row that blows up because one sample of a
few milliseconds caught a descheduling, which is the row somebody then cannot
quote. `sieve` went from a 50.0% run-to-run disagreement to 4.6%, `tuples` from
30.2% to 3.6%, `recurse` from 18.8% to 1.2%. that matters more than it sounds,
because a run is failed by its worst row rather than its median

on a **busy** machine the same shape shows up much larger. alternating the two
timers benchmark by benchmark through six rounds at loads between 44 and 165, the
median floor went 8.4% → 6.0% and the upper quartile 20.9% → 11.9%, with the
duration-based timer lower in 30 of 48 paired readings

⚠️ **speedups from a run before this change are not comparable with one after
it**, and the method is only half the reason. those earlier runs were the busier
ones, and contention pushes a reported speedup *up*: `words` read 91x and 110x on
a loaded evening and 48x in the quiet stretch, on one unchanged compiler
