# the intellij plugin

the basedpython plugin makes `.by` a first-class language in PyCharm and IntelliJ IDEA. install it
from *Settings | Plugins | Marketplace* by searching for `basedpython`, open a project with a `.by`
file in it, and the file is checked, completed, navigable, runnable and debuggable

the plugin carries its own `by`, `buff` and `bpd`, so there is nothing else to install. the exception
is debugging on windows, which [needs `debugpy`](debugger.md#in-pycharm-and-intellij)

## installing

- **from the IDE**: *Settings | Plugins | Marketplace*, search for `basedpython`. the listing is
    [on JetBrains Marketplace](https://plugins.jetbrains.com/plugin/33922-basedpython), which hands your IDE the build for its operating system and CPU
- **from disk**: each [release](https://github.com/KotlinIsland/basedpython-intellij/releases) has one
    zip per platform, which installs with *Settings | Plugins | ⚙ | Install Plugin from Disk*

## what it needs

the IDE has to be **IntelliJ IDEA Ultimate or PyCharm Professional on the 2026.3 platform**, build
263.7307 or newer. earlier versions are refused, and so are the Community editions, because the
language server support the plugin is built on is only in the paid IDEs

it runs `by` and `buff` from the first of these it finds:

1. the path set in *Settings | Languages & Frameworks | basedpython*
1. a `.venv` found by walking up from the project (`.venv/bin/by`, or `.venv/Scripts/by.exe` on
    windows)
1. the virtual environment behind the project's python interpreter
1. `~/.basedpython/bin`, where the plugin's download action puts them
1. the ones bundled with the plugin
1. `PATH`

a project that [installs basedpython](../getting-started.md#installation) therefore gets the version it
pins. a run configuration can pin one source instead, under *Environment*, and then fails rather than
falling back to another. when none is found, a banner offers to install it with uv

a project is treated as basedpython's when it has an `api.lock`, a `basedpython.toml`, a top-level
`.by` or `.byi`, or a `pyproject.toml` that mentions basedpython

in PyCharm a `.py` stays with the Python plugin, even in a basedpython project. *Apply to .py files*,
on the settings page, changes that

## what it gives you

### editing

- everything the [language server](editor.md) answers: diagnostics, completion, hover, go to
    definition, find usages, rename, inlay hints, semantic highlighting, the outline, code actions
- formatting and lint fixes through `buff`, with format on save and import optimizing
- refactorings with the reason when `by` refuses: *Move*, *Change Signature*, *Pull Members Up*,
    *Push Members Down*, *Extract Superclass*, *Introduce Parameter* and *Introduce Field*
- *Override Methods*, *Implement Methods*, *Go to Test*, *Method Hierarchy*, and usage and inheritor
    counts above each declaration
- each inlay hint kind is *never*, *always* or **push-to-hint**: drawn only while you hold a key,
    `Ctrl+Alt` by default
- a docstring rendered as formatted prose in place of its source, with *Render documentation comments*
- postfix and live templates, file templates under *New | basedpython File*, surround-with, and a
    *New Project* wizard

### running

- `by run`, `by build` and `by check` run configurations, with gutter icons on a `main` and on
    `if __name__ == "__main__":`
- **arguments for `main`**: running a [`def main(a: int)`](main-function.md) that needs arguments asks
    for them in a form generated from its signature
- pytest through `by run pytest`, with results in the test tree at their `.by` lines, gutter icons on
    each test and *Rerun Failed Tests*
- *Run with Coverage*, shown in the gutter and the Coverage tool window
- *Build Project* runs `by build`, with each diagnostic at its `.by` line in the Build tool window
- **basedpython Console**: [`by repl`](repl.md), an input at a time, and Alt+Shift+E sends a selection
    to it
- `by run` configurations run on Docker, SSH and WSL targets

### debugging

[breakpoints in `.by` files](debugger.md#in-pycharm-and-intellij), stepping and variables at `.by`
lines, log points, exception breakpoints, *Reset Frame*, hot reload, and why a
[basedpython-ui](../frameworks/basedpython-ui.md) composable ran

### the project

- **basedpython Environment** tool window: whether there is an environment, whether it matches what
    the project declares, its dependencies as a tree grouped by where they are declared, and adding,
    removing and syncing them. **Set Up** fixes the one problem it names. it works through uv, which it
    installs into `~/.basedpython/bin` if it is missing
- **Modules**, under the settings page: a uv workspace's members, created, changed, renamed and
    removed there
- **basedpython Tasks** tool window: the hooks of a `.pre-commit-config.yaml` or `lefthook.yml`, and
    the aliases of `[tool.pyprojectx.aliases]`, each runnable from the list
- *Tools | basedpython*: *Transpile to .py*, *Reverse Transpile to .by*, *Generate api.lock*,
    *Diff api.lock*, *Check Project*, *Explain Rule...*
- *Go to Related* jumps between a `.by` and the `.py` generated from it

## settings

most of it is on *Settings | Languages & Frameworks | basedpython*: binary paths with a **Test**
button, per-server toggles, extra arguments, format on save, inlay hints and the debugger. the colours
are under *Settings | Editor | Color Scheme | basedpython*, and each of `by`'s rules is an inspection
under *Settings | Editor | Inspections | basedpython | by rules*, whose switch and severity are sent
to `by`
