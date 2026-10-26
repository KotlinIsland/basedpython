# the repl

> **status: design.** `by/inputStatus` is built; `by repl` itself is being built, and lands in
> the commits that follow

`by repl` is an interactive session in which you type basedpython. each input is checked and
transpiled with everything the session has defined so far, and runs in one python process
that lives as long as the session

```console
$ by repl
>>> let rate = 0.2
>>> def taxed(amount: float) -> float:
...     return amount * (1 + rate)
...
>>> taxed(10)
12.0
>>> rate = 0.3
  --> <by-input-4>:1:1
error[invalid-assignment]: Reassignment of `Final` symbol `rate` is not allowed
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
- a name declared with a type cannot be declared again with another, since one module cannot
    do that. a name assigned without a declaration can be assigned anything

what runs is the python written for the input: the module made of the session and the input
is transpiled as a whole, and the statements transpiled from the input's lines run, after
any runtime helper they need that has not run yet. the earlier inputs' statements ran when
they were entered. an input that would change the python an earlier input was transpiled to —
which has already run, and cannot run again differently — is refused, and says so

two things differ from `by run`, because a session is never run as a whole. an `import` runs
when it is entered rather than lazily, so a module that cannot be imported says so then, and a
`def main` is a function like any other: nothing calls it

## what is printed

an input that ends in an expression prints its value, through `sys.displayhook`, as python's
repl prints it. an exception prints a traceback whose frames name the
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
`sys.exit` was given. ctrl-c abandons the input being typed, or interrupts the one running with
a `KeyboardInterrupt`, which is printed, and the session carries on

an input can also be sent whole, framed as a terminal frames a paste: `ESC [200~` before it and
`ESC [201~` after it. the text between the two is one input whatever it holds, blank lines
inside a function body included, and runs when the closing marker arrives. this is how an
editor sends one

while an input runs, the lines that arrive are the program's: `input()` reads them, as it reads
python's

## in an editor

an editor's console runs `by repl` as a process and talks to it through its standard streams:

- **sending.** each input is written to `by repl`'s standard input framed, as above, followed by
    a newline. while the program reads `input()`, a line is written as it is

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

- **completion.** `--session-file PATH` keeps `PATH` holding the session's source — the inputs
    that ran, in order, each ending in a newline — from the moment the session starts. completion
    in the console is the language server's ordinary completion, asked of a document whose text
    is that file's followed by the input being typed, at the caret's offset past the file's
    length: the same module the session checks the input as, so what is offered is what the
    session has defined
