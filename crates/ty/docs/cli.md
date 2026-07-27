<!-- WARNING: This file is auto-generated (cargo dev generate-all). Edit the doc comments in 'crates/ty/src/args.rs' if you want to change anything here. -->

# CLI Reference

## by

an extremely fast Python type checker, with basedpython support

<h3 class="cli-reference">Usage</h3>

```
by <COMMAND>
```

<h3 class="cli-reference">Commands</h3>

<dl class="cli-reference"><dt><a href="#by-check"><code>by check</code></a></dt><dd><p>Check a project for type errors</p></dd>
<dt><a href="#by-server"><code>by server</code></a></dt><dd><p>Start the language server</p></dd>
<dt><a href="#by-version"><code>by version</code></a></dt><dd><p>Display ty's version</p></dd>
<dt><a href="#by-explain"><code>by explain</code></a></dt><dd><p>Explain rules and other parts of ty</p></dd>
<dt><a href="#by-run"><code>by run</code></a></dt><dd><p>Transpile and run a module with <code>python -m &lt;module&gt;</code></p></dd>
<dt><a href="#by-build"><code>by build</code></a></dt><dd><p>Transpile all .by files and write them to out/</p></dd>
<dt><a href="#by-generate-api-file"><code>by generate-api-file</code></a></dt><dd><p>Generate an api lockfile (<code>api.lock</code>) summarising the public type-level surface of the project</p></dd>
<dt><a href="#by-transpile"><code>by transpile</code></a></dt><dd><p>Transpile a file to stdout, or a whole directory in place (reads stdin if no path given)</p></dd>
<dt><a href="#by-help"><code>by help</code></a></dt><dd><p>Print this message or the help of the given subcommand(s)</p></dd>
</dl>

## by check

Check a project for type errors

<h3 class="cli-reference">Usage</h3>

```
by check [OPTIONS] [PATH]...
```

<h3 class="cli-reference">Arguments</h3>

<dl class="cli-reference"><dt id="by-check--paths"><a href="#by-check--paths"><code>PATHS</code></a></dt><dd><p>List of files or directories to check [default: the project root]</p>
</dd></dl>

<h3 class="cli-reference">Options</h3>

<dl class="cli-reference"><dt id="by-check--add-ignore"><a href="#by-check--add-ignore"><code>--add-ignore</code></a></dt><dd><p>Adds <code>ty: ignore</code> comments to suppress all rule diagnostics</p>
</dd><dt id="by-check--color"><a href="#by-check--color"><code>--color</code></a> <i>when</i></dt><dd><p>Control when colored output is used</p>
<p>Possible values:</p>
<ul>
<li><code>auto</code>:  Display colors if the output goes to an interactive terminal</li>
<li><code>always</code>:  Always display colors</li>
<li><code>never</code>:  Never display colors</li>
</ul></dd><dt id="by-check--config"><a href="#by-check--config"><code>--config</code></a>, <code>-c</code> <i>config-option</i></dt><dd><p>A TOML <code>&lt;KEY&gt; = &lt;VALUE&gt;</code> pair (such as you might find in a <code>ty.toml</code> configuration file)
overriding a specific configuration option.</p>
<p>Overrides of individual settings using this option always take precedence
over all configuration files.</p>
</dd><dt id="by-check--config-file"><a href="#by-check--config-file"><code>--config-file</code></a> <i>path</i></dt><dd><p>The path to a <code>ty.toml</code> file to use for configuration.</p>
<p>While ty configuration can be included in a <code>pyproject.toml</code> file, it is not allowed in this context.</p>
<p>May also be set with the <code>TY_CONFIG_FILE</code> environment variable.</p></dd><dt id="by-check--error"><a href="#by-check--error"><code>--error</code></a> <i>rule</i></dt><dd><p>Treat the given rule as having severity 'error'. Can be specified multiple times. Use 'all' to apply to all rules.</p>
</dd><dt id="by-check--error-on-warning"><a href="#by-check--error-on-warning"><code>--error-on-warning</code></a></dt><dd><p>Use exit code 1 if there are any warning-level diagnostics.</p>
<p>Cannot be used in combination with <code>--exit-zero</code> or <code>--exit-zero-on-warning</code>.</p>
</dd><dt id="by-check--exclude"><a href="#by-check--exclude"><code>--exclude</code></a> <i>exclude</i></dt><dd><p>Glob patterns for files to exclude from type checking.</p>
<p>Uses gitignore-style syntax to exclude files and directories from type checking. Supports patterns like <code>tests/</code>, <code>*.tmp</code>, <code>**/__pycache__/**</code>.</p>
</dd><dt id="by-check--exit-zero"><a href="#by-check--exit-zero"><code>--exit-zero</code></a></dt><dd><p>Always use exit code 0, even when there are error-level diagnostics.</p>
<p>Cannot be used in combination with <code>--error-on-warning</code>.</p>
</dd><dt id="by-check--exit-zero-on-warning"><a href="#by-check--exit-zero-on-warning"><code>--exit-zero-on-warning</code></a></dt><dd><p>Use exit code 0 if there are no error-level diagnostics.</p>
<p>Cannot be used in combination with <code>--error-on-warning</code>.</p>
</dd><dt id="by-check--extra-search-path"><a href="#by-check--extra-search-path"><code>--extra-search-path</code></a> <i>path</i></dt><dd><p>Additional path to use as a module-resolution source (can be passed multiple times).</p>
<p>This is an advanced option that should usually only be used for first-party or third-party modules that are not installed into your Python environment in a conventional way. Use <code>--python</code> to point ty to your Python environment if it is in an unusual location.</p>
</dd><dt id="by-check--fix"><a href="#by-check--fix"><code>--fix</code></a></dt><dd><p>Apply fixes to resolve errors</p>
</dd><dt id="by-check--force-exclude"><a href="#by-check--force-exclude"><code>--force-exclude</code></a></dt><dd><p>Enforce exclusions, even for paths passed to ty directly on the command-line. Use <code>--no-force-exclude</code> to disable</p>
</dd><dt id="by-check--help"><a href="#by-check--help"><code>--help</code></a>, <code>-h</code></dt><dd><p>Print help (see a summary with '-h')</p>
</dd><dt id="by-check--ignore"><a href="#by-check--ignore"><code>--ignore</code></a> <i>rule</i></dt><dd><p>Disables the rule. Can be specified multiple times. Use 'all' to apply to all rules.</p>
</dd><dt id="by-check--no-progress"><a href="#by-check--no-progress"><code>--no-progress</code></a></dt><dd><p>Hide all progress outputs.</p>
<p>For example, spinners or progress bars.</p>
</dd><dt id="by-check--output-format"><a href="#by-check--output-format"><code>--output-format</code></a> <i>output-format</i></dt><dd><p>The format to use for printing diagnostic messages</p>
<p>May also be set with the <code>TY_OUTPUT_FORMAT</code> environment variable.</p><p>Possible values:</p>
<ul>
<li><code>full</code>:  Print diagnostics verbosely, with context and helpful hints (default)</li>
<li><code>concise</code>:  Print diagnostics concisely, one per line</li>
<li><code>gitlab</code>:  Print diagnostics in the JSON format expected by GitLab Code Quality reports</li>
<li><code>github</code>:  Print diagnostics in the format used by GitHub Actions workflow error annotations</li>
<li><code>junit</code>:  Print diagnostics as a JUnit-style XML report</li>
</ul></dd><dt id="by-check--project"><a href="#by-check--project"><code>--project</code></a> <i>project</i></dt><dd><p>Run the command within the given project directory.</p>
<p>All <code>pyproject.toml</code> files will be discovered by walking up the directory tree from the given project directory, as will the project's virtual environment (<code>.venv</code>) unless the <code>venv-path</code> option is set.</p>
<p>Other command-line arguments (such as relative paths) will be resolved relative to the current working directory.</p>
</dd><dt id="by-check--python"><a href="#by-check--python"><code>--python</code></a>, <code>--venv</code> <i>path</i></dt><dd><p>Path to your project's Python environment or interpreter.</p>
<p>ty uses your Python environment to resolve third-party imports in your code.</p>
<p>This can be a path to:</p>
<ul>
<li>A Python interpreter, e.g. <code>.venv/bin/python3</code> - A virtual environment directory, e.g. <code>.venv</code> - A system Python <a href="https://docs.python.org/3/library/sys.html#sys.prefix"><code>sys.prefix</code></a> directory, e.g. <code>/usr</code></li>
</ul>
<p>If you're using a project management tool such as uv or you have an activated Conda or virtual environment, you should not generally need to specify this option.</p>

</dd><dt id="by-check--python-platform"><a href="#by-check--python-platform"><code>--python-platform</code></a>, <code>--platform</code> <i>platform</i></dt><dd><p>Target platform to assume when resolving types.</p>
<p>This is used to specialize the type of <code>sys.platform</code> and will affect the visibility of platform-specific functions and attributes. If the value is set to <code>all</code>, no assumptions are made about the target platform. If unspecified, the current system's platform will be used.</p>
</dd><dt id="by-check--python-version"><a href="#by-check--python-version"><code>--python-version</code></a>, <code>--target-version</code> <i>version</i></dt><dd><p>Python version to assume when resolving types.</p>
<p>The Python version affects allowed syntax, type definitions of the standard library, and type definitions of first- and third-party modules that are conditional on the Python version.</p>
<p>If a version is not specified on the command line or in a configuration file, ty will try the following techniques in order of preference to determine a value: 1. Check for the <code>project.requires-python</code> setting in a <code>pyproject.toml</code> file and use the minimum version from the specified range 2. Check for an activated or configured Python environment and attempt to infer the Python version of that environment 3. Fall back to the latest stable Python version supported by ty (see <code>ty check --help</code> output)</p>
<p>Possible values:</p>
<ul>
<li><code>3.7</code></li>
<li><code>3.8</code></li>
<li><code>3.9</code></li>
<li><code>3.10</code></li>
<li><code>3.11</code></li>
<li><code>3.12</code></li>
<li><code>3.13</code></li>
<li><code>3.14</code></li>
<li><code>3.15</code></li>
</ul></dd><dt id="by-check--quiet"><a href="#by-check--quiet"><code>--quiet</code></a>, <code>-q</code></dt><dd><p>Use quiet output (or <code>-qq</code> for silent output)</p>
</dd><dt id="by-check--respect-ignore-files"><a href="#by-check--respect-ignore-files"><code>--respect-ignore-files</code></a></dt><dd><p>Respect file exclusions via <code>.gitignore</code> and other standard ignore files. Use <code>--no-respect-ignore-files</code> to disable</p>
</dd><dt id="by-check--typeshed"><a href="#by-check--typeshed"><code>--typeshed</code></a>, <code>--custom-typeshed-dir</code> <i>path</i></dt><dd><p>Custom directory to use for stdlib typeshed stubs</p>
</dd><dt id="by-check--verbose"><a href="#by-check--verbose"><code>--verbose</code></a>, <code>-v</code></dt><dd><p>Use verbose output (or <code>-vv</code> and <code>-vvv</code> for more verbose output)</p>
</dd><dt id="by-check--warn"><a href="#by-check--warn"><code>--warn</code></a> <i>rule</i></dt><dd><p>Treat the given rule as having severity 'warn'. Can be specified multiple times. Use 'all' to apply to all rules.</p>
</dd><dt id="by-check--watch"><a href="#by-check--watch"><code>--watch</code></a>, <code>-W</code></dt><dd><p>Watch files for changes and recheck files related to the changed files</p>
</dd></dl>

## by server

Start the language server

<h3 class="cli-reference">Usage</h3>

```
by server
```

<h3 class="cli-reference">Options</h3>

<dl class="cli-reference"><dt id="by-server--help"><a href="#by-server--help"><code>--help</code></a>, <code>-h</code></dt><dd><p>Print help</p>
</dd></dl>

## by version

Display ty's version

<h3 class="cli-reference">Usage</h3>

```
by version [OPTIONS]
```

<h3 class="cli-reference">Options</h3>

<dl class="cli-reference"><dt id="by-version--help"><a href="#by-version--help"><code>--help</code></a>, <code>-h</code></dt><dd><p>Print help</p>
</dd><dt id="by-version--output-format"><a href="#by-version--output-format"><code>--output-format</code></a> <i>output-format</i></dt><dd><p>The format in which to display the version information</p>
<p>[default: text]</p><p>Possible values:</p>
<ul>
<li><code>text</code></li>
<li><code>json</code></li>
</ul></dd></dl>

## by generate-shell-completion

Generate shell completion

<h3 class="cli-reference">Usage</h3>

```
by generate-shell-completion <SHELL>
```

<h3 class="cli-reference">Arguments</h3>

<dl class="cli-reference"><dt id="by-generate-shell-completion--shell"><a href="#by-generate-shell-completion--shell"><code>SHELL</code></a></dt></dl>

<h3 class="cli-reference">Options</h3>

<dl class="cli-reference"><dt id="by-generate-shell-completion--help"><a href="#by-generate-shell-completion--help"><code>--help</code></a>, <code>-h</code></dt><dd><p>Print help</p>
</dd></dl>

## by explain

Explain rules and other parts of ty

<h3 class="cli-reference">Usage</h3>

```
by explain <COMMAND>
```

<h3 class="cli-reference">Commands</h3>

<dl class="cli-reference"><dt><a href="#by-explain-rule"><code>by explain rule</code></a></dt><dd><p>Explain a rule (or all rules)</p></dd>
<dt><a href="#by-explain-help"><code>by explain help</code></a></dt><dd><p>Print this message or the help of the given subcommand(s)</p></dd>
</dl>

### by explain rule

Explain a rule (or all rules)

<h3 class="cli-reference">Usage</h3>

```
by explain rule [OPTIONS] [RULE]
```

<h3 class="cli-reference">Arguments</h3>

<dl class="cli-reference"><dt id="by-explain-rule--rule"><a href="#by-explain-rule--rule"><code>RULE</code></a></dt><dd><p>Rule to explain</p>
<p>Defaults to all rules if omitted.</p>
</dd></dl>

<h3 class="cli-reference">Options</h3>

<dl class="cli-reference"><dt id="by-explain-rule--help"><a href="#by-explain-rule--help"><code>--help</code></a>, <code>-h</code></dt><dd><p>Print help (see a summary with '-h')</p>
</dd><dt id="by-explain-rule--output-format"><a href="#by-explain-rule--output-format"><code>--output-format</code></a> <i>output-format</i></dt><dd><p>Output format</p>
<p>[default: text]</p><p>Possible values:</p>
<ul>
<li><code>text</code></li>
<li><code>json</code></li>
</ul></dd></dl>

### by explain help

Print this message or the help of the given subcommand(s)

<h3 class="cli-reference">Usage</h3>

```
by explain help [COMMAND]
```

## by run

Transpile and run a module with `python -m <module>`

<h3 class="cli-reference">Usage</h3>

```
by run [OPTIONS] <MODULE> [ARGS]...
```

<h3 class="cli-reference">Arguments</h3>

<dl class="cli-reference"><dt id="by-run--module"><a href="#by-run--module"><code>MODULE</code></a></dt><dd><p>module to run (e.g. <code>by run main</code> looks for main.by)</p>
</dd><dt id="by-run--args"><a href="#by-run--args"><code>ARGS</code></a></dt><dd><p>arguments forwarded to the program, as <code>sys.argv[1:]</code></p>
</dd></dl>

<h3 class="cli-reference">Options</h3>

<dl class="cli-reference"><dt id="by-run--help"><a href="#by-run--help"><code>--help</code></a>, <code>-h</code></dt><dd><p>Print help</p>
</dd><dt id="by-run--min-version"><a href="#by-run--min-version"><code>--min-version</code></a> <i>version</i></dt><dd><p>minimum Python version the output must run on [default: the version of the interpreter that will run it]</p>
</dd><dt id="by-run--no-checked-cast"><a href="#by-run--no-checked-cast"><code>--no-checked-cast</code></a></dt><dd><p>lower <code>&lt;value&gt; cast &lt;type&gt;</code> to an unchecked <code>typing.cast</code> instead of a runtime-checked cast that raises on a type mismatch (the <code>cast?</code> safe form is unaffected)</p>
</dd><dt id="by-run--runtime-raises-checks"><a href="#by-run--runtime-raises-checks"><code>--runtime-raises-checks</code></a></dt><dd><p>wrap every function with a <code>raises</code> clause in a runtime guard that fails when it raises something the clause does not include</p>
</dd><dt id="by-run--soundness"><a href="#by-run--soundness"><code>--soundness</code></a> <i>spec</i></dt><dd><p>which runtime type-soundness checks to insert: <code>default</code>, <code>all</code> (adds the opt-in <code>parameters</code> entry checks), <code>none</code>, or a comma-separated subset of <code>generic-calls</code>, <code>projections</code>, <code>iterations</code>, <code>assignments</code>, <code>returns</code>, <code>arguments</code>, <code>parameters</code></p>
<p>[default: default]</p></dd></dl>

## by build

Transpile all .by files and write them to out/

<h3 class="cli-reference">Usage</h3>

```
by build [OPTIONS]
```

<h3 class="cli-reference">Options</h3>

<dl class="cli-reference"><dt id="by-build--help"><a href="#by-build--help"><code>--help</code></a>, <code>-h</code></dt><dd><p>Print help</p>
</dd><dt id="by-build--min-version"><a href="#by-build--min-version"><code>--min-version</code></a> <i>version</i></dt><dd><p>minimum Python version the output must run on</p>
<p>[default: 3.10]</p></dd><dt id="by-build--no-checked-cast"><a href="#by-build--no-checked-cast"><code>--no-checked-cast</code></a></dt><dd><p>lower <code>&lt;value&gt; cast &lt;type&gt;</code> to an unchecked <code>typing.cast</code> instead of a runtime-checked cast that raises on a type mismatch (the <code>cast?</code> safe form is unaffected)</p>
</dd><dt id="by-build--runtime-raises-checks"><a href="#by-build--runtime-raises-checks"><code>--runtime-raises-checks</code></a></dt><dd><p>wrap every function with a <code>raises</code> clause in a runtime guard that fails when it raises something the clause does not include</p>
</dd><dt id="by-build--soundness"><a href="#by-build--soundness"><code>--soundness</code></a> <i>spec</i></dt><dd><p>which runtime type-soundness checks to insert: <code>default</code>, <code>all</code> (adds the opt-in <code>parameters</code> entry checks), <code>none</code>, or a comma-separated subset of <code>generic-calls</code>, <code>projections</code>, <code>iterations</code>, <code>assignments</code>, <code>returns</code>, <code>arguments</code>, <code>parameters</code></p>
<p>[default: default]</p></dd></dl>

## by generate-api-file

Generate an api lockfile (`api.lock`) summarising the public type-level surface of the project.

The file is one record per public symbol in a terse, line-oriented format. It is not meant to be parsed back into types — the goal is that any meaningful change to the public api shows up as a diff.

<h3 class="cli-reference">Usage</h3>

```
by generate-api-file [OPTIONS]
```

<h3 class="cli-reference">Options</h3>

<dl class="cli-reference"><dt id="by-generate-api-file--help"><a href="#by-generate-api-file--help"><code>--help</code></a>, <code>-h</code></dt><dd><p>Print help (see a summary with '-h')</p>
</dd><dt id="by-generate-api-file--output"><a href="#by-generate-api-file--output"><code>--output</code></a>, <code>-o</code> <i>path</i></dt><dd><p>Where to write the lockfile. Defaults to <code>api.lock</code> in the project root</p>
</dd><dt id="by-generate-api-file--project"><a href="#by-generate-api-file--project"><code>--project</code></a> <i>project</i></dt><dd><p>Run the command within the given project directory</p>
</dd><dt id="by-generate-api-file--python"><a href="#by-generate-api-file--python"><code>--python</code></a>, <code>--venv</code> <i>path</i></dt><dd><p>Path to your project's Python environment or interpreter</p>
</dd><dt id="by-generate-api-file--python-version"><a href="#by-generate-api-file--python-version"><code>--python-version</code></a>, <code>--target-version</code> <i>version</i></dt><dd><p>Python version to assume when resolving types</p>
<p>Possible values:</p>
<ul>
<li><code>3.7</code></li>
<li><code>3.8</code></li>
<li><code>3.9</code></li>
<li><code>3.10</code></li>
<li><code>3.11</code></li>
<li><code>3.12</code></li>
<li><code>3.13</code></li>
<li><code>3.14</code></li>
<li><code>3.15</code></li>
</ul></dd><dt id="by-generate-api-file--stdout"><a href="#by-generate-api-file--stdout"><code>--stdout</code></a></dt><dd><p>Write the lockfile to stdout instead of a file</p>
</dd></dl>

## by transpile

Transpile a file to stdout, or a whole directory in place (reads stdin if no path given)

<h3 class="cli-reference">Usage</h3>

```
by transpile [OPTIONS] [FILE]
```

<h3 class="cli-reference">Arguments</h3>

<dl class="cli-reference"><dt id="by-transpile--file"><a href="#by-transpile--file"><code>FILE</code></a></dt><dd><p>file to transpile to stdout, or a directory to transpile in place (every <code>.by</code> → <code>.py</code>, or with <code>--reverse</code> every <code>.py</code> → <code>.by</code>)</p>
</dd></dl>

<h3 class="cli-reference">Options</h3>

<dl class="cli-reference"><dt id="by-transpile--help"><a href="#by-transpile--help"><code>--help</code></a>, <code>-h</code></dt><dd><p>Print help</p>
</dd><dt id="by-transpile--min-version"><a href="#by-transpile--min-version"><code>--min-version</code></a> <i>version</i></dt><dd><p>minimum Python version the output must run on</p>
<p>[default: 3.10]</p></dd><dt id="by-transpile--no-checked-cast"><a href="#by-transpile--no-checked-cast"><code>--no-checked-cast</code></a></dt><dd><p>lower <code>&lt;value&gt; cast &lt;type&gt;</code> to an unchecked <code>typing.cast</code> instead of a runtime-checked cast that raises on a type mismatch (the <code>cast?</code> safe form is unaffected)</p>
</dd><dt id="by-transpile--reverse"><a href="#by-transpile--reverse"><code>--reverse</code></a></dt><dd><p>convert Python source into basedpython idioms (instead of the default by → py direction)</p>
</dd><dt id="by-transpile--runtime-raises-checks"><a href="#by-transpile--runtime-raises-checks"><code>--runtime-raises-checks</code></a></dt><dd><p>wrap every function with a <code>raises</code> clause in a runtime guard that fails when it raises something the clause does not include</p>
</dd><dt id="by-transpile--soundness"><a href="#by-transpile--soundness"><code>--soundness</code></a> <i>spec</i></dt><dd><p>which runtime type-soundness checks to insert: <code>default</code>, <code>all</code> (adds the opt-in <code>parameters</code> entry checks), <code>none</code>, or a comma-separated subset of <code>generic-calls</code>, <code>projections</code>, <code>iterations</code>, <code>assignments</code>, <code>returns</code>, <code>arguments</code>, <code>parameters</code></p>
<p>[default: default]</p></dd></dl>

## by help

Print this message or the help of the given subcommand(s)

<h3 class="cli-reference">Usage</h3>

```
by help [COMMAND]
```

