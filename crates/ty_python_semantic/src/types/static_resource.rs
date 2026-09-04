//! basedpython: `import "data/config.yaml" as config`.
//!
//! a static resource is a json, toml or yaml file that is part of the program
//! rather than something the program opens. the import says so, and this module
//! is what turns the path it names into a value with a type.
//!
//! the document is not read into types directly. it is rendered as python — a
//! mapping as a class, a sequence as a tuple, a scalar as a `Final` literal —
//! and that python is inferred like any other module, so `config.a.b[1]` is
//! answered by the same machinery that answers it for a hand-written module.
//! the transpiler emits the very same rendering, which is why the type a value
//! has here and the object the program gets cannot disagree.

use std::fmt;
use std::fmt::Write as _;

use by_resource::Rendered;
use ruff_db::files::{File, FilePath, system_path_to_file};
use ruff_db::resource::{Resource, resource, resource_format, resource_module};
use ruff_db::source::source_text;
use ruff_db::system::{SystemPath, SystemPathBuf};

use crate::Db;

/// why a static resource import could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResourceError {
    /// the path is absolute, so it names a place on one machine
    NotRelative,
    /// the importing file is not a file on the system — a stub out of the
    /// vendored typeshed, say — so a path relative to it means nothing
    Unanchored,
    /// nothing is at the path
    NotFound(SystemPathBuf),
    /// something is at the path, but not in a format a resource is written in
    UnsupportedFormat(String),
    /// the document could not be read
    Unreadable(String),
}

impl fmt::Display for ResourceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ResourceError::NotRelative => {
                f.write_str("a static resource is named by a path relative to the importing file")
            }
            ResourceError::Unanchored => {
                f.write_str("this file is not on the file system, so it cannot name a path")
            }
            ResourceError::NotFound(path) => write!(f, "no file at `{path}`"),
            ResourceError::UnsupportedFormat(path) => {
                write!(f, "`{path}` is not {formats}", formats = list_of_formats())
            }
            ResourceError::Unreadable(message) => f.write_str(message),
        }
    }
}

/// the resource formats, as a phrase to put in a message.
fn list_of_formats() -> String {
    let mut formats = String::from("a ");
    for (index, extension) in by_resource::Format::EXTENSIONS.iter().enumerate() {
        if index > 0 {
            formats.push_str(if index + 1 == by_resource::Format::EXTENSIONS.len() {
                " or "
            } else {
                ", "
            });
        }
        let _ = write!(formats, "`.{extension}`");
    }
    formats.push_str(" file");
    formats
}

/// the file a static resource import names, resolved against the file that
/// imports it.
///
/// the path is relative to the importing file's own directory, the way an
/// import of a data file reads in every other language that has one. an
/// absolute path is refused because it names a place on the machine that
/// happened to build the program.
pub fn resolve_static_resource(
    db: &dyn Db,
    importing_file: File,
    path: &str,
) -> Result<File, ResourceError> {
    if path.is_empty() || SystemPath::new(path).is_absolute() {
        return Err(ResourceError::NotRelative);
    }

    let FilePath::System(importing_path) = importing_file.path(db) else {
        return Err(ResourceError::Unanchored);
    };
    let Some(directory) = importing_path.parent() else {
        return Err(ResourceError::Unanchored);
    };

    let resolved = SystemPath::absolute(path, directory);
    if resource_format(resolved.extension()).is_none() {
        return Err(ResourceError::UnsupportedFormat(resolved.to_string()));
    }

    system_path_to_file(db, &resolved).map_err(|_| ResourceError::NotFound(resolved))
}

/// the python a resource file stands for, bound to `binding`.
///
/// the file's own rendering — the one the type checker infers — binds the
/// document to a name taken from the file name. a caller that needs it under
/// another name, which is what an `as` clause asks for, gets it rendered again
/// rather than aliased, so the class a reader sees in the emitted python is
/// called what the import called it.
pub fn render_as(db: &dyn Db, file: File, binding: &str) -> Result<Rendered, ResourceError> {
    let path = file.path(db);
    let Some(format) = resource_format(path.extension()) else {
        return Err(ResourceError::UnsupportedFormat(path.to_string()));
    };

    let text = source_text(db, file);
    by_resource::transpile(format, text.as_str(), binding)
        .map_err(|error| ResourceError::Unreadable(error.to_string()))
}

/// the rendering the type checker infers for a resource file, and the file that
/// holds it.
///
/// the rendering is a module in its own right, at a path of its own, so a range
/// in it names text that exists. reading a resource through the document's own
/// file would hand every consumer of a range — a diagnostic, a hover, a
/// definition to jump to — a position in text it is not looking at.
pub(crate) fn rendered(db: &dyn Db, document: File) -> Result<(File, &Rendered), ResourceError> {
    match resource(db, document) {
        Resource::Rendered(rendered) => Ok((resource_module(db, document), rendered)),
        Resource::Unreadable(message) => Err(ResourceError::Unreadable(message.clone())),
        Resource::NotAResource => Err(ResourceError::UnsupportedFormat(
            document.path(db).to_string(),
        )),
    }
}
