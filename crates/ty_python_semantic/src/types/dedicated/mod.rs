pub(super) mod django;
pub(super) mod pydantic;
pub(super) mod role;
pub(super) mod sqlalchemy;

/// a supported framework that ships no inline type annotations and needs an
/// external pep 561 stubs package for precise checking. consulted by the
/// `missing-framework-stubs` lint; future frameworks with external stubs add
/// an entry here
pub(crate) struct ExternallyStubbedFramework {
    /// the framework's top-level package
    pub(crate) package: &'static str,
    /// the stubs distribution's import-visible directory (`<package>-stubs`)
    pub(crate) stubs_directory: &'static str,
    /// the distribution to install, for the diagnostic's suggestion
    pub(crate) stubs_distribution: &'static str,
}

pub(crate) const EXTERNALLY_STUBBED_FRAMEWORKS: &[ExternallyStubbedFramework] =
    &[ExternallyStubbedFramework {
        package: "django",
        stubs_directory: "django-stubs",
        stubs_distribution: "django-stubs",
    }];
