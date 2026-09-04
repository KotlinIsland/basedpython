//! basedpython-specific rules.
//!
//! These check `.by` source for python spellings of constructs basedpython has
//! its own syntax for. They never fire on a `.py` file, where their fixes would
//! not parse.
//!
//! Rules that need a type to decide belong in `by check` rather than here: the
//! linter is per-file and typeless by construction.
mod helpers;
pub(crate) mod rules;

#[cfg(test)]
mod tests {
    use std::path::Path;

    use anyhow::Result;
    use test_case::test_case;

    use crate::registry::Rule;
    use crate::test::test_path;
    use crate::{assert_diagnostics, settings};

    #[test_case(Rule::ManualNoneCoalesce, Path::new("BY001.by"))]
    #[test_case(Rule::ManualNoneCoalesce, Path::new("BY001.py"))]
    #[test_case(Rule::ManualOptionalChain, Path::new("BY002.by"))]
    #[test_case(Rule::ManualOptionalChain, Path::new("BY002.py"))]
    #[test_case(Rule::ManualIsinstance, Path::new("BY003.by"))]
    #[test_case(Rule::ManualSuperCall, Path::new("BY004.by"))]
    #[test_case(Rule::ManualAnyAnnotation, Path::new("BY007.by"))]
    #[test_case(Rule::ManualUnpackAnnotation, Path::new("BY009.by"))]
    #[test_case(Rule::ManualTypeofAnnotation, Path::new("BY010.by"))]
    #[test_case(Rule::ManualReExport, Path::new("BY011.by"))]
    #[test_case(Rule::RedundantTypingImport, Path::new("BY012.by"))]
    #[test_case(Rule::UnnecessaryStubBody, Path::new("BY017.by"))]
    #[test_case(Rule::ManualSentinel, Path::new("BY019.by"))]
    #[test_case(Rule::ManualCastCall, Path::new("BY020.by"))]
    #[test_case(Rule::ManualProperty, Path::new("BY021.by"))]
    #[test_case(Rule::ManualModifier, Path::new("BY022.by"))]
    #[test_case(Rule::RedundantNoneCoalesce, Path::new("BY101.by"))]
    fn rules(rule_code: Rule, path: &Path) -> Result<()> {
        let snapshot = format!(
            "{}_{}",
            rule_code
                .noqa_code()
                .expect("a basedpython rule always has a noqa code"),
            path.to_string_lossy()
        );
        let diagnostics = test_path(
            Path::new("basedpython").join(path).as_path(),
            &settings::LinterSettings::for_rule(rule_code),
        )?;
        assert_diagnostics!(snapshot, diagnostics);
        Ok(())
    }
}
