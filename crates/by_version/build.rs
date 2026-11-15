use std::{error::Error, fs, path::PathBuf};

fn main() -> Result<(), Box<dyn Error>> {
    let pyproject = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?)
        .join("..")
        .join("..")
        .join("pyproject.toml");
    println!("cargo::rerun-if-changed={}", pyproject.display());

    let text = fs::read_to_string(&pyproject)
        .map_err(|error| format!("could not read `{}`: {error}", pyproject.display()))?;
    let table: toml::Table = text
        .parse()
        .map_err(|error| format!("could not parse `{}`: {error}", pyproject.display()))?;
    let version = table
        .get("project")
        .and_then(|project| project.get("version"))
        .and_then(toml::Value::as_str)
        .ok_or_else(|| format!("`{}` has no `project.version`", pyproject.display()))?;
    println!("cargo::rustc-env=BY_VERSION={version}");
    Ok(())
}
