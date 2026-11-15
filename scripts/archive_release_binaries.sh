#!/usr/bin/env bash
# packs the `by` and `buff` binaries that maturin built for a target's wheel into the
# `basedpython-<target>` archive that cargo-dist plans for the github release, beside its
# `.sha256` sidecar. archiving the wheel's own build means the archive and the wheel ship the
# same binaries, built from `crates/basedpython` against its `Cargo.lock`
#
# the layout is the one cargo-dist's installers unpack: a `.tar.gz` holds one top-level
# `basedpython-<target>/` directory, a `.zip` holds the binaries at its root
#
# usage: scripts/archive_release_binaries.sh <target>
set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "usage: $0 <target>" >&2
  exit 2
fi

target=$1
name="basedpython-$target"
# `crates/basedpython` is its own cargo workspace, so its build lands beside that manifest
# rather than in the repository's `target`
release_dir="crates/basedpython/target/$target/release"

case "$target" in
  *-windows-*)
    archive="$name.zip"
    7z a "$archive" "./$release_dir/by.exe" "./$release_dir/buff.exe"
    sha256sum "$archive" > "$archive.sha256"
    ;;
  *)
    archive="$name.tar.gz"
    mkdir "$name"
    cp "$release_dir/by" "$release_dir/buff" "$name/"
    # macos's tar would otherwise add an `._` file for each file with extended attributes
    COPYFILE_DISABLE=1 tar czvf "$archive" "$name"
    shasum -a 256 "$archive" > "$archive.sha256"
    ;;
esac
