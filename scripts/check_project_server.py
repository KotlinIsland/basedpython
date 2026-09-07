# /// script
# requires-python = ">=3.11"
# ///
"""Check that `by check` gives the same answer through a running server as it does alone.

Everything else that covers the project server runs the server and the caller in one
process, where the two share a build, a working directory and a view of the file system by
construction. This runs them the way a user does — a real `by server` over stdin and stdout,
a real `by check` finding it — and compares the two answers byte for byte.

    uv run --no-project scripts/check_project_server.py path/to/by [--modules N]

`--modules` sizes the generated project, which is also what makes the timings mean anything:
the point of the feature is a project big enough that checking it twice is worth avoiding.
"""

from __future__ import annotations

import argparse
import json
import os
import queue
import shutil
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path
from typing import Any


class Server:
    """A `by server` on the other end of a pipe, with its output drained.

    Draining is not optional. This asks for pushed diagnostics over a project of a few
    thousand files, which is more than a pipe holds — a client that only reads when it wants
    something blocks the server inside a write and never gets there.
    """

    def __init__(self, by: Path, root: Path):
        self.process = subprocess.Popen(
            [by, "server"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            cwd=root,
        )
        # `Popen` types the pipes as optional because asking for them is optional; they
        # were asked for just above, so bind them once rather than at every use
        stdin, stdout = self.process.stdin, self.process.stdout
        if stdin is None or stdout is None:
            raise RuntimeError("the server started without the pipes it was given")
        self.stdin, self.stdout = stdin, stdout
        self.messages: queue.Queue[dict[str, Any]] = queue.Queue()
        self.reader = threading.Thread(target=self._read_forever, daemon=True)
        self.reader.start()

    def _read_forever(self):
        while True:
            length = None
            while True:
                line = self.stdout.readline()
                if not line:
                    return
                line = line.strip()
                if not line:
                    break
                if line.lower().startswith(b"content-length:"):
                    length = int(line.split(b":")[1])
            if length is None:
                return
            self.messages.put(json.loads(self.stdout.read(length)))

    def send(self, message: dict[str, Any]):
        body = json.dumps(message).encode()
        self.stdin.write(f"Content-Length: {len(body)}\r\n\r\n".encode() + body)
        self.stdin.flush()

    def await_message(self, matches, timeout: float) -> dict[str, Any] | None:
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            try:
                message = self.messages.get(timeout=deadline - time.monotonic())
            except queue.Empty:
                return None
            # the server asks the client for things during startup, and a request it never
            # gets an answer to leaves it waiting rather than working
            if message.get("method") == "workspace/configuration":
                self.send(
                    {
                        "jsonrpc": "2.0",
                        "id": message["id"],
                        "result": [None] * len(message["params"]["items"]),
                    }
                )
            elif "id" in message and "method" in message:
                self.send({"jsonrpc": "2.0", "id": message["id"], "result": None})
            if matches(message):
                return message
        return None


def write_project(root: Path, modules: int):
    """A project with one error in it, and `modules` files of work around that error.

    The work has to be work. A few thousand files that each declare a dataclass and add up a
    list infer almost instantly, and a measurement over those measures process startup — the
    part this feature cannot save. What makes a real check slow is inference that has
    somewhere to go: generics that get solved, overloads that get picked between, and long
    chains where each step's type depends on the last.
    """
    (root / "pyproject.toml").write_text('[project]\nname = "sample"\nversion = "0"\n')
    (root / "main.py").write_text("def f() -> str:\n    return 42\n")
    for index in range(modules):
        previous = (
            f"from module_{index - 1} import pipeline as previous\n" if index else ""
        )
        seed = "previous(rows)" if index else "rows"
        (root / f"module_{index}.py").write_text(
            f"from collections.abc import Callable, Iterable, Mapping, Sequence\n"
            f"from dataclasses import dataclass, field\n"
            f"from typing import Generic, TypeVar, overload\n"
            f"{previous}\n"
            f"T = TypeVar('T')\n"
            f"U = TypeVar('U')\n"
            f"\n"
            f"@dataclass\n"
            f"class Row{index}(Generic[T]):\n"
            f"    key: str\n"
            f"    value: T\n"
            f"    tags: dict[str, list[tuple[int, str]]] = field(default_factory=dict)\n"
            f"\n"
            f"class Box{index}(Generic[T]):\n"
            f"    def __init__(self, inner: T) -> None:\n"
            f"        self.inner = inner\n"
            f"\n"
            f"    def map(self, f: Callable[[T], U]) -> 'Box{index}[U]':\n"
            f"        return Box{index}(f(self.inner))\n"
            f"\n"
            f"@overload\n"
            f"def widen(value: int) -> float: ...\n"
            f"@overload\n"
            f"def widen(value: str) -> str: ...\n"
            f"def widen(value: int | str) -> float | str:\n"
            f"    return value + 0 if isinstance(value, int) else value\n"
            f"\n"
            f"def pipeline(rows: Sequence[Row{index}[int]]) -> Sequence[Row{index}[int]]:\n"
            f"    seeded = {seed}\n"
            f"    boxed = [Box{index}(row.value).map(widen).map(str).map(len) for row in seeded]\n"
            f"    grouped: Mapping[str, list[int]] = {{\n"
            f"        row.key: [box.inner for box in boxed] for row in seeded\n"
            f"    }}\n"
            f"    ordered: Iterable[tuple[str, list[int]]] = sorted(\n"
            f"        grouped.items(), key=lambda pair: (len(pair[1]), pair[0])\n"
            f"    )\n"
            f"    return [Row{index}(key, sum(values)) for key, values in ordered]\n"
        )


def run_check(
    by: Path, root: Path, *args: str, env: dict[str, str] | None = None
) -> tuple[float, subprocess.CompletedProcess[str]]:
    # a clean switch position rather than whatever the caller exported, so that a developer
    # with the kill switch set in their shell does not get a confusing failure here
    environment = {**os.environ, "BY_NO_PROJECT_SERVER": "0", **(env or {})}
    started = time.monotonic()
    done = subprocess.run(
        [by, "check", *args], cwd=root, capture_output=True, text=True, env=environment
    )
    return time.monotonic() - started, done


USED_THE_SERVER = "using project server information"

# every one of these changes what a check reports or how it reports it, and none of them is
# something a server was asked to do — so each has to send the caller back to checking for
# itself rather than being quietly ignored
FLAGS_THAT_MUST_REFUSE = [
    ("--python-version", "3.9"),
    ("--error-on-warning",),
    ("--output-format", "concise"),
    ("--no-server",),
    ("-vv",),
]


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("by", type=Path, help="the `by` binary to exercise")
    parser.add_argument("--modules", type=int, default=200)
    parser.add_argument(
        "--project",
        type=Path,
        help="an existing project to measure instead of a generated one. a real project is "
        "the only corpus that says anything about the timings: what this feature saves is "
        "inference, and generated files have almost none to save",
    )
    args = parser.parse_args()

    by = args.by.resolve()
    generated = args.project is None
    if generated:
        root = Path(tempfile.mkdtemp(prefix="by-project-server-"))
        write_project(root, args.modules)
    else:
        root = args.project.resolve()

    server = Server(by, root)
    try:
        server.send(
            {
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "processId": os.getpid(),
                    # pull diagnostics, which is how an editor in workspace mode asks a
                    # server to check a project. without it the server checks nothing, and a
                    # measurement against it measures the server doing the whole check inside
                    # the request — which is the thing this is supposed to avoid
                    "capabilities": {
                        "textDocument": {"diagnostic": {}},
                        "workspace": {"diagnostics": {"refreshSupport": False}},
                    },
                    "workspaceFolders": [{"uri": root.as_uri(), "name": "sample"}],
                    # the server has to be checking the whole project before it can answer for
                    # one, and this is the setting that decides that
                    "initializationOptions": {"diagnosticMode": "workspace"},
                },
            }
        )
        _ = server.await_message(lambda message: message.get("id") == 1, timeout=30)
        server.send({"jsonrpc": "2.0", "method": "initialized", "params": {}})

        warming = time.monotonic()
        server.send(
            {
                "jsonrpc": "2.0",
                "id": 3,
                "method": "workspace/diagnostic",
                "params": {"previousResultIds": []},
            }
        )
        if server.await_message(lambda m: m.get("id") == 3, timeout=900) is None:
            print("the server never answered the workspace diagnostic", file=sys.stderr)
            return 1
        print(f"warming the server took {time.monotonic() - warming:.2f}s")

        hot_elapsed, hot = run_check(by, root)
        cold_elapsed, cold = run_check(by, root, env={"BY_NO_PROJECT_SERVER": "1"})
        flagged = {
            flags: run_check(by, root, *flags)[1] for flags in FLAGS_THAT_MUST_REFUSE
        }
    finally:
        server.send({"jsonrpc": "2.0", "id": 2, "method": "shutdown", "params": None})
        _ = server.await_message(lambda message: message.get("id") == 2, timeout=30)
        server.send({"jsonrpc": "2.0", "method": "exit", "params": None})
        server.process.wait(timeout=10)
        if generated:
            shutil.rmtree(root, ignore_errors=True)

    print(f"hot:  {hot_elapsed:6.2f}s  exit {hot.returncode}")
    print(f"cold: {cold_elapsed:6.2f}s  exit {cold.returncode}")

    failures = []
    if USED_THE_SERVER not in hot.stderr:
        failures.append(
            "the check did not use the server. every reason it might have refused is logged, "
            "so run `TY_LOG=ty=debug by check` in the project to see which one it was — not "
            f"`-v`, which is itself a reason to refuse.\nstderr was:\n{hot.stderr}"
        )
    for flags, done in flagged.items():
        if USED_THE_SERVER in done.stderr:
            failures.append(
                f"`by check {' '.join(flags)}` was answered by the server, which cannot have "
                "resolved the same check"
            )
    if hot.stdout != cold.stdout:
        failures.append(
            f"the answers differ.\nhot:\n{hot.stdout}\ncold:\n{cold.stdout}"
        )
    if hot.returncode != cold.returncode:
        failures.append(
            f"the exit statuses differ: {hot.returncode} hot, {cold.returncode} cold"
        )

    for failure in failures:
        print(f"\nFAILED: {failure}", file=sys.stderr)
    return 1 if failures else 0


sys.exit(main())
