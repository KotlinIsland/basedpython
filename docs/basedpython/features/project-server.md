# the project server

`by check` asks a running [language server](editor.md) for the project's diagnostics before
checking for itself, and says so when it does:

```console
$ by check
using project server information
All checks passed!
```

a server that has the project open has already parsed and inferred it, and has been keeping
that current ever since. on a project of about twelve thousand files that is the difference
between four seconds and under one

## what it needs

the server has to be checking the whole project, which is not what an editor asks for by
default — the default is only the files you have open. a server in that mode has never looked
at the rest of the project, so it has no answer to give and does not pretend to:

```json
{ "ty.diagnosticMode": "workspace" }
```

this is the same setting that decides whether errors in files you have not opened appear in
your editor's problems list, so it is one you want anyway if you want this

## it is the same answer, or it is no answer

what comes back is what your own process would have computed. the server refuses whenever
that might not be true, and a refusal is not a failure — the check simply runs the way it
always did, and you see no message

it refuses when the two are different builds of `by`, when they resolved different
configuration, when they resolved different environments, when a file open in the editor has
unsaved changes, and when the session is being typed into so fast that the check keeps being
cancelled

**any flag that changes the check is a different check.** `--python-version`,
`--error-on-warning`, `--output-format` and the rest all land in the configuration the two
compare, so a `by check` carrying one of them is always run in full. so is a `-v` check, whose
diagnostics carry an explanation of where each rule was turned on that a server's do not. and
`by check` does not ask at all when the invocation is not a whole-project check: `--watch`,
`--fix`, `--add-ignore`, or a check pointed at particular paths

the answer describes the project as it is on disk, not as your editor last noticed it — a
`git checkout` your editor missed does not go unseen

### finding out which one it was

every refusal is logged. `-v` will not show you, because a verbose check is itself a reason to
refuse; ask for the log directly instead:

```console
$ TY_LOG=ty=debug by check
```

## turning it off

`--no-server` checks from scratch:

```console
$ by check --no-server
```

`BY_NO_PROJECT_SERVER=1` does the same, and does it on both halves: a command line with it set
checks for itself, and a server started with it set does not open the socket that makes it
reachable at all

```console
$ BY_NO_PROJECT_SERVER=1 by check
```

## what it opens

a server listens on a loopback port and writes a record of itself — the port and a random
secret — into a per-user directory under ty's cache, readable only by you. a request without
the secret is not answered, and an answer that does not repeat the secret back is not believed

the record is removed when the server shuts down. one whose server was killed outright stays
behind until the next `by check` finds nothing listening and clears it away
