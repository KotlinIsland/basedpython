## What it does

Checks for imports of an installed distribution the project never declared a
dependency on.

## Why is this bad?

The import only works because something else the project depends on happened to
pull the distribution in. Nothing keeps that true: the dependency that brought it
can drop it, or move to a version that no longer needs it, and then the import
fails for everyone installing the project fresh — while continuing to work in the
environment it was written in.

Depending on something means saying so.

## Examples

`requests` installs `charset_normalizer`, but a project that only declares
`requests` has not declared `charset_normalizer`:

```toml {data-mdtest="ignore"}
[project]
name = "my-lib"
dependencies = ["requests"]
```

```python {data-mdtest="ignore"}
import charset_normalizer  # error: [undeclared-dependency]
```

Adding it to `[project].dependencies` fixes the import. In an editor this is
offered as a quick fix.

## Configuration

The check is only made when the project says what it depends on: a project with
no `[project]` table, an environment with no install metadata, and a module ty
cannot attribute to a distribution are all cases where nothing is reported.
