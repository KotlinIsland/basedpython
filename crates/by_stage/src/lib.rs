//! Writing a project out as python, and putting one file back into a tree that
//! already exists.
//!
//! `by build` and `by run` both need the same thing: the project, rendered as a
//! directory python can import. That is more than the transpiled `.by` files. A
//! project is also its hand-written `.py` modules, its `py.typed` marker, its
//! templates and json and fixture data — and a tree holding only the transpiled
//! half is not a project at all. A module that imports a `.py` sibling fails to
//! import, and anything that opens a data file relative to the working directory
//! fails to open it.
//!
//! # Why this is a crate and not a module of `by`
//!
//! Because a second program needs the same answers, and needs them to be the
//! *same* answers rather than answers that happen to agree today.
//!
//! `by run` stages the project into a temp directory and runs the program out of
//! there, so nothing the user is editing is the file the process is executing: a
//! `.by` because it was transpiled, a hand-written `.py` because it was copied.
//! Reloading a function into that running program therefore means putting new
//! bytes into the tree first — and the bytes have to be the ones the build would
//! have written, down to the byte, or the debugger is being handed a module body
//! its line table does not describe.
//!
//! Rebuilding the tree to get them is not affordable. Measured on a 97-file
//! project, `by check` is 8.5 seconds and `by build` is 24.9; one file's share of
//! the latter is about 165 milliseconds. So one file is re-staged at a time, out
//! of a project database that is already warm — which is the language server's,
//! not the `by` binary's. `ty` depends on `ty_server`, so the shared pipeline
//! cannot live in `ty`; it lives here, and both depend on it.
//!
//! # The shape of it
//!
//! - [`project`] — which files a build is made of, and the db it reads them
//!   through.
//! - [`emit`] — checking those files and turning them into python. The one
//!   transpile path.
//! - [`staging`] — the output tree: what lands where, and what a rebuild takes
//!   back.
//! - [`verbatim`] — everything the transpiler did not produce, carried over
//!   unchanged.
//! - [`sourcemap`] — `_by_sourcemap.py`, written whole by a build and edited one
//!   entry at a time by a re-stage.
//! - [`record`] — `_by_build.json`, what a tree says about the build that wrote
//!   it.
//! - [`restage`] — one file's slot in an existing tree, recomputed.

pub mod emit;
pub mod project;
pub mod record;
pub mod restage;
pub mod sourcemap;
pub mod staging;
pub mod verbatim;
