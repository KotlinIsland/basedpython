# the repl

`by repl` is an interactive session in which you type basedpython. each input is checked and
transpiled with everything the session has defined so far, and runs in one python process
that lives as long as the session

```console
$ by repl
>>> let rate = 0.2
>>> def taxed(amount: float) -> float:
...     return amount * (1 + rate)
...
>>> taxed(10.0)
12.0
>>> rate = 0.3
error[invalid-assignment]: read-only symbol `rate` cannot be reassigned
 --> <by-input-4>:1:1
  |
1 | rate = 0.3
  | ^^^^^^^^^^ reassigned here
  |
 ::: <by-input-1>:1:1
  |
1 | let rate = 0.2
  | ---- declared read-only here
```

it starts where `by run` would: in the project around the working directory, on the
project's interpreter (`--python` names another), with the project's modules staged as `by run` stages them. so `import app.pricing` imports the project's `.by` module, transpiled, and an
error raised inside it is reported at its `.by` line. like `by run`, it does not start while
the project has check errors

## a session is a module

the inputs that ran, in order, are the source of one module, and each new input is that
module's continuation. it is checked as such: a name defined two inputs ago is defined, an
import is in scope, a `let` stays final, an extension declared earlier applies, and the
checker's answer for the input is the one it would give for the same lines at the end of a
file. what this means in practice:

- an input with check errors does not run, and is not part of the session: it is as if it had
    not been typed. its errors are reported against the input, as `<by-input-N>` and the
    input's own lines
- warnings are not reported. an import that nothing uses yet is not unused, it is unused *so
    far*, and an expression whose value is discarded is the value the repl prints
- an input that raised is part of the session: the statements before the one that raised
    have run, and what they bound is bound, as in python's repl
- a name can be assigned again, declared again with another type, and a function or a class
    defined again, as they can further down a module. a `let` cannot, since it is final
- the session's module is the project's own code, so a `sealed` class declared in one input
    can be subclassed in a later one

what runs is the python written for the input: the module made of the session and the input
is transpiled as a whole, and the statements transpiled from the input's lines run, after
any runtime helper they need that has not run yet. the earlier inputs' statements ran when
they were entered. an input that would change the python an earlier input was transpiled to —
which has already run, and cannot run again differently — is refused, and says so

two things differ from `by run`, because a session is never run as a whole. a `from` import
binds its names when it is entered rather than at their first use, so a name that cannot be
imported says so then, and a `def main` is a function like any other: nothing calls it

## starting in a module

`by repl --module app.main` starts the session in a module of the project, named as `by run`
takes one — what an editor's *run with console* does. the module runs first, as `__main__`, with
the `__file__`, package and spec `by run` gives it, and the session goes on from there with its
names. its source is the session's first: each input is checked and transpiled as the module's
continuation, as the module itself, so what it defines is defined, a `let` of its stays
read-only, and a relative import resolves, in the module and in an input

```by
# app/main.by
from .helpers import double

let base = 21
answer = double(base)
print(f"answer is {answer}")
```

```console
$ by repl --module app.main
answer is 42
>>> answer + 1
43
>>> base = 1
error[invalid-assignment]: read-only symbol `base` cannot be reassigned
 --> <by-input-2>:1:1
  |
1 | base = 1
  | ^^^^^^^^ reassigned here
  |
 ::: app/main.by:3:1
  |
3 | let base = 21
  | ---- declared read-only here
```

it runs as the start of a session, not as a program: its `def main` is a function like any
other, which nothing calls — `main()` is one input away — and a value its last line computes is
not printed. a diagnostic or a traceback that points into it names its file and line. a module
the project does not have, or one whose python cannot run as a session's start, keeps the
session from starting

## what is printed

each expression statement of an input prints its value, through `sys.displayhook`, as python's
repl prints it: both of `1; 2`, and one in a loop's body, but not one in a function's. an input is numbered whether it runs or not, as python numbers its own. an
exception prints a traceback whose frames name the
input they ran in, at the input's line, with the line quoted, and the project's modules at
their `.by` lines

```console
>>> def ratio(a: int, b: int) -> float:
...     return a / b
...
>>> ratio(1, 0)
Traceback (most recent call last):
  File "<by-input-2>", line 1, in <module>
    ratio(1, 0)
    ~~~~~^^^^^^
  File "<by-input-1>", line 2, in ratio
    return a / b
ZeroDivisionError: division by zero
```

## input

on a terminal, prompts are `>>>` and `...`, and lines are read the way python's repl reads them:
a line that completes a statement runs it, a compound statement runs at the blank line after
it, and a line that cannot be the start of anything runs at once, to report what is wrong with
it. whether an input is complete is a question about basedpython's grammar, so `by` answers it
— the same answer the [language server](#in-an-editor) gives

the end of input (ctrl-d) ends the session, and its exit status is the program's: 0, or what
`sys.exit` was given, or, when a signal ends the interpreter, 128 and the signal's number. ctrl-c abandons the input being typed, or interrupts the one running with
a `KeyboardInterrupt`, which is printed, and the session carries on

an input can also be sent whole, framed as a terminal frames a paste: `ESC [200~` before it and
`ESC [201~` after it. the text between the two is one input whatever it holds, blank lines
inside a function body included, and runs when the closing marker arrives. this is how an
editor sends one

while an input runs, the lines that arrive are the program's: `input()` reads them, as it reads
python's, and an editor is told when that is with [`--events`](#in-an-editor)

## in an editor

an editor's console runs `by repl` as a process and talks to it through its standard streams:

- **sending.** each input is written to `by repl`'s standard input framed, as above, followed by
    a newline. a line for the program — what `input()` reads — is written as it is, unframed

- **events.** over a pipe a console cannot tell whether `by repl` is waiting for an input or the
    program is reading standard input, and the two are sent differently. `--events PORT` has
    `by repl` tell it, on a connection of its own that program output cannot mix with: it
    connects to `PORT` on the loopback interface (`127.0.0.1`) before the session starts, and
    writes one line of json there for each event. the first line is
    `{"token": T}`, where `T` is the value of `BY_REPL_EVENTS_TOKEN` in `by repl`'s environment
    (`null` when it is unset), for the console to know the connection is the session's; the
    program does not see the variable. then:

    | event                         | meaning                                                                                                                                                                                                                        |
    | ----------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
    | `{"reading": "input"}`        | `by repl` waits for the next input: send one, framed                                                                                                                                                                           |
    | `{"reading": "continuation"}` | an input sent unframed is not whole yet, and `by repl` waits for its next line                                                                                                                                                 |
    | `{"reading": "program"}`      | the program reads standard input — `input()`, `sys.stdin` or `sys.stdin.buffer` — and the next line is its own: send it unframed                                                                                               |
    | `{"finished": N, "ran": R}`   | input `N`, as it is numbered in `<by-input-N>`, has finished. `R` is `true` when it ran, which makes it part of the session even when it raised, and `false` when it did not: it was refused, or interrupted before it started |

    `reading` is sent each time standard input is read for that purpose, and `finished` before
    the `reading` that follows it. what the session printed before an event is in its pipes before
    the event is sent. a line sent while nothing is reading waits for whatever reads next, as
    typed-ahead text does in a terminal; a thread of the program's that reads standard input
    while the session waits for an input reads lines meant for either. the connection closes
    when the session ends

- **output.** the program's output, the printed values and the tracebacks are the process's
    standard output and standard error, as they are python's. prompts are written only when
    standard input is a terminal, so a console that reads a pipe shows none

- **interrupting.** `by repl` itself ignores `SIGINT`, so a console interrupts the input that is
    running by sending `SIGINT` to the process tree, as a terminal sends it to the process group

- **enter.** whether enter runs the input or starts a new line is the language server's
    `by/inputStatus` request, given the input's text:

    ```json
    { "text": "def taxed(amount: float) -> float:\n" }
    ```

    ```json
    { "status": "incomplete" }
    ```

    `complete` — it can run; `incomplete` — it needs more lines: a compound statement whose body
    could still go on, an open bracket, an unterminated triple-quoted string, a line ending in
    `\`; `invalid` — no more lines can make it valid, and running it reports why. blank input,
    and input of comments only, is `complete`

- **completion.** `--session-file PATH` keeps `PATH` holding the session's source — the module
    it started in, if it started in one, then the inputs that ran, in order, each ending in a
    newline — from the moment the session starts. completion
    in the console is the language server's ordinary completion, asked of a document whose text
    is that file's followed by the input being typed, at the caret's offset past the file's
    length: the same module the session checks the input as, so what is offered is what the
    session has defined
