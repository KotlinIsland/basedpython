//! Rules from [flake8-no-pep420](https://pypi.org/project/flake8-no-pep420/).
pub(crate) mod rules;

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use anyhow::Result;
    use test_case::test_case;

    use crate::registry::Rule;

    use crate::assert_diagnostics;
    use crate::settings::LinterSettings;
    use crate::settings::rule_table::RuleTable;
    use crate::test::{test_path, test_resource_path};

    #[test_case(Path::new("test_fail_empty"), Path::new("example.py"))]
    #[test_case(Path::new("test_fail_nonempty"), Path::new("example.py"))]
    #[test_case(Path::new("test_ignored"), Path::new("example.py"))]
    #[test_case(Path::new("test_pass_init"), Path::new("example.py"))]
    #[test_case(Path::new("test_pass_namespace_package"), Path::new("example.py"))]
    #[test_case(Path::new("test_pass_pep723"), Path::new("script.py"))]
    #[test_case(Path::new("test_pass_pyi"), Path::new("example.pyi"))]
    #[test_case(Path::new("test_pass_script"), Path::new("script"))]
    #[test_case(Path::new("test_pass_shebang"), Path::new("example.py"))]
    fn default(path: &Path, filename: &Path) -> Result<()> {
        let snapshot = format!("{}", path.to_string_lossy());
        let p = PathBuf::from(format!(
            "flake8_no_pep420/{}/{}",
            path.display(),
            filename.display()
        ));
        let diagnostics = test_path(
            p.as_path(),
            &LinterSettings {
                namespace_packages: vec![test_resource_path(
                    "fixtures/flake8_no_pep420/test_pass_namespace_package",
                )],
                ..LinterSettings::for_rule(Rule::ImplicitNamespacePackage)
            },
        )?;
        insta::with_settings!({filters => vec![(r"\\", "/")]}, {
            assert_diagnostics!(snapshot, diagnostics);
        });
        Ok(())
    }

    /// a directory below the package root holding a basedpython module is a package, as the
    /// basedpython build writes it an `__init__.py`, but one elsewhere is not
    #[test_case(Path::new("src/pkg/a.by"))]
    #[test_case(Path::new("tools/a.by"))]
    fn package_roots(filename: &Path) -> Result<()> {
        let snapshot = format!(
            "package_roots_{}",
            filename.to_string_lossy().replace('/', "_")
        );
        let project_root = test_resource_path("fixtures/flake8_no_pep420/test_package_roots");
        let diagnostics = test_path(
            Path::new("flake8_no_pep420/test_package_roots").join(filename),
            &LinterSettings {
                rules: RuleTable::from_iter([Rule::ImplicitNamespacePackage]),
                ..LinterSettings::new(&project_root)
            },
        )?;
        insta::with_settings!({filters => vec![(r"\\", "/")]}, {
            assert_diagnostics!(snapshot, diagnostics);
        });
        Ok(())
    }
}
