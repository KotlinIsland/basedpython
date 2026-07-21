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
    "$PATCH" "$TYPESHED"

    # phase 3 needs the final `.byi` form: it type-checks the whole typeshed with
    # ty and marks every genuine override, so it must run after the ast patches
    echo '==> phase 3: mark overriding methods with `override`'
    "$OVERRIDE_PATCH" "$TYPESHED"
fi

echo "==> done. review diff with: git diff -- $TYPESHED"
echo "==> next step: cargo nextest run + uvx prek run -a"
