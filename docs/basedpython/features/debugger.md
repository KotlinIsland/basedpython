# debugging

a `.by` program debugs at its `.by` lines. `breakpoint()` needs nothing installed:

```by
data class Point:
    x: int
    y: int

def norm(p: Point) -> int:
    breakpoint()
    return p.x * p.x + p.y * p.y

print(norm(Point(3, 4)))
```

```console
$ by run main
> /app/main.by(6)norm()
-> breakpoint()
(Pdb) p p
Point(x=3, y=4)
(Pdb) n
> /app/main.by(7)norm()
-> return p.x * p.x + p.y * p.y
(Pdb) c
25
```

that is python's own `pdb`, and it reports `main.by` because that is what is running: `by run`
compiles each module under its `.by` path, with each line the `.by` line it came from. so a
traceback, a warning, `inspect` and a debugger all read the file you wrote. code the transpiler wrote
on its own has no `.by` line, and is line 0

for breakpoints in the editor, use [pycharm or intellij](#in-pycharm-and-intellij)

## in pycharm and intellij

with [the intellij plugin](intellij-plugin.md), set a breakpoint in the gutter of a `.by` file and
press Debug on a `by run` or test configuration. it stops on that line, and stepping, frames,
variables and evaluation all report `.by` sources. a `.py` module in the project takes breakpoints
too, and a stack through both shows each at its own file

the debugger behind that is a choice, under *Settings | Languages & Frameworks | basedpython*:

|                | [`bpd`](#bpd)                                 | `debugpy`                |
| -------------- | --------------------------------------------- | ------------------------ |
| default        | yes                                           | no                       |
| installing     | bundled with the plugin                       | `uv add --dev debugpy`   |
| python         | 3.13 or later                                 | what `debugpy` supports  |
| windows        | no: `by run` cannot start its launcher script | yes                      |
| a stale `.by`  | refused, naming the file                      | maps the line regardless |
| *Reset Frame*  | yes                                           | no                       |
| hot reload     | yes                                           | no                       |
| data flow      | yes                                           | no                       |
| why a rerender | yes                                           | no                       |

when `debugpy` is missing, the session says so, names the interpreter that could not import it and
offers to install it

- **log points**: a breakpoint that logs an expression instead of stopping. turn on *Breakpoints Over
    Line Numbers* in the gutter's right-click menu, under *Appearance*, then click between two line
    numbers or press `Ctrl+Alt+F8`. Alt+Enter on a `print(…)` offers to replace it with one
- **exception breakpoints**: *On termination* is on and *On raise* is off, under
    *Breakpoints | basedpython Exceptions*. checked exceptions do not make *On termination* redundant,
    since the checker does not model every exception: a `KeyError` from a dict lookup still ends a
    program. under `bpd`, *+* adds one for a single exception class and its subclasses
- **Reset Frame**: runs a stopped frame again from its first line, any frame on the stack. python
    cannot pop a frame, so this is not the JVM's: side effects already performed are not undone, and
    a `with` the frame was inside gets no `__exit__`
- **hot reload**: edit a file during a session, and the toolbar that appears replaces the code in the
    running process. it applies when every change is inside the body of a function that keeps its
    arguments, it applies to all of a file or none of it, and it is refused while any frame is running
    code it would change. stop in another function, or let that frame return
- **data flow**: while stopped, the branches below the stop line are answered as the `true` or `false`
    they will be, and code that will not run is greyed. off by default, beside the debugger choice. see
    [debugger facts](editor.md#debugger-facts)
- **why a rerender**: under a [basedpython-ui](../frameworks/basedpython-ui.md) program, the
    *basedpython Recompositions* tool window lists every composable that ran and why, the state write,
    argument or parent that made it

a `by build` or `by check` configuration has no program to debug

## `bpd`

`bpd` is a debugger for python and basedpython, written in rust on PEP 669, and it knows what a `.by`
file is. it is a project of its own,
[basedpython-debugger](https://github.com/KotlinIsland/basedpython-debugger), published on pypi under
that name, and early. the plugin bundles it. to have one outside the plugin:

```sh
uv add --dev basedpython-debugger
```

it needs cpython 3.13 or later, so a project whose `requires-python` allows anything older cannot add
it this way. `bpd doctor` answers for any interpreter:

```console
$ bpd doctor python3.13
interpreter  python3.13
executable   /usr/local/bin/python3.13
version      3.13.16 (cpython), minimum is 3.13.0
build        gil
extension    .cpython-313-darwin.so
PEP 669      `sys.monitoring` present
PEP 768      unavailable, needs python 3.14.0 or newer

this interpreter can be debugged
attaching to a running process is not built yet — see `PEP 768` above
```

| command      | what it is                                                              |
| ------------ | ----------------------------------------------------------------------- |
| `bpd dap`    | a debug adapter, for an editor: on stdin and stdout, or `--listen PORT` |
| `bpd mcp`    | the same session as an MCP server, for an ai agent                      |
| `bpd launch` | runs a program under the debugger, with nothing to drive it             |
| `bpd doctor` | whether an interpreter can be debugged, and why not                     |
| `bpd cache`  | what the agent staging cache is holding, and reclaiming it              |

before it maps a line, `bpd` checks a digest of both the `.by` and the python made from it, and a build
out of date with its source is refused rather than mapped to the wrong line:

```console
$ bpd launch --python python3.13 build/main.py
error: the basedpython build this program runs out of cannot be mapped
  caused by: the basedpython build in `build` is stale: `main.by` has changed since it was transpiled. every line the map reports about it would be wrong with total confidence, so bpd will not report one. transpile again and debug the build that comes out
```

a vs code extension and `nvim-dap` can drive `bpd dap` too. the extension is `editors/vscode/` in that
repository, loaded from a checkout rather than a marketplace. the vs code extension for basedpython
has no debugger of its own

## from another debugger

`by run --launcher PATH` starts the program through another program, without choosing the interpreter
for it, which is how a debugger becomes the process the program runs in. the launcher is run as
`PATH <interpreter> <runner> MODULE [ARGS...]`. with the `breakpoint()` gone from `main.by`, and this
as `debug.sh`:

```sh
#!/bin/sh
python="$1"
shift
exec bpd launch --python "$python" "$@"
```

```console
$ by run --launcher ./debug.sh main
25
```

to stop at a breakpoint, a launcher hands the same three things to `bpd dap`, as the `launch` request:

```json
{ "program": "<runner>", "args": ["main"], "python": "<interpreter>" }
```

and a breakpoint names the `.by` file as its source. the stack it reports is the program's, `norm`
and `<module>` at their `main.by` lines, without the runner beneath them. the plugin debugs a program
this way

`by run`'s tree lasts only as long as the program, so it cannot be named ahead of time. a debugger
that has to see what the runner's frames are, or which files are `by`'s rather than the program's, can
read them from the map beside the program: see
[code `by` writes for itself](../development/sourcemaps.md#code-by-writes-for-itself) and
[`--launcher`](../cli-reference.md#by-run)

## compiled modules

debug a program without `--compiled`. a [compiled](compilation.md) function has no python frame and
no python lines, so a debugger cannot stop inside it or read its locals, and `breakpoint()` in one
stops in the function that forwards to it instead:

```by
def add(a: int, b: int) -> int:
    breakpoint()
    return a + b
```

```console
$ by run --compiled main
> <by native forwarder>(29)add()
```

the rest of the stack still reads as usual, and a traceback through a compiled function still names
its `.by` file and line

## the python it became

to step through the generated python rather than the source, build it and debug that like any other
python:

```console
$ by build
$ python -m pdb build/main.py
> /app/build/main.py(1)<module>()
-> from dataclasses import dataclass
```

the plugin's *Debug .by (pdb)* action does the same, from a `.by` file

## for editors

an editor can ask the language server what a stopped program settles about the code below it, and
which expression the cursor is on: see [debugger facts](editor.md#debugger-facts) and
[the expression under the cursor](editor.md#the-expression-under-the-cursor)
