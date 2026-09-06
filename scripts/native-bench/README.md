# native-bench

```sh
cargo build --bin by
uv run --no-project --python 3.13 python scripts/native-bench/bench.py
uv run --no-project --python 3.13 python scripts/native-bench/bench.py --self-check
```

times each benchmark four ways — interpreted, `by compile`, `by compile` again,
and mypyc — and reports paired ratios with the noise floor those two identical
builds measured for themselves

a benchmark written as `.by` rather than `.py` is a basedpython one: `by compile`
takes its source as written, while the interpreted and mypyc builds run the
python `by transpile` lowers it to, produced outside every clock

the method, the guardrails and the reason for every benchmark in the set are in
[the docs](../../docs/basedpython/development/compilation/benchmarks.md). read
that before changing anything here, and before quoting a number out of it

`--python` takes anything `uv python find` accepts, including a free-threaded
build (`3.13t`, `3.14t`) — the version is recorded in the run's metadata and a
baseline from a different one warns before it compares
