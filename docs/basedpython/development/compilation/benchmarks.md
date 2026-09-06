# the benchmark suite

```sh
cargo build --bin by
TMPDIR=<a directory holding a .metadata_never_index file> \
  uv run --no-project --python 3.13 python scripts/native-bench/bench.py
```

the suite lives in `scripts/native-bench/`: `bench.py` stages and builds,
`timer.py` times, `programs/` holds the benchmarks and `programs.toml` says what
each is for and what it is allowed to leave interpreted. it needs `uv`, which
fetches both the interpreter and mypyc

a **debug** `by` is the right one, and this used to say the opposite. what the
suite times is the extension module `by` emitted, and clang compiles that module
with flags from `by`'s own pipeline rather than from how `by` was built — so the
emitted C is byte-identical across the two profiles, measured over eight diverse
programs. a release `by` stages a little faster (1.06s against 1.17s for eight
programs), on a phase that is not timed, and costs a ten-minute build that
saturates the cores. **that build is itself the problem**: the load a lane makes
to prepare a measurement is the load that makes the harness refuse rows

⚠️ **stage somewhere spotlight does not index.** `bench.py` uses
`tempfile.mkdtemp()`, so it stages under `$TMPDIR` in `/var/folders/...` — which
an exclusion of the project directory does not cover. macos then indexes every
generated `.c` and `.so`, and `mds_stores` sits at ~100% cpu. pointing `TMPDIR`
at a directory carrying a `.metadata_never_index` file took the median noise
floor from **±9.58% to ±1.46%** and the refused rows from 16 of 33 to 5

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

a benchmark written in basedpython is built the same four ways, except that the
two interpreted-source builds run the python `by transpile` lowers it to while
`by compile` takes the `.by` as written — see
[a benchmark written in basedpython](#a-benchmark-written-in-basedpython)

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

| refusal                                                | the failure it closes                                                                        |
| ------------------------------------------------------ | -------------------------------------------------------------------------------------------- |
| the artefact must be a real extension module           | a broken shim made `by compile` fail silently and the previous `.so` was timed in its place  |
| …under this run's own root, newer than this build      | the run's root is a fresh temporary directory, so a stale one cannot be reached at all       |
| an interpreted build must **not** be an extension      | the mirror image, which would make the baseline a compiled one                               |
| every build must return cpython's answer               | a build that got a different answer is not a faster build                                    |
| the decline count must match `programs.toml` exactly   | a benchmark can decline and quietly measure interpreted code while still posting a number    |
| a declared `setup` must change what `bench()` answers  | a benchmark that builds its own inputs inside the clock reports two costs as one             |
| an unknown name, an unlisted program, an empty run     | a harness that matches nothing looks exactly like a harness that found nothing wrong         |
| a basedpython program that will not transpile          | a `.by` row with no python baseline, silently comparing two of its four builds               |
| one benchmark name on disk as both a `.py` and a `.by` | the extension says how a program is built, so a name carrying both cannot say which is meant |

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

## what is inside the clock

everything `bench()` does is timed, so a benchmark with inputs to build must not
build them there. eight of them did: `dot` built the two lists it multiplies,
`dicthist` built its twenty thousand words, `dictget` built the keys its own
docstring said were "handed in already built", and `prefix`, `chars`, `strops`,
`sets` and `inherit` each prepared something before doing the work they are
named for

those inputs are now built in a `setup()` the harness calls once per process,
after the module is imported and before any clock starts, and the program says
`setup = true` in `programs.toml`. what a row reports is then the operation in
its name and nothing else

it matters more for a **ratio** than the fraction of the timed region would
suggest. preparation does not speed up by anything like what the work does — the
repetition that builds `chars`'s text is one call into cpython and runs at the
same speed in every build — so a fixed cost sits in both sides of the quotient
and caps what the row can report however fast the compiled loop gets. that is
Amdahl's law arriving through the benchmark rather than through the compiler,
and it pushes every affected row's speedup down towards 1.00

the declaration is checked by **withholding** it. with its inputs unbuilt, a
benchmark has to answer differently — `dot` returns `0.0`, `prefix` raises
`IndexError` — because it has nothing to work on. a `bench()` that had gone back
to building its own inputs would answer the same either way, so `bench.py`
refuses a program whose two answers agree. the fix is otherwise silently
reversible: fold the build back in and everything goes on running, with a
`setup` left behind that no longer does anything

not every build inside a `bench()` is preparation. `words` measures
concatenation, `sieve` owns the buffer it writes, and `generic` and
`generic_mono` are a pair whose whole difference lives in building a list for a
consumer that cannot unbox it — those builds are the measurement, and they stay
where they are

⚠️ **a figure recorded for any of those eight rows before this change is not
comparable with one taken after it.** they measure something narrower now

## what it measures

one axis per group, and a benchmark earns its place by being the only one on its
axis — or by being half of a pair whose *difference* is the axis:

| group      | benchmarks                                                                                                                         | the question                                                                                                                                                                                 |
| ---------- | ---------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `float`    | `mandel`, `mandel_inline`                                                                                                          | scalar float work; the pair isolates calls                                                                                                                                                   |
| `int`      | `loops`, `bigint`                                                                                                                  | tagged integers, and leaving the word                                                                                                                                                        |
| `dispatch` | `calls`, `methods`, `recurse`, `inherit`, `props`, `props_ext`, `props_get`, `accessors`, `dunder`, `kwargs`, `closure`, `sortkey` | call, method, depth, an override, a property pair, a lone getter, that pair written as an accessor block, the operator slots, an argument shape, a captured cell, and a call a builtin makes |
| `memory`   | `alloc`, `alloc_slots`, `fields`, `objects`, `globals_`, `dataclass`                                                               | allocation with and without a dict, field access, a name read from the module, and a synthesised constructor                                                                                 |
| `list`     | `dot`, `prefix`, `sieve`, `comp`, `slices`                                                                                         | indexed reads, growth, indexed writes, the comprehension against `prefix`, and a slice                                                                                                       |
| `tuple`    | `tuples`, `pairs`                                                                                                                  | pack, unpack and a two-value return; the pair isolates the slot                                                                                                                              |
| `set`      | `sets`                                                                                                                             | membership as the whole operation                                                                                                                                                            |
| `dict`     | `dictget`, `dicthist`                                                                                                              | lookup-and-update, and the histogram miss                                                                                                                                                    |
| `str`      | `words`, `chars`, `strops`, `keybuild`                                                                                             | build, scan, methods, and key construction                                                                                                                                                   |
| `boxing`   | `generic`, `generic_mono`                                                                                                          | the pair isolates what a type parameter costs                                                                                                                                                |
| `control`  | `excs`, `with_`, `match_`                                                                                                          | a raise that is caught, a `try` that never fires, a context manager, and a pattern ladder                                                                                                    |
| `frames`   | `gen`, `coro`, `coro_none`                                                                                                         | the resumable-frame lowering, and an await that never suspends                                                                                                                               |
| `enum`     | `payloads`                                                                                                                         | a `match` over an enum whose variants carry data                                                                                                                                             |
| `bind`     | `destructure`                                                                                                                      | a pattern binding in a `for` target and in a `let`                                                                                                                                           |

the pairs are read as pairs. `generic` alone says almost nothing; against
`generic_mono`, which is the same program with the call monomorphised by hand,
it says exactly what the type parameter costs. same for `mandel` against
`mandel_inline`, and for `objects` against `alloc`, `fields` and `methods`

`props_ext` is `props` with one line added — a class in the same module that
extends the one holding the pair. that makes the holder a mutable heap type an
interpreted class may subclass and whose attributes may be rebound, so the direct
call `props` gets is not licensed: each read and write asks first whether the
receiver is exactly that class and the pair is still the one that was compiled,
and takes the descriptor protocol when it is not. the difference between the two
rows is what that question costs, and `fields` is the floor under both: the same
reads and writes done directly

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

`dataclass`, `kwargs`, `slices`, `closure` and `sortkey` are the rows about
*syntax* rather than about a runtime primitive: the shapes ordinary python is
full of, each read against the row that measures the same work spelled out by
hand

- `dataclass` is `alloc` with the constructor deleted from the source and the
    field list left to build it. what differs between the two is a constructor
    the compiler never sees written, and `fields` is under both. it is also the
    one row here that declines: the decorator is moved to module init, which
    leaves `Pair` to its interpreted definition while the loop around it is
    compiled — so the row currently reports what it costs to build an
    interpreted object from compiled code, which is worth having a number for
- `kwargs` is `calls` reached through an argument list that is not simply
    positional: a `*args` tuple to pack, a `**kwargs` dict to build, a
    keyword-only parameter passed by name and another left to its default. same
    body, same count, so the difference is the boundary
- `slices` takes `a[i:j]`, `a[:n]` and `a[::2]` where `dot` and `chars` take
    single elements. a slice allocates and copies where an index does neither
- `closure` is `calls` with the callee reading and writing a name from the
    scope that made it, so it pays for the cell that name has to live in. it is
    deliberately not recursive — a closure that names itself keeps itself alive
    through its own cell, and that cycle is a leak rather than a cost
- `sortkey` is the only call in the set that python does not make: `sorted`
    holds the loop and reaches the key function from inside it. it uses
    `sorted` rather than `list.sort` so the input is not left ordered for the
    next round, and the input is shuffled by a fixed sequence in `setup`, since
    timsort over an ordered list makes n-1 comparisons and measures nothing

`accessors` is `props` written the way basedpython offers to write it — a
`var v: int = 0` with a `get`/`set` block under it, which the transpiler emits as
exactly the `@property` and `@v.setter` pair `props` writes by hand. the work the
two loops do is identical, so the difference between the rows is the surface
syntax and nothing else, and that is a question about the *backend* rather than
about the program: a lowering costs nothing only when it lands on a shape the
backend already knew

adding one is three files: the program, an entry in `programs.toml`, and a line
in the table above. the entry has to say what it measures that nothing else
does — and if the program has inputs to prepare, they go in a `setup()` and the
entry says `setup = true`

## a benchmark written in basedpython

for a long time every benchmark here was plain python, which left the language
this project exists to build entirely unmeasured. a program written as `.by` is
one, and nothing in `programs.toml` says so: the extension already decides how a
program is built, so it is what the harness reads. one name may not be on disk
both ways

such a row is a genuine three-way comparison rather than a two-way one:

| build     | what it runs                                   |
| --------- | ---------------------------------------------- |
| `cpython` | the python `by transpile` lowers the source to |
| `by`      | `by compile` over the `.by` source as written  |
| `mypyc`   | mypyc over that same transpiled python         |

so the row answers two questions at once — what the native backend makes of our
own syntax, and what that syntax costs against the python it replaces. that is
worth more than skipping mypyc, which is why a `.by` program is not a
`mypyc = false` one

the lowering happens once per program, before anything is staged and outside
every clock: it is a build step, like `setup()`, not work under measurement. the
python it produces is kept in the staging tree, because a row whose number is
surprising is one somebody will want to read rather than guess at. a lowering
that fails is a refusal — the alternative is a row quietly comparing two of its
four builds

both `by compile` and `by transpile` are given `--soundness none`, and the run's
metadata records it. that is not a speed switch, it is the same symmetry
`strict-float` buys: `by compile` emits no soundness checks into native code,
while the python the same source transpiles to gets an `isinstance` per checked
call under the default set. leaving them on would put a fixed cost on the
interpreted side of every quotient and inflate the speedup a `.by` row reports —
the `setup()` problem arriving through the lowering instead. it is a no-op for a
plain-python benchmark, since a `.py` source is its own interpreted fallback

that is worth a number, because it is large enough to change what a row *says*
rather than merely to widen it. `payloads` reads **0.90x** with the checks off
and **1.16x** with them on, both at floors under ±4% — the difference between
reporting that compiling that program makes it slower and reporting that it
makes it faster. `destructure` and `accessors` do not move, because both decline
the functions the checks would have sat inside

⚠️ **a `.by` row's decline count is the interesting half of it.** `accessors`
declines three functions where `props` declines none, which is to say that the
accessor block's whole class is left interpreted while the `@property` pair it
lowers to compiles. the pair reads **1.00x against 65x** because of it, on floors
of ±0.4% and ±4%. that is not a defect in the row, it is what the row is for, and
tuning such a program until its count reaches zero would delete the measurement

it is also worth knowing which *source* a decline is about. handing `by compile`
a `.by` and handing it the python that same `.by` transpiles to are not the same
compilation: `accessors` declines three functions as basedpython and none as the
python it lowers to, and `destructure` declines two and none. `payloads` goes the
other way, one against eight. so a `.by` row's declines are a statement about the
`.by` path in particular

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
