//! basedpython static resources: a data file read as the python it stands for.
//!
//! `import "data/config.yaml" as config` imports a json, toml or yaml file as a
//! value. everything that answers a question about that value — what type
//! `config.a.b[1]` has, what the transpiled program binds `config` to — reads it
//! through the python [`by_resource`] renders the document into, so the file is
//! parsed and inferred exactly like a hand-written module.
//!
//! # why the python gets a file of its own
//!
//! The rendering could have been served as the document's own contents, and for
//! a while it was. But a position in the rendering is not a position in the
//! document: the python is longer, and its lines are somewhere else entirely.
//! Everything that takes a range and a file and expects the range to name text
//! in that file — a diagnostic, a hover, a definition an editor is asked to jump
//! to — was then holding two halves of different things, and a range past the
//! end of the document is not a wrong answer but a panic.
//!
//! So the rendering is [a file of its own](resource_module), at a path no
//! document has, whose contents are the python and whose ranges therefore mean
//! what they say. [`source_text`] keeps returning what each file actually holds,
//! which matters because plenty of the toml and json in a project is not a
//! resource at all — `pyproject.toml` is read through it.
//!
//! [`source_text`]: crate::source::source_text

use by_resource::{Format, Rendered};

use crate::Db;
use crate::files::File;
use crate::system::{SystemVirtualPath, SystemVirtualPathBuf};

/// the scheme the rendering of a document is filed under.
///
/// a virtual path, because there is no file on disk holding this python and
/// nothing should go looking for one. the document's own path follows, so the
/// two are one-to-one and either can be found from the other.
const SCHEME: &str = "by-resource:";

/// what a file offers when it is read as a static resource.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resource {
    /// the file's extension is not one a static resource is written in, so the
    /// file is whatever it already was
    NotAResource,
    /// the document, as python
    Rendered(Rendered),
    /// the file is written in a resource format but could not be read as one
    Unreadable(String),
}

/// the format an extension names, if it names one.
pub fn resource_format(extension: Option<&str>) -> Option<Format> {
    Format::from_extension(extension?)
}

/// the file holding `document`'s rendering.
///
/// the same file every time it is asked for, so the semantic index built over
/// it is reused rather than rebuilt.
pub fn resource_module(db: &dyn Db, document: File) -> File {
    let path = SystemVirtualPathBuf::from(format!("{SCHEME}{}", document.path(db)));
    if let Some(existing) = db.files().try_virtual_file(&path) {
        return existing.file();
    }
    db.files().virtual_file(db, &path).file()
}

/// the document a rendering was made from, if `file` is a rendering.
pub fn resource_document(db: &dyn Db, file: File) -> Option<File> {
    let crate::files::FilePath::SystemVirtual(path) = file.path(db) else {
        return None;
    };
    let document = path.as_str().strip_prefix(SCHEME)?;
    crate::files::system_path_to_file(db, document).ok()
}

/// whether `path` names a rendering rather than a file anyone wrote.
pub fn is_resource_module(path: &SystemVirtualPath) -> bool {
    path.as_str().starts_with(SCHEME)
}

/// read `file` as a static resource.
///
/// the result is the same one the type checker infers and the transpiler emits,
/// so there is no way for the two to be looking at different renderings of the
/// same document.
#[salsa::tracked(returns(ref), heap_size=ruff_memory_usage::heap_size)]
pub fn resource(db: &dyn Db, file: File) -> Resource {
    let path = file.path(db);
    let Some(format) = resource_format(path.extension()) else {
        return Resource::NotAResource;
    };

    let text = crate::source::source_text(db, file);
    if let Some(error) = text.read_error() {
        return Resource::Unreadable(error.to_string());
    }

    // the name the document is bound to comes from the file name, so the class
    // the checker names in a message is the one a reader would guess
    let stem = path
        .as_str()
        .rsplit(['/', '\\'])
        .next()
        .and_then(|name| name.rsplit_once('.'))
        .map_or("resource", |(stem, _)| stem);

    match by_resource::transpile(format, text.as_str(), &by_resource::binding_name(stem)) {
        Ok(rendered) => Resource::Rendered(rendered),
        Err(error) => Resource::Unreadable(error.to_string()),
    }
}

impl get_size2::GetSize for Resource {
    fn get_heap_size(&self) -> usize {
        match self {
            Resource::NotAResource => 0,
            Resource::Rendered(rendered) => {
                rendered.source.len()
                    + rendered.root.len()
                    + rendered
                        .unusable_keys
                        .iter()
                        .map(|key| key.len() + size_of::<String>())
                        .sum::<usize>()
            }
            Resource::Unreadable(message) => message.len(),
        }
    }
}
