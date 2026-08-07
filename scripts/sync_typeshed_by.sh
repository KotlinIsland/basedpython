#!/usr/bin/env bash
# regenerate the basedpython typeshed (`.byi`) from upstream `.pyi`
#
# run this after `git merge upstream/main` brings in fresh `.pyi` stubs
# from astral-sh/ruff. produces the committed `.byi` typeshed in-place
# under `crates/ty_vendored/vendor/typeshed/`
#
# pipeline:
#   1. reverse-transpile each `.pyi` → `.byi` in-place, delete `.pyi`
#   2. apply rust ast patches (`by_typeshed_patch`): semantic patches plus the
#      legacy-TypeVar → pep 695 conversion (explicit variance + nice names)
#
# verification happens via `cargo nextest run` after this script — the
# basedpython parser is exercised through ty's mdtest suite and the
# `typeshed_versions_consistent_with_vendored_stubs` integration test
#
# usage:
#   scripts/sync_typeshed_by.sh [--skip-patches]

set -euo pipefail

REPO_ROOT="$(git rev-parse --show-toplevel)"
TYPESHED="$REPO_ROOT/crates/ty_vendored/vendor/typeshed/stdlib"

SKIP_PATCHES=0
for arg in "$@"; do
    case "$arg" in
        --skip-patches) SKIP_PATCHES=1 ;;
        *) echo "unknown arg: $arg" >&2; exit 2 ;;
    esac
done

if [[ ! -d "$TYPESHED" ]]; then
    echo "typeshed stdlib not found at $TYPESHED" >&2
    exit 1
fi

cd "$REPO_ROOT"

echo "==> building by + by_typeshed_patch"
cargo build --bin by --bin by_typeshed_patch --bin by_override_patch

BY="$REPO_ROOT/target/debug/by"
PATCH="$REPO_ROOT/target/debug/by_typeshed_patch"
OVERRIDE_PATCH="$REPO_ROOT/target/debug/by_override_patch"

echo "==> phase 1: reverse-transpile .pyi -> .byi"
pyi_count=0
while IFS= read -r -d '' pyi; do
    byi="${pyi%.pyi}.byi"
    "$BY" transpile --reverse "$pyi" > "$byi"
    rm "$pyi"
    pyi_count=$((pyi_count + 1))
done < <(find "$TYPESHED" -name "*.pyi" -print0)
echo "    converted $pyi_count files"

if [[ "$pyi_count" -eq 0 ]]; then
    echo "    (no .pyi files found — already converted or upstream sync not yet merged)"
fi

if [[ "$SKIP_PATCHES" -eq 0 ]]; then
    echo "==> phase 2: ast patches + pep 695 conversion"
    # run to a fixed point: a few post-patches only see a form an earlier
    # post-patch produced (`private type _X` needs the `type _X = …` statement
    # that `TypeAliasStatements` writes), so the first pass leaves work for the
    # second
    for pass_no in 1 2 3 4; do
        # the binary reports on stderr, so the fixed-point check has to read it
        out="$("$PATCH" "$TYPESHED" 2>&1)"
        echo "    pass $pass_no: $out"
        if [[ "$out" == *"patched 0"* ]]; then
            break
        fi
        if [[ "$pass_no" -eq 4 ]]; then
            echo "    patches did not reach a fixed point in 4 passes" >&2
            exit 1
        fi
    done

    # phase 3 needs the final `.byi` form: it type-checks the whole typeshed with
    # ty and marks every genuine override, so it must run after the ast patches
    # shellcheck disable=SC2016
    echo '==> phase 3: mark overriding methods with `override`'
    "$OVERRIDE_PATCH" "$TYPESHED"

    # `redundant_none_return` refuses an override, which on a fresh sync it can
    # only see once phase 3 has written the markers. every post-patch is
    # idempotent, so replaying them here costs nothing and is a no-op on a tree
    # that already carries them
    echo "==> phase 4: replay the ast patches over the marked overrides"
    echo "    $("$PATCH" "$TYPESHED" 2>&1)"
fi

echo "==> done. review diff with: git diff -- $TYPESHED"
echo "==> next step: cargo nextest run + uvx prek run -a"
