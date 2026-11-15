use std::path::Path;
use std::process::Command;

use insta_cmd::get_cargo_bin;

/// the version the wheel is published under, read from the manifest as the test runs
fn manifest_version() -> anyhow::Result<String> {
    let pyproject = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("pyproject.toml");
    let pyproject: toml::Table = std::fs::read_to_string(pyproject)?.parse()?;
    pyproject["project"]["version"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| anyhow::anyhow!("`pyproject.toml` has no `project.version`"))
}

fn stdout_of(args: &[&str]) -> anyhow::Result<String> {
    let output = Command::new(get_cargo_bin("by")).args(args).output()?;
    anyhow::ensure!(output.status.success(), "`by {}` failed", args.join(" "));
    Ok(String::from_utf8(output.stdout)?)
}

/// the commit and its date follow the version where the build had git to ask
#[test]
fn version_flag_names_the_manifest_version() -> anyhow::Result<()> {
    let version = manifest_version()?;
    let stdout = stdout_of(&["--version"])?;

    let rest = stdout.strip_prefix(&format!("by {version}"));
    assert!(
        rest.is_some_and(|rest| rest == "\n" || rest.starts_with(" (")),
        "`by --version` printed {stdout:?}, and the manifest is at {version}"
    );
    Ok(())
}

#[test]
fn version_command_says_what_the_flag_says() -> anyhow::Result<()> {
    assert_eq!(stdout_of(&["version"])?, stdout_of(&["--version"])?);
    Ok(())
}

/// the version alone, without the commit `--version` follows it with
#[test]
fn version_command_names_the_manifest_version_as_json() -> anyhow::Result<()> {
    let json: serde_json::Value =
        serde_json::from_str(&stdout_of(&["version", "--output-format", "json"])?)?;
    assert_eq!(json, serde_json::json!({ "version": manifest_version()? }));
    Ok(())
}
