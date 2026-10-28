//! a project's git merge integration: `by merge-driver` set up as the merge driver git runs for
//! its python and basedpython files and its `api.lock`
//!
//! git takes a merge driver from two places: `.gitattributes` gives a path the driver's name,
//! and the repository's configuration says what command the name runs. so the integration is
//! the attribute lines in the project's `.gitattributes`, which are committed and shared, and
//! `merge.by.driver` in the repository's own configuration, which is not, and which every clone
//! sets up again. in a linked work tree, the repository's configuration is the one every work
//! tree of it shares
//!
//! the command names the `by` that set it up by its absolute path, as it was run. git runs a
//! merge driver with the environment of whatever runs git, and an editor's git rarely has a
//! project's environment on `PATH`, so a bare `by` would not be found there. the path is the one
//! the `by` was run by, not where its links lead: a tool installer's link, a virtual
//! environment's `bin` or a package manager's link stays where it is when `by` is upgraded.
//! a driver whose `by` is gone is reported as not found, and setting the integration up again
//! names the `by` that does it
//!
//! the driver needs git 2.44 or later: it is the first git to hand a merge driver the labels of
//! the versions it merges (`%S %X %Y`), and the first whose `git merge-file` takes the diff
//! algorithm `git merge` uses. an older git is not set up, since there the driver could not
//! merge a file

use std::fmt::Write as _;
use std::process::Command;

use anyhow::{Context, anyhow, bail};
use ruff_db::system::{System, SystemPath, SystemPathBuf};
use ruff_source_file::{LineEnding, find_newline};

/// the name `.gitattributes` gives the driver, and its section of the git configuration
const DRIVER: &str = "by";

/// what git hands the driver: see `by merge-driver`
const DRIVER_ARGUMENTS: &str = "merge-driver %O %A %B %L %P %S %X %Y";

/// the oldest git the driver merges with: see the module's documentation
const MINIMUM_GIT: (u32, u32) = (2, 44);

/// the paths the driver merges, each with a path in the project that the pattern matches, which
/// git is asked the attribute of
const MERGED: [(&str, &str); 5] = [
    ("*.by", "a.by"),
    ("*.byi", "a.byi"),
    ("*.py", "a.py"),
    ("*.pyi", "a.pyi"),
    ("api.lock", "api.lock"),
];

/// a project's git merge integration, as git reads it
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitMergeIntegration {
    /// whether the project is in a git work tree, without which there is nothing to set up
    pub repository: bool,
    /// the version of the git on `PATH`, as `git version` writes it
    pub git_version: String,
    /// the project's `.gitattributes`, where the attribute lines are written
    pub attributes_path: SystemPathBuf,
    /// the patterns of the paths git does not merge with the driver, which the project's
    /// `.gitattributes` lacks lines for
    pub missing_attributes: Vec<&'static str>,
    /// the command `merge.by.driver` runs, where it is set
    pub driver: Option<String>,
    /// whether the `by` the command runs is there
    pub driver_found: bool,
}

impl GitMergeIntegration {
    /// whether git merges every path the driver is for with a driver that is there
    pub fn is_enabled(&self) -> bool {
        self.repository && self.missing_attributes.is_empty() && self.driver_found
    }

    /// whether the git on `PATH` is one the driver can merge with, 2.44 or later
    pub fn git_supported(&self) -> bool {
        parse_git_version(&self.git_version).is_some_and(|version| version >= MINIMUM_GIT)
    }

    /// the integration of the project at `project_root`, as git reads it
    pub fn of(system: &dyn System, project_root: &SystemPath) -> anyhow::Result<Self> {
        let attributes_path = project_root.join(".gitattributes");
        let git_version =
            git(project_root, &["version"])?.ok_or_else(|| anyhow!("`git version` failed"))?;
        let repository = git(project_root, &["rev-parse", "--is-inside-work-tree"])?
            .is_some_and(|inside| inside == "true");
        if !repository {
            return Ok(Self {
                repository,
                git_version,
                attributes_path,
                missing_attributes: MERGED.iter().map(|(pattern, _)| *pattern).collect(),
                driver: None,
                driver_found: false,
            });
        }
        // what git itself makes of every attributes file that applies, rather than of the
        // project's alone, one `<path>: merge: <value>` line for each path asked about
        let mut arguments = vec!["check-attr", "-z", "merge", "--"];
        arguments.extend(MERGED.iter().map(|(_, path)| *path));
        let attributes =
            git(project_root, &arguments)?.ok_or_else(|| anyhow!("`git check-attr` failed"))?;
        let merged_by_driver: Vec<&str> = attributes
            .split('\0')
            .collect::<Vec<_>>()
            .as_chunks::<3>()
            .0
            .iter()
            .filter(|[_, _, value]| *value == DRIVER)
            .map(|[path, _, _]| *path)
            .collect();
        let missing_attributes = MERGED
            .iter()
            .filter(|(_, path)| !merged_by_driver.contains(path))
            .map(|(pattern, _)| *pattern)
            .collect();
        let driver = git(
            project_root,
            &["config", "--get", &format!("merge.{DRIVER}.driver")],
        )?;
        let driver_found = driver
            .as_deref()
            .and_then(executable_of)
            .is_some_and(|executable| executable_exists(system, &executable));
        Ok(Self {
            repository,
            git_version,
            attributes_path,
            missing_attributes,
            driver,
            driver_found,
        })
    }

    /// sets the integration of the project at `project_root` up, with a driver that runs `by`,
    /// and answers it as it then is. the lines of `.gitattributes` are kept, and only those it
    /// lacks are added after them, in its line ending. a driver whose `by` is there and whose
    /// command is the current one is kept; any other is replaced with one that runs `by`
    pub fn enable(
        system: &dyn System,
        project_root: &SystemPath,
        by: &SystemPath,
    ) -> anyhow::Result<Self> {
        let integration = Self::of(system, project_root)?;
        if !integration.repository {
            bail!("`{project_root}` is not in a git work tree: there is no git to set up");
        }
        if !integration.git_supported() {
            bail!(
                "`{}` is older than git {}.{}, the first that gives a merge driver what \
                 `by merge-driver` needs: upgrade git to set up its merges",
                integration.git_version,
                MINIMUM_GIT.0,
                MINIMUM_GIT.1
            );
        }
        if !integration.missing_attributes.is_empty() {
            let writable = system
                .as_writable()
                .ok_or_else(|| anyhow!("`{}` cannot be written", integration.attributes_path))?;
            // a file that is there is written back with its own lines kept, so one that cannot
            // be read is not written at all
            let mut attributes = match system.read_to_string(&integration.attributes_path) {
                Ok(attributes) => attributes,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!(
                            "could not read `{}`, which the lines git needs are added to",
                            integration.attributes_path
                        )
                    });
                }
            };
            let line_ending = find_newline(&attributes)
                .map_or(LineEnding::Lf, |(_, line_ending)| line_ending)
                .as_str();
            if !attributes.is_empty() && !attributes.ends_with(['\n', '\r']) {
                attributes.push_str(line_ending);
            }
            let _ = write!(
                attributes,
                "# merged by `by merge-driver`: see `by init --git-merge`{line_ending}"
            );
            for pattern in &integration.missing_attributes {
                let _ = write!(attributes, "{pattern} merge={DRIVER}{line_ending}");
            }
            writable
                .write_file(&integration.attributes_path, &attributes)
                .with_context(|| format!("could not write `{}`", integration.attributes_path))?;
        }
        if !integration.driver_found {
            let name = format!("merge.{DRIVER}.name");
            let driver = format!("merge.{DRIVER}.driver");
            let command = format!("{} {DRIVER_ARGUMENTS}", shell_quote(by.as_str()));
            for (key, value) in [
                (name.as_str(), "by: python imports, and api.lock"),
                (driver.as_str(), command.as_str()),
            ] {
                git(project_root, &["config", "--local", key, value])?
                    .ok_or_else(|| anyhow!("`git config --local {key}` failed"))?;
            }
        }
        Self::of(system, project_root)
    }
}

/// `git` run in `directory`, its output without the line ending, or `None` where it fails
fn git(directory: &SystemPath, arguments: &[&str]) -> anyhow::Result<Option<String>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(directory.as_std_path())
        .args(arguments)
        .output()
        .context("could not run `git`")?;
    if !output.status.success() {
        return Ok(None);
    }
    Ok(Some(
        String::from_utf8_lossy(&output.stdout)
            .trim_end()
            .to_string(),
    ))
}

/// the major and minor version of a git, from what `git version` writes: `git version 2.44.0`,
/// `git version 2.39.3 (Apple Git-146)` or `git version 2.44.0.windows.1`
fn parse_git_version(version: &str) -> Option<(u32, u32)> {
    let number = version
        .strip_prefix("git version ")?
        .split_whitespace()
        .next()?;
    let mut parts = number.split('.');
    Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
}

/// `text` as one word of a POSIX shell command, which git runs a merge driver's command with
fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// the executable a merge driver's command runs: its first word, unquoted
fn executable_of(command: &str) -> Option<String> {
    let command = command.trim_start();
    if let Some(quoted) = command.strip_prefix('\'') {
        // a single-quoted word, `'\''` standing for a quote within it
        let mut word = String::new();
        let mut rest = quoted;
        loop {
            let end = rest.find('\'')?;
            word.push_str(&rest[..end]);
            rest = &rest[end + 1..];
            match rest.strip_prefix(r"\''") {
                Some(after) => {
                    word.push('\'');
                    rest = after;
                }
                None => return Some(word),
            }
        }
    }
    command.split_whitespace().next().map(str::to_string)
}

/// whether `executable` is a file, or for a bare name, a file in a directory on `PATH`
fn executable_exists(system: &dyn System, executable: &str) -> bool {
    find_executable(
        system,
        SystemPath::new(executable),
        system.current_directory(),
    )
    .is_some()
}

/// the file `executable` runs, as a shell finds it: a path with a directory in it, made absolute
/// against `cwd`, or for a bare name, the first file of that name in a directory on `PATH`.
/// where `PATHEXT` is set, as it is on windows, the name with each of its extensions is a
/// candidate too, after the name itself
fn find_executable(
    system: &dyn System,
    executable: &SystemPath,
    cwd: &SystemPath,
) -> Option<SystemPathBuf> {
    let extensions = system.env_var("PATHEXT").unwrap_or_default();
    let candidates = |path: &SystemPath| -> Vec<SystemPathBuf> {
        std::iter::once(path.to_path_buf())
            .chain(
                extensions
                    .split(';')
                    .filter(|extension| !extension.is_empty())
                    .map(|extension| SystemPathBuf::from(format!("{path}{extension}"))),
            )
            .collect()
    };
    if executable.as_str().contains('/') || executable.as_str().contains('\\') {
        return candidates(&SystemPath::absolute(executable, cwd))
            .into_iter()
            .find(|candidate| system.is_file(candidate));
    }
    let path = system.env_var("PATH").ok()?;
    std::env::split_paths(&path)
        .filter_map(|directory| SystemPathBuf::from_path_buf(directory).ok())
        .flat_map(|directory| candidates(&directory.join(executable)))
        .find(|candidate| system.is_file(candidate))
        .map(|candidate| SystemPath::absolute(&candidate, cwd))
}

/// the `by` this process is, by the path it was run as: `argv[0]` found as a shell finds it,
/// against `cwd`, the directory the process was started in. its links are not followed, so that
/// a link an installer keeps in place through upgrades is what is named
pub fn invoked_executable(system: &dyn System, cwd: &SystemPath) -> anyhow::Result<SystemPathBuf> {
    let argv0 = std::env::args_os()
        .next()
        .ok_or_else(|| anyhow!("the process has no `argv[0]` to tell which `by` it is"))?;
    let argv0 = SystemPathBuf::from_path_buf(argv0.into()).map_err(|path| {
        anyhow!(
            "`{}`, which `by` was run as, is not unicode",
            path.display()
        )
    })?;
    find_executable(system, &argv0, cwd)
        .ok_or_else(|| anyhow!("could not find `{argv0}`, which `by` was run as, for git to run"))
}

#[cfg(test)]
mod tests {
    use super::{executable_of, parse_git_version, shell_quote};

    #[test]
    fn git_versions_are_read_as_every_git_writes_them() {
        assert_eq!(parse_git_version("git version 2.44.0"), Some((2, 44)));
        assert_eq!(
            parse_git_version("git version 2.39.3 (Apple Git-146)"),
            Some((2, 39))
        );
        assert_eq!(
            parse_git_version("git version 2.44.0.windows.1"),
            Some((2, 44))
        );
        assert_eq!(parse_git_version("hub version 2.14.2"), None);
    }

    #[test]
    fn a_quoted_executable_is_read_back_as_it_was_written() {
        for path in ["/opt/by/bin/by", "/home/o'neil/.local/bin/by", "/a b/by"] {
            let command = format!("{} merge-driver %O %A %B", shell_quote(path));
            assert_eq!(executable_of(&command).as_deref(), Some(path));
        }
        assert_eq!(
            executable_of("by merge-driver %O %A %B").as_deref(),
            Some("by")
        );
    }
}
