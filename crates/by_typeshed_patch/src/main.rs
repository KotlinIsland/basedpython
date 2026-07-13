//! binary entry: walks the basedpython typeshed and rewrites each `.byi`
//! stub. invoked by `scripts/sync_typeshed_by.sh` after reverse-transpile
//!
//! each file is rewritten in three passes:
//!
//! 1. the registered semantic [`Patch`]es (e.g. mapping key covariance), which
//!    operate on the legacy `TypeVar` + `Generic[...]` form
//! 1. the pep 695 conversion ([`by_typeshed_patch::pep695`]), which turns
//!    legacy generic classes into pep 695 headers with explicit variance and
//!    nice names
//! 1. the post-conversion [`Patch`]es (e.g. output widening), which need the
//!    explicit variance keywords the conversion emits
//!
//! the passes run sequentially with a re-parse between each: a pass may rewrite
//! a reference the next pass depends on, so each must see the prior output
//!
//! usage:
//!   `by_typeshed_patch` `<typeshed-stdlib-dir>`

#![allow(clippy::print_stderr)]

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::{env, fs};

use anyhow::{Context, Result, bail};
use ruff_python_ast::PySourceType;
use ruff_python_parser::parse_unchecked_source;
use walkdir::WalkDir;

use by_typeshed_patch::{Patch, all_patches, all_post_patches, apply_edits, pep695};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let args: Vec<String> = env::args().collect();
    let [_, dir] = args.as_slice() else {
        bail!("usage: by_typeshed_patch <typeshed-stdlib-dir>");
    };
    let root = PathBuf::from(dir);
    if !root.is_dir() {
        bail!("not a directory: {}", root.display());
    }

    let patches = all_patches();
    let post_patches = all_post_patches();
    if patches.is_empty() && post_patches.is_empty() {
        eprintln!("no patches registered; nothing to do");
        return Ok(());
    }

    let mut patched = 0_usize;
    let mut visited = 0_usize;
    for entry in WalkDir::new(&root).into_iter().filter_map(Result::ok) {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "byi") {
            continue;
        }
        visited += 1;
        let rel = path.strip_prefix(&root).unwrap_or(path);
        if apply_patches_to_file(path, rel, &patches, &post_patches)
            .with_context(|| format!("applying patches to {}", path.display()))?
        {
            patched += 1;
        }
    }
    eprintln!("visited {visited} file(s); patched {patched}");
    Ok(())
}

fn apply_patches_to_file(
    path: &Path,
    rel: &Path,
    patches: &[Box<dyn Patch>],
    post_patches: &[Box<dyn Patch>],
) -> Result<bool> {
    let original = fs::read_to_string(path).with_context(|| format!("{}", path.display()))?;

    // pass 1: registered semantic patches over the legacy form
    //
    // soft errors retained on `parsed.errors()`; we don't gate on them — the
    // basedpython parser accepts patterns (decorator + modifier chain) that
    // the standalone parser still flags. real parse failures are caught by
    // `cargo nextest run` downstream
    let parsed = parse_unchecked_source(&original, PySourceType::BasedPythonStub);
    let mut edits = Vec::new();
    for patch in patches {
        edits.extend(patch.rewrite(rel, &parsed, &original));
    }
    let patched = if edits.is_empty() {
        original.clone()
    } else {
        apply_edits(&original, edits)
    };

    // pass 2: pep 695 conversion over the patched source (re-parsed so it sees
    // any typevar references the patches rewrote)
    let reparsed = parse_unchecked_source(&patched, PySourceType::BasedPythonStub);
    let conversion = pep695::convert_module(&reparsed, &patched);
    let converted = if conversion.is_empty() {
        patched
    } else {
        apply_edits(&patched, conversion)
    };

    // pass 3: post-conversion patches over the final pep 695 form (re-parsed so
    // they see the explicit variance keywords the conversion emitted)
    let reparsed = parse_unchecked_source(&converted, PySourceType::BasedPythonStub);
    let mut post_edits = Vec::new();
    for patch in post_patches {
        post_edits.extend(patch.rewrite(rel, &reparsed, &converted));
    }
    let final_source = if post_edits.is_empty() {
        converted
    } else {
        apply_edits(&converted, post_edits)
    };

    if final_source == original {
        return Ok(false);
    }
    fs::write(path, &final_source).with_context(|| format!("{}", path.display()))?;
    Ok(true)
}
