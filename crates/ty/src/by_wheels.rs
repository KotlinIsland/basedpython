//! Building a release: one wheel per python, and the source distribution.
//!
//! A project builds into one wheel by default, lowered to the oldest python it
//! supports. That wheel runs everywhere, which is the point — but it means a
//! reader on 3.13 gets code written around 3.9's limits, and a `typing_extensions`
//! dependency they have no use for. Lowering each wheel to one python and tagging
//! it accordingly lets an installer hand every interpreter the best wheel it can
//! use, and a python with no wheel of its own falls back to the newest one below
//! it.
//!
//! Nothing here packages anything. `uv` is the build frontend, called once per
//! version exactly as it would be from a shell; what this adds is the part a
//! shell loop gets wrong. A release is only useful if the set is *complete* —
//! every version covered, no untagged wheel left behind to outrank the rest, and
//! nothing published at all if one of them failed. That is the whole reason this
//! is a command rather than three lines of shell.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Context;
use ruff_db::system::{OsSystem, System, SystemPath};
use ruff_python_ast::PythonVersion as AstPythonVersion;
use ty_project::{Db, ProjectDatabase, ProjectMetadata};
use ty_site_packages::PythonEnvironment;

use crate::ExitStatus;

/// The tag every wheel carries when it was not lowered for one python.
///
/// It is the one that must never appear beside the others: an installer ranks it
/// above every `py3X` tag older than the running interpreter, so a single stray
/// generic wheel silently wins over the whole set.
const UNTAGGED_WHEEL_MARKER: &str = "-py3-none-any.whl";

#[allow(clippy::print_stderr)]
pub(crate) fn cmd_build_wheels(
    out: Option<&Path>,
    stamps: &[String],
) -> anyhow::Result<ExitStatus> {
    let cwd = std::env::current_dir().context("failed to get current directory")?;
    let destination = cwd.join(out.unwrap_or(Path::new("dist")));

    let uv = find_uv(&cwd)?;
    let versions = wheel_versions(&cwd)?;
    if versions.is_empty() {
        anyhow::bail!(
            "no python versions to build for — `build.wheel-versions` is empty, \
             and a release with no wheels in it is not a release"
        );
    }

    eprintln!(
        "building for {}",
        versions
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(", ")
    );

    // nothing reaches the destination until everything succeeded. a half-built
    // release published is worse than no release: the versions that did build
    // outrank nothing, so an interpreter whose wheel is missing quietly takes an
    // older one and no one is told
    let staging = tempfile::TempDir::new().context("failed to create temp directory")?;

    // a release is one build, however many times the frontend is called inside
    // it. the stamps are settled here and handed down, so every wheel and the
    // source distribution report the same commit and the same moment rather than
    // each reading the clock — and, if something lands mid-release, the same
    // commit rather than two. `PYTHON_VERSION` is left out on purpose: each
    // wheel is lowered to a different python and settles that one for itself
    //
    // only the stamps travel. the rest of `--min-version`'s neighbours do not
    // reach the builds inside a `--wheels` run today
    let mut settled = crate::by_stamps::parse_explicit(stamps)?;
    crate::by_stamps::fill_discovered(&mut settled, &cwd, None);
    let settled = serde_json::to_string(&settled)
        .context("could not describe the stamps this release settled")?;

    run_uv(&uv, &cwd, &["build", "--sdist"], staging.path(), &settled)?;
    for version in &versions {
        run_uv(
            &uv,
            &cwd,
            &[
                "build",
                "--wheel",
                "--config-setting",
                &format!("python-version={version}"),
            ],
            staging.path(),
            &settled,
        )?;
    }

    let built = verify(staging.path(), &versions)?;
    verify_destination(&destination, &built)?;
    fs::create_dir_all(&destination)
        .with_context(|| format!("could not create {}", destination.display()))?;
    for artifact in &built {
        let name = artifact
            .file_name()
            .context("a built artifact has no name")?;
        fs::copy(artifact, destination.join(name))
            .with_context(|| format!("could not write {}", destination.join(name).display()))?;
    }

    eprintln!();
    for artifact in &built {
        if let Some(name) = artifact.file_name().and_then(std::ffi::OsStr::to_str) {
            eprintln!("{}", destination.join(name).display());
        }
    }
    let wheels = built
        .iter()
        .filter(|artifact| {
            artifact
                .extension()
                .is_some_and(|extension| extension == "whl")
        })
        .count();
    eprintln!("\n{wheels} wheel(s) and a source distribution");
    Ok(ExitStatus::Success)
}

/// Whether this is something a release is made of.
///
/// The frontend leaves more than artifacts in its output directory — `uv` writes
/// a `.gitignore` — and a release is the wheels and the source distribution,
/// not whatever else is in the folder.
#[expect(
    clippy::case_sensitive_file_extension_comparisons,
    reason = "the name is lowercased first, and `.tar.gz` is two extensions to `Path`"
)]
fn is_artifact(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(std::ffi::OsStr::to_str) else {
        return false;
    };
    let name = name.to_ascii_lowercase();
    name.ends_with(".whl") || name.ends_with(".tar.gz")
}

/// Check that what was built is a release rather than a pile of wheels.
fn verify(staging: &Path, versions: &[String]) -> anyhow::Result<Vec<PathBuf>> {
    let mut artifacts: Vec<PathBuf> = fs::read_dir(staging)
        .with_context(|| format!("could not read {}", staging.display()))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_file() && is_artifact(path))
        .collect();
    artifacts.sort();

    let names: BTreeSet<String> = artifacts
        .iter()
        .filter_map(|path| path.file_name()?.to_str().map(str::to_owned))
        .collect();

    // an untagged wheel outranks every `py3X` tag below the running python, so
    // one of these in the set makes every wheel beside it unreachable
    if let Some(untagged) = names
        .iter()
        .find(|name| name.ends_with(UNTAGGED_WHEEL_MARKER))
    {
        anyhow::bail!(
            "`{untagged}` is not tagged for a python version, and an installer \
             would prefer it over the wheels that are — so none of them would \
             ever be chosen"
        );
    }

    for version in versions {
        let tag = format!("-py{}-", version.replace('.', ""));
        if !names.iter().any(|name| name.contains(&tag)) {
            anyhow::bail!(
                "nothing was built for python {version}, so an interpreter of that \
                 version would fall back to an older wheel without being told"
            );
        }
    }

    if !names.iter().any(|name| name.ends_with(".tar.gz")) {
        anyhow::bail!("no source distribution was built");
    }

    Ok(artifacts)
}

/// Check that nothing already in the destination will outrank what is about to
/// be put there.
///
/// This is where the release is published *from*, so it is where a stale artifact
/// does its damage. An untagged wheel of the version being built is the one that
/// matters most — an installer prefers it to every tagged wheel, so the whole set
/// becomes unreachable — but a wheel for a version no longer built is stale in the
/// same way, and both would be uploaded by a `publish` that takes the directory as
/// it finds it.
///
/// Only this version is examined. An artifact of a *different* version is a
/// previous release, and a resolver picks the version before it picks a tag, so it
/// takes nothing away from this one.
fn verify_destination(destination: &Path, built: &[PathBuf]) -> anyhow::Result<()> {
    let Ok(entries) = fs::read_dir(destination) else {
        // nothing there yet, which is the common case and nothing to check
        return Ok(());
    };

    let ours: BTreeSet<&str> = built
        .iter()
        .filter_map(|path| path.file_name()?.to_str())
        .collect();
    let Some(release) = built.iter().find_map(|path| release_prefix(path)) else {
        return Ok(());
    };

    let stale: Vec<String> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_file() && is_artifact(path))
        .filter_map(|path| path.file_name()?.to_str().map(str::to_owned))
        .filter(|name| !ours.contains(name.as_str()))
        .filter(|name| release_prefix(Path::new(name)).as_deref() == Some(&release))
        .collect();

    if !stale.is_empty() {
        anyhow::bail!(
            "`{}` already holds {} of this release that this build did not produce:\n       {}\n       \
             they would be published alongside it, and an untagged wheel among them \
             outranks every wheel that is tagged — remove them and build again",
            destination.display(),
            if stale.len() == 1 {
                "an artifact"
            } else {
                "artifacts"
            },
            stale.join("\n       "),
        );
    }
    Ok(())
}

/// The `name-version` an artifact belongs to, which is what makes two of them
/// part of the same release.
fn release_prefix(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    if let Some(stem) = name.strip_suffix(".tar.gz") {
        return Some(stem.to_owned());
    }
    let stem = name.strip_suffix(".whl")?;
    // `name-version-python-abi-platform`: everything before the three tag fields
    let mut fields: Vec<&str> = stem.split('-').collect();
    if fields.len() < 4 {
        return None;
    }
    fields.truncate(fields.len() - 3);
    Some(fields.join("-"))
}

/// The versions to build for: what the project lists, else every version from
/// the one it targets up to the newest this release can emit for.
fn wheel_versions(cwd: &Path) -> anyhow::Result<Vec<String>> {
    let Some(sys_cwd) = SystemPath::from_std_path(cwd) else {
        anyhow::bail!("non-utf8 path: {}", cwd.display());
    };
    let system = OsSystem::new(sys_cwd);
    let metadata = ProjectMetadata::discover(sys_cwd, &system)
        .with_context(|| format!("failed to discover project at {sys_cwd}"))?;
    let db = ProjectDatabase::use_defaults(metadata, system);

    if let Some(listed) = db
        .project()
        .metadata(&db)
        .options()
        .build
        .as_ref()
        .and_then(|build| build.wheel_versions.as_ref())
    {
        return Ok(listed.iter().map(|version| (**version).clone()).collect());
    }

    // the floor is what the project already declares it supports, so there is
    // nothing else to ask: `requires-python` is what an installer enforces, and
    // building below it would ship a wheel no one may install
    let floor = db.project().program(&db).python_version(&db);
    Ok(AstPythonVersion::iter()
        .filter(|version| *version >= floor && *version <= AstPythonVersion::latest())
        .map(|version| version.to_string())
        .collect())
}

/// Find the `uv` that will do the packaging.
///
/// The project environment first, for the same reason `by run` looks there: it
/// is the environment this project is developed in. `PATH` after it.
fn find_uv(cwd: &Path) -> anyhow::Result<PathBuf> {
    if let Some(sys_cwd) = SystemPath::from_std_path(cwd) {
        let system = OsSystem::new(sys_cwd);
        if let Ok(Some(environment)) = PythonEnvironment::discover(sys_cwd, &system) {
            let binaries = if cfg!(windows) {
                environment.sys_prefix().join("Scripts")
            } else {
                environment.sys_prefix().join("bin")
            };
            let candidate = binaries.join(if cfg!(windows) { "uv.exe" } else { "uv" });
            if system.is_file(&candidate) {
                return Ok(PathBuf::from(candidate.as_str()));
            }
        }
    }

    which_uv().context(
        "could not find `uv`, which is what builds the wheels — \
         `by build --wheels` drives it rather than packaging anything itself. \
         install it, or build a single wheel with `uv build`",
    )
}

fn which_uv() -> Option<PathBuf> {
    let name = if cfg!(windows) { "uv.exe" } else { "uv" };
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|directory| directory.join(name))
            .find(|candidate| candidate.is_file())
    })
}

#[allow(clippy::print_stderr)]
fn run_uv(
    uv: &Path,
    cwd: &Path,
    arguments: &[&str],
    out: &Path,
    settled_stamps: &str,
) -> anyhow::Result<()> {
    let status = Command::new(uv)
        .args(arguments)
        .arg("--out-dir")
        .arg(out)
        .current_dir(cwd)
        // reaches the `by build` the frontend eventually runs, through the
        // backend, which passes its environment on
        .env(crate::by_stamps::SETTLED_STAMPS, settled_stamps)
        .status()
        .with_context(|| format!("could not run `{}`", uv.display()))?;
    if !status.success() {
        anyhow::bail!(
            "`uv {}` failed — nothing was written, because a release missing one \
             of its wheels hands an interpreter an older one without saying so",
            arguments.join(" ")
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn staged(names: &[&str]) -> tempfile::TempDir {
        let directory = tempfile::tempdir().expect("tempdir");
        for name in names {
            fs::write(directory.path().join(name), "").expect("write");
        }
        directory
    }

    #[test]
    fn a_complete_set_verifies() {
        let directory = staged(&[
            "thing-1.0.tar.gz",
            "thing-1.0-py39-none-any.whl",
            "thing-1.0-py310-none-any.whl",
        ]);
        let built = verify(directory.path(), &["3.9".to_owned(), "3.10".to_owned()])
            .expect("a complete set");
        assert_eq!(built.len(), 3);
    }

    /// the failure this command exists to prevent: an untagged wheel outranks
    /// every `py3X` tag below the running python, so one of them makes the whole
    /// set unreachable
    #[test]
    fn an_untagged_wheel_beside_the_others_is_refused() {
        let directory = staged(&[
            "thing-1.0.tar.gz",
            "thing-1.0-py39-none-any.whl",
            "thing-1.0-py3-none-any.whl",
        ]);
        let error = verify(directory.path(), &["3.9".to_owned()]).expect_err("refused");
        assert!(error.to_string().contains("not tagged"), "{error}");
    }

    #[test]
    fn a_version_with_no_wheel_is_refused() {
        let directory = staged(&["thing-1.0.tar.gz", "thing-1.0-py39-none-any.whl"]);
        let error =
            verify(directory.path(), &["3.9".to_owned(), "3.13".to_owned()]).expect_err("refused");
        assert!(error.to_string().contains("python 3.13"), "{error}");
    }

    #[test]
    fn a_release_without_a_source_distribution_is_refused() {
        let directory = staged(&["thing-1.0-py39-none-any.whl"]);
        let error = verify(directory.path(), &["3.9".to_owned()]).expect_err("refused");
        assert!(error.to_string().contains("source distribution"), "{error}");
    }

    /// the release an artifact belongs to is what makes two of them the same
    /// release, and it is everything before the three tag fields
    #[test]
    fn an_artifact_names_the_release_it_belongs_to() {
        assert_eq!(
            release_prefix(Path::new("thing-1.0-py39-none-any.whl")).as_deref(),
            Some("thing-1.0")
        );
        assert_eq!(
            release_prefix(Path::new("thing-1.0.tar.gz")).as_deref(),
            Some("thing-1.0")
        );
        // a version with its own hyphens still ends where the tags begin
        assert_eq!(
            release_prefix(Path::new("thing-1.0.dev1-py3-none-any.whl")).as_deref(),
            Some("thing-1.0.dev1")
        );
        assert_eq!(release_prefix(Path::new("nonsense.whl")), None);
    }

    /// the failure the whole command exists to prevent, in the one place it
    /// actually happens: a stale untagged wheel left in the directory a release is
    /// published from outranks every wheel this build just tagged
    #[test]
    fn a_stale_untagged_wheel_in_the_destination_is_refused() {
        let destination = staged(&["thing-1.0-py3-none-any.whl", "thing-1.0-py39-none-any.whl"]);
        let built = vec![
            destination.path().join("thing-1.0-py39-none-any.whl"),
            destination.path().join("thing-1.0.tar.gz"),
        ];
        let error = verify_destination(destination.path(), &built).expect_err("refused");
        let message = error.to_string();
        assert!(message.contains("thing-1.0-py3-none-any.whl"), "{message}");
        assert!(message.contains("outranks"), "{message}");
    }

    /// a version this build no longer produces is stale in the same way — it would
    /// be published, and be chosen by the interpreter it is tagged for
    #[test]
    fn a_wheel_for_a_version_no_longer_built_is_refused() {
        let destination = staged(&["thing-1.0-py38-none-any.whl", "thing-1.0-py39-none-any.whl"]);
        let built = vec![destination.path().join("thing-1.0-py39-none-any.whl")];
        let error = verify_destination(destination.path(), &built).expect_err("refused");
        assert!(error.to_string().contains("py38"), "{error}");
    }

    /// a previous release is not a threat to this one: a resolver picks the version
    /// before it picks a tag, so an older version's wheels take nothing away
    #[test]
    fn artifacts_of_another_release_are_left_alone() {
        let destination = staged(&["thing-0.9-py3-none-any.whl", "thing-1.0-py39-none-any.whl"]);
        let built = vec![destination.path().join("thing-1.0-py39-none-any.whl")];
        verify_destination(destination.path(), &built).expect("another release is not stale");
    }

    #[test]
    fn rebuilding_over_this_releases_own_artifacts_is_fine() {
        let destination = staged(&["thing-1.0-py39-none-any.whl", "thing-1.0.tar.gz"]);
        let built = vec![
            destination.path().join("thing-1.0-py39-none-any.whl"),
            destination.path().join("thing-1.0.tar.gz"),
        ];
        verify_destination(destination.path(), &built).expect("replacing our own is fine");
    }

    #[test]
    fn a_destination_that_does_not_exist_yet_is_fine() {
        let directory = tempfile::tempdir().expect("tempdir");
        let absent = directory.path().join("dist");
        verify_destination(&absent, &[absent.join("thing-1.0.tar.gz")]).expect("nothing to check");
    }

    /// `py310` must not be read as `py31`, which is why the tag is matched with
    /// its separators rather than as a prefix
    #[test]
    fn a_version_is_not_matched_by_a_shorter_one() {
        let directory = staged(&["thing-1.0.tar.gz", "thing-1.0-py310-none-any.whl"]);
        let error = verify(directory.path(), &["3.1".to_owned()]).expect_err("refused");
        assert!(error.to_string().contains("python 3.1"), "{error}");
    }
}
