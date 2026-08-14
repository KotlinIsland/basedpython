"""Tests for the round-trip report renderer and its error comparison.

Run with::

    uv run --no-project --with pytest pytest scripts/test_check_ecosystem_roundtrip.py

The fixtures below reproduce the shapes seen in a real CI run: a salsa panic
with a full `RUST_BACKTRACE=1` dump, and a build that failed with thousands of
unresolved-import diagnostics.
"""

from __future__ import annotations

import re
from pathlib import Path

from check_ecosystem_roundtrip import (
    _COMMENT_BUDGET,
    BUILD_PATH,
    COMMENT_CHAR_LIMIT,
    FileDiff,
    ProjectDiff,
    ProjectOutcome,
    _detail_cap,
    _truncate_detail,
    canonical_error,
    classify_project,
    render_diff_report,
)


def panic(*, thread: int, first_line: int, frames: int) -> str:
    """A salsa cycle panic, as `by` writes it to stderr under RUST_BACKTRACE=1."""
    body = [
        f"reverse: thread 'main' ({thread}) panicked at "
        f"/root/.cargo/registry/src/salsa-0.28.1/src/function/execute.rs:731:9:",
        "infer_statement_types_impl(Id(398)): execute: too many cycle iterations",
        "Query stack:",
        "[",
        "    infer_scope_types_impl(Id(206)),",
        "]",
        "stack backtrace:",
    ]
    for i in range(frames):
        body.append(f"  {i}: some_frame_{i}")
        body.append(
            f"             at ./crates/ty_python_semantic/src/x.rs:{first_line + i}:9"
        )
    body.append(
        "note: Some details are omitted, run with `RUST_BACKTRACE=full` "
        "for a verbose backtrace."
    )
    return "\n".join(body)


def diagnostics(*, binary: str, sha: str, count: int) -> str:
    """A `by build` that bailed out with unresolved third-party imports."""
    body = ["build: error[unresolved-import]: Cannot resolve imported module `x`"]
    for i in range(count):
        body.append(f"error[unresolved-import]: Cannot resolve module `dep{i}`")
        body.append("info: Searched in the following paths during module resolution:")
        body.append("info:   1. /tmp/proj (first-party code)")
    body.append(f"info: Version: unknown ({sha} 2026-08-09)")
    body.append(f'info: Args: ["/work/{binary}", "build", "--min-version", "3.15"]')
    return "\n".join(body)


class TestCanonicalError:
    def test_thread_id_is_not_a_difference(self):
        assert canonical_error(panic(thread=4094, first_line=10, frames=3)) == (
            canonical_error(panic(thread=9999, first_line=10, frames=3))
        )

    def test_backtrace_line_numbers_are_not_a_difference(self):
        # an unrelated edit above the frame shifts every `at ...:LINE:COL`
        assert canonical_error(panic(thread=1, first_line=1289, frames=3)) == (
            canonical_error(panic(thread=1, first_line=1304, frames=3))
        )

    def test_backtrace_depth_is_not_a_difference(self):
        # the same cycle panic, unwound a few frames deeper
        assert canonical_error(panic(thread=1, first_line=10, frames=3)) == (
            canonical_error(panic(thread=1, first_line=10, frames=40))
        )

    def test_version_and_argv_footer_is_not_a_difference(self):
        # the two binaries are built from different commits, by construction
        assert canonical_error(
            diagnostics(binary="by-base", sha="de1d05d", count=2)
        ) == (canonical_error(diagnostics(binary="by-pr", sha="4dbc1ae", count=2)))

    def test_salsa_ids_are_not_a_difference(self):
        a = "panicked: asserts_guard_targets(Id(5c80)) at Id(2e5)"
        b = "panicked: asserts_guard_targets(Id(9f11)) at Id(0aa)"
        assert canonical_error(a) == canonical_error(b)

    def test_a_different_panic_message_is_a_difference(self):
        a = panic(thread=1, first_line=10, frames=3)
        b = a.replace("too many cycle iterations", "dependency graph cycle")
        assert canonical_error(a) != canonical_error(b)

    def test_a_different_diagnostic_set_is_a_difference(self):
        assert canonical_error(diagnostics(binary="by-base", sha="a", count=2)) != (
            canonical_error(diagnostics(binary="by-pr", sha="b", count=3))
        )

    def test_query_stack_names_survive(self):
        a = panic(thread=1, first_line=10, frames=3)
        b = a.replace("infer_scope_types_impl", "infer_definition_types")
        assert canonical_error(a) != canonical_error(b)


class TestClassifyProject:
    def test_same_failure_under_both_binaries_is_reported_but_not_changed(self):
        old = ProjectOutcome(
            error=panic(thread=4094, first_line=10, frames=3), outputs={}
        )
        new = ProjectOutcome(
            error=panic(thread=4104, first_line=25, frames=9), outputs={}
        )
        (diff,) = classify_project("mypy", old, new).diffs  # ty: ignore[refutable-unpacking]
        assert diff.kind == "error-unchanged"

    def test_a_genuinely_changed_failure_is_reported(self):
        old = ProjectOutcome(error=panic(thread=1, first_line=10, frames=3), outputs={})
        new = ProjectOutcome(error="build: something else entirely", outputs={})
        (diff,) = classify_project("mypy", old, new).diffs  # ty: ignore[refutable-unpacking]
        assert diff.kind == "error-changed"
        assert diff.path == BUILD_PATH

    def test_a_new_failure_is_still_a_regression(self):
        old = ProjectOutcome(error=None, outputs={"a.py": b"x"})
        new = ProjectOutcome(error=panic(thread=1, first_line=10, frames=3), outputs={})
        (diff,) = classify_project("mypy", old, new).diffs  # ty: ignore[refutable-unpacking]
        assert diff.kind == "broken"

    def test_a_resolved_failure_is_still_an_improvement(self):
        old = ProjectOutcome(error=panic(thread=1, first_line=10, frames=3), outputs={})
        new = ProjectOutcome(error=None, outputs={"a.py": b"x"})
        (diff,) = classify_project("mypy", old, new).diffs  # ty: ignore[refutable-unpacking]
        assert diff.kind == "fixed"


class TestTruncateDetail:
    def test_short_detail_is_untouched(self):
        assert _truncate_detail("a\nb\nc", 2_000) == "a\nb\nc"

    def test_long_detail_keeps_both_ends(self):
        detail = panic(thread=1, first_line=10, frames=4000)
        out = _truncate_detail(detail, 2_000)
        assert len(out) <= 2_000
        assert "too many cycle iterations" in out
        assert "characters elided" in out
        assert out.endswith("for a verbose backtrace.")

    def test_a_detail_of_few_enormous_lines_is_still_capped(self):
        assert len(_truncate_detail("x" * 500_000, 2_000)) <= 2_000

    def test_cuts_land_on_line_boundaries(self):
        # neither end may be left holding half a line
        out = _truncate_detail("\n".join(f"line{i}" for i in range(10_000)), 2_000)
        body = [x for x in out.splitlines() if x and "characters elided" not in x]
        assert all(re.fullmatch(r"line\d+", x) for x in body)

    def test_more_findings_means_a_smaller_share_each(self):
        caps = [_detail_cap(n) for n in (1, 5, 25, 200)]
        assert caps == sorted(caps, reverse=True)
        assert caps[0] > caps[-1]

    def test_the_cap_never_collapses_to_nothing(self):
        assert _detail_cap(100_000) >= 1_000
        assert _detail_cap(0) > 0


def project(name: str, kind: str, detail: str) -> ProjectDiff:
    return ProjectDiff(name, 10, [FileDiff(BUILD_PATH, kind, detail)], None)


class TestRenderDiffReport:
    def test_the_real_world_blowup_fits(self):
        # 23 error-changed findings, one of them the 4MB strawberry dump: the
        # exact input that 422'd the comment job
        results = [
            project(
                f"proj{i}", "error-changed", panic(thread=i, first_line=1, frames=900)
            )
            for i in range(22)
        ]
        results.append(
            project(
                "strawberry",
                "error-changed",
                diagnostics(binary="by-pr", sha="a", count=40_000),
            )
        )
        body, clean = render_diff_report(results, "base", "head")
        assert len(body) < COMMENT_CHAR_LIMIT
        assert clean  # error changes don't fail the check
        assert "<!-- by-ecosystem-roundtrip -->" in body
        assert "error changes: 23" in body
        # a report this size fits whole — nothing should need dropping
        assert "finding(s) omitted" not in body
        assert "strawberry" in body

    def test_regressions_are_never_dropped_for_lesser_findings(self):
        results = [
            project(
                f"noise{i}", "error-changed", panic(thread=i, first_line=1, frames=900)
            )
            for i in range(200)
        ]
        results.append(project("critical", "broken", "build: boom"))
        body, clean = render_diff_report(results, "base", "head")
        assert len(body) <= _COMMENT_BUDGET
        assert not clean
        assert "critical" in body
        assert "finding(s) omitted" in body

    def test_counts_reflect_everything_even_when_entries_are_dropped(self):
        results = [
            project(
                f"noise{i}", "error-changed", panic(thread=i, first_line=1, frames=900)
            )
            for i in range(200)
        ]
        body, _ = render_diff_report(results, "base", "head")
        assert "error changes: 200" in body
        assert "finding(s) omitted" in body

    def test_a_small_report_is_not_truncated(self):
        body, _ = render_diff_report(
            [project("a", "error-changed", "base: x\nhead: y")], "base", "head"
        )
        assert "finding(s) omitted" not in body
        assert "base: x" in body

    def test_a_clean_report_lists_skipped_projects(self):
        results = [ProjectDiff("spack", 0, [], "skipped: known to OOM the runner")]
        body, clean = render_diff_report(results, "base", "head")
        assert clean
        assert "no round-trip differences" in body
        assert "spack" in body
        assert "known to OOM" in body

    def test_a_multiline_skip_reason_stays_one_list_item(self):
        reason = "setup failed:\nCloning into '/tmp/x'...\nresolved 40 packages\n"
        results = [ProjectDiff("AutoSplit", 0, [], reason)]
        body, _ = render_diff_report(results, "base", "head")
        (item,) = [x for x in body.splitlines() if x.startswith("- `AutoSplit`")]  # ty: ignore[refutable-unpacking]
        assert "Cloning into" in item  # the whole reason is on the one line

    def test_a_project_failing_on_both_is_named_but_does_not_fail_the_job(self):
        results = [
            project(
                "mypy", "error-unchanged", panic(thread=1, first_line=1, frames=900)
            )
        ]
        body, clean = render_diff_report(results, "base", "head")
        assert clean  # not a regression: it failed on base too
        assert "1 project(s) fail to round-trip" in body
        assert "- `mypy`: reverse: panicked at execute.rs:731:9: " in body
        # one line, not a 100KB backtrace dump
        assert "stack backtrace" not in body

    def test_a_clean_run_with_failures_does_not_read_as_all_good(self):
        results = [
            project(
                f"p{i}", "error-unchanged", panic(thread=i, first_line=1, frames=900)
            )
            for i in range(22)
        ]
        body, clean = render_diff_report(results, "base", "head")
        assert clean
        assert "22 project(s) fail to round-trip" in body
        assert (
            "no round-trip differences" in body
        )  # true, and no longer the whole story

    def test_the_failure_count_survives_even_if_the_entries_are_dropped(self):
        results = [
            project(
                f"noise{i}", "error-changed", panic(thread=i, first_line=1, frames=900)
            )
            for i in range(400)
        ]
        results += [
            project(
                f"dead{i}", "error-unchanged", panic(thread=i, first_line=1, frames=900)
            )
            for i in range(30)
        ]
        body, _ = render_diff_report(results, "base", "head")
        assert len(body) <= _COMMENT_BUDGET
        assert "30 project(s) fail to round-trip" in body

    def test_an_unchanged_failure_is_not_counted_as_an_error_change(self):
        results = [project("a", "error-unchanged", "reverse: boom")]
        body, _ = render_diff_report(results, "base", "head")
        assert "error changes:" not in body

    def test_a_bare_failure_line_carries_its_cause(self):
        # the real shape from parso and sphinx: `by failed` says nothing on its
        # own, and the reason is on the lines beneath it
        err = (
            "reverse: by failed\n"
            "  Cause: ./test/normalizer_issue_files/latin-1.py\n"
            "  Cause: stream did not contain valid UTF-8"
        )
        results = [project("parso", "error-unchanged", err)]
        body, _ = render_diff_report(results, "base", "head")
        (item,) = [x for x in body.splitlines() if x.startswith("- `parso`")]  # ty: ignore[refutable-unpacking]
        assert "stream did not contain valid UTF-8" in item
        assert "latin-1.py" in item

    def test_an_error_is_surfaced_past_a_leading_warning(self):
        err = (
            "build: warning[redundant-return-annotation]: Redundant -> None\n"
            " --> m.by:1:36\n"
            "  |\n"
            "error[unresolved-reference]: Name some used when not defined"
        )
        results = [project("beartype", "error-unchanged", err)]
        body, _ = render_diff_report(results, "base", "head")
        (item,) = [x for x in body.splitlines() if x.startswith("- `beartype`")]  # ty: ignore[refutable-unpacking]
        assert "unresolved-reference" in item

    def test_a_panic_still_wins_over_the_line_scan(self):
        results = [
            project(
                "mypy", "error-unchanged", panic(thread=1, first_line=1, frames=900)
            )
        ]
        body, _ = render_diff_report(results, "base", "head")
        (item,) = [x for x in body.splitlines() if x.startswith("- `mypy`")]  # ty: ignore[refutable-unpacking]
        assert "panicked at execute.rs:731:9" in item
        assert "stack backtrace" not in item

    def test_summarises_a_non_panic_failure_too(self):
        results = [
            project(
                "strawberry",
                "error-unchanged",
                diagnostics(binary="by-pr", sha="a", count=900),
            )
        ]
        body, _ = render_diff_report(results, "base", "head")
        assert "- `strawberry`: build: error[unresolved-import]" in body

    def test_entries_are_ordered_by_project_not_by_shard(self):
        results = [project(n, "error-unchanged", "reverse: boom") for n in "dbca"]
        body, _ = render_diff_report(results, "base", "head")
        names = [x.split("`")[1] for x in body.splitlines() if x.startswith("- `")]
        assert names == ["a", "b", "c", "d"]

    def test_changed_output_renders_as_a_diff_fence(self):
        results = [
            ProjectDiff("a", 1, [FileDiff(Path("m.py"), "changed", "-old\n+new")], None)
        ]
        body, _ = render_diff_report(results, "base", "head")
        assert "```diff" in body
        assert "+new" in body
