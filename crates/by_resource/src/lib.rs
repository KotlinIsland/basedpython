//! importing a json, toml or yaml file as a value with typed dot access.
//!
//! `import "data/config.yaml" as config` says the file is a *static* resource:
//! part of the program, fixed at build time, read through named attributes
//! rather than opened at runtime. this crate is what that claim rests on. it
//! reads the document (`parse`) and renders it as python (`render`) — a
//! mapping as a class, a sequence as a tuple, a scalar as a `Final` literal.
//!
//! both halves of the language read a resource through this one rendering: the
//! type checker infers the rendered module to answer what `config.a.b[1]` is,
//! and the transpiler writes the same rendering into the python it emits. there
//! is no second description of what a document means, so the type and the object
//! cannot disagree.

mod parse;
mod render;
mod value;

pub(crate) use parse::parse;
pub use parse::{Format, ParseError};
pub(crate) use render::render;
pub use render::{REQUIRED_IMPORT, Rendered, binding_name};

/// read `text` as `format` and render it as python bound to `root`.
pub fn transpile(format: Format, text: &str, root: &str) -> Result<Rendered, ParseError> {
    Ok(render(&parse(format, text)?, root))
}
