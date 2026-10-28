//! where configuration files are read from: the host's file system, or the view of it another
//! program has, as a language server reads a workspace through its own file system

use std::io;
use std::path::{Path, PathBuf};

use anyhow::Result;

/// the paths a glob matches, or why it can't be matched
pub type GlobResult = Result<Vec<PathBuf>>;

/// the files configuration is found in and read from
pub trait ConfigurationFiles {
    /// whether `path` is a file
    fn is_file(&self, path: &Path) -> bool;

    /// the contents of the file at `path`
    fn read_to_string(&self, path: &Path) -> io::Result<String>;

    /// the paths matching the glob `pattern`
    fn glob(&self, pattern: &str) -> GlobResult;
}

/// the host's file system
#[derive(Debug, Clone, Copy, Default)]
pub struct HostFiles;

impl ConfigurationFiles for HostFiles {
    fn is_file(&self, path: &Path) -> bool {
        path.is_file()
    }

    fn read_to_string(&self, path: &Path) -> io::Result<String> {
        std::fs::read_to_string(path)
    }

    fn glob(&self, pattern: &str) -> GlobResult {
        Ok(glob::glob(pattern)?.collect::<Result<Vec<_>, _>>()?)
    }
}

/// the paths matching the glob `pattern`, as [`glob::glob`] matches them, found by listing
/// directories with `list`, which answers the paths of a directory's entries and whether each is
/// a directory, and checking each match exists with `exists`. for a file system other than the
/// host's, which `glob::glob` can't read
pub fn glob_by_listing(
    pattern: &str,
    list: &dyn Fn(&Path) -> Vec<(PathBuf, bool)>,
    exists: &dyn Fn(&Path) -> bool,
) -> GlobResult {
    let options = glob::MatchOptions::new();
    let mut matches = vec![PathBuf::new()];
    let mut components = Path::new(pattern).components().peekable();
    while let Some(component) = components.next() {
        let std::path::Component::Normal(name) = component else {
            for path in &mut matches {
                path.push(component);
            }
            continue;
        };
        let name_pattern = name
            .to_str()
            .filter(|name| name.contains(['*', '?', '[']))
            .map(glob::Pattern::new)
            .transpose()?;
        matches = match name_pattern {
            None => matches.into_iter().map(|path| path.join(name)).collect(),
            // `**` is every directory below the directory, and the directory itself unless the
            // pattern ends with it
            Some(_) if name == "**" => {
                let last = components.peek().is_none();
                let mut found = Vec::new();
                let mut pending: Vec<(PathBuf, bool)> =
                    matches.into_iter().rev().map(|path| (path, true)).collect();
                while let Some((directory, is_start)) = pending.pop() {
                    let mut entries: Vec<PathBuf> = list(&directory)
                        .into_iter()
                        .filter_map(|(entry, is_directory)| is_directory.then_some(entry))
                        .collect();
                    entries.sort();
                    if !(last && is_start) {
                        found.push(directory);
                    }
                    pending.extend(entries.into_iter().rev().map(|entry| (entry, false)));
                }
                found
            }
            Some(name_pattern) => {
                let mut found = Vec::new();
                for directory in matches {
                    let mut entries: Vec<PathBuf> = list(&directory)
                        .into_iter()
                        .map(|(entry, _)| entry)
                        .filter(|entry| {
                            entry
                                .file_name()
                                .and_then(|name| name.to_str())
                                .is_some_and(|name| name_pattern.matches_with(name, options))
                        })
                        .collect();
                    entries.sort();
                    found.extend(entries);
                }
                found
            }
        };
    }
    matches.retain(|path| exists(path));
    Ok(matches)
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use anyhow::Result;

    use super::{ConfigurationFiles, HostFiles, glob_by_listing};

    /// matching by listing directories finds what `glob::glob` finds
    #[test]
    fn listing_matches_as_glob_does() -> Result<()> {
        let root = tempfile::tempdir()?;
        for directory in ["libs/one/inner", "libs/two", "src", ".hidden"] {
            std::fs::create_dir_all(root.path().join(directory))?;
        }
        std::fs::write(root.path().join("libs/file.py"), "")?;
        let list = |directory: &Path| {
            std::fs::read_dir(directory)
                .into_iter()
                .flatten()
                .flatten()
                .map(|entry| (entry.path(), entry.path().is_dir()))
                .collect::<Vec<(PathBuf, bool)>>()
        };
        let exists = |path: &Path| path.exists();
        for pattern in [
            "src",
            "missing",
            "libs/*",
            "libs/t*",
            "libs/**",
            "*",
            "libs/[to]*",
            "libs/**/inner",
            "**/two",
        ] {
            let pattern = root.path().join(pattern);
            let pattern = pattern.to_string_lossy();
            assert_eq!(
                glob_by_listing(&pattern, &list, &exists)?,
                HostFiles.glob(&pattern)?,
                "{pattern}"
            );
        }
        Ok(())
    }
}
