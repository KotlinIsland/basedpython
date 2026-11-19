#![warn(
    clippy::disallowed_methods,
    reason = "Prefer System trait methods over std methods in ty crates"
)]
use ruff_db::vendored::VendoredFileSystem;
use std::sync::LazyLock;

/// The source commit of the vendored typeshed.
pub const SOURCE_COMMIT: &str =
    include_str!("../vendor/typeshed/source_commit.txt").trim_ascii_end();

static_assertions::const_assert_eq!(SOURCE_COMMIT.len(), 40);

/// A SHA-256 of every vendored stub's path and contents, computed when the zip is built.
///
/// This, not [`SOURCE_COMMIT`], names the stubs: they are typeshed rewritten for basedpython,
/// so builds on one typeshed commit can hold different stubs, and a copy on disk named after
/// the commit is a copy of whichever build wrote it first.
pub const CONTENT_HASH: &str = env!("TY_VENDORED_CONTENT_HASH");

static_assertions::const_assert_eq!(CONTENT_HASH.len(), 64);

// The file path here is hardcoded in this crate's `build.rs` script.
// Luckily this crate will fail to build if this file isn't available at build time.
static TYPESHED_ZIP_BYTES: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/zipped_typeshed.zip"));

pub fn file_system() -> &'static VendoredFileSystem {
    static VENDORED_TYPESHED_STUBS: LazyLock<VendoredFileSystem> =
        LazyLock::new(|| VendoredFileSystem::new_static(TYPESHED_ZIP_BYTES).unwrap());
    &VENDORED_TYPESHED_STUBS
}

#[cfg(test)]
mod tests {
    use std::io::{self, Read};
    use std::path::Path;

    use ruff_db::vendored::VendoredPath;

    use super::*;

    #[test]
    fn typeshed_zip_created_at_build_time() {
        let mut typeshed_zip_archive =
            zip::ZipArchive::new(io::Cursor::new(TYPESHED_ZIP_BYTES)).unwrap();

        let mut functools_module_stub = typeshed_zip_archive
            .by_name("stdlib/functools.byi")
            .unwrap();
        assert!(functools_module_stub.is_file());

        let mut functools_module_stub_source = String::new();
        functools_module_stub
            .read_to_string(&mut functools_module_stub_source)
            .unwrap();

        // `def update_wrapper` rather than `def update_wrapper(`: the
        // basedpython typeshed converts it to a pep 695 generic function, so a
        // type-parameter list `[...]` follows the name
        assert!(functools_module_stub_source.contains("def update_wrapper"));
    }

    /// The hash the build names the stubs by is the hash of what the zip holds: every file,
    /// in the zip's order, with its path. A file the zip gains or changes changes the name.
    #[test]
    fn content_hash_covers_every_file_in_the_zip() {
        use sha2::{Digest, Sha256};

        let mut archive = zip::ZipArchive::new(io::Cursor::new(TYPESHED_ZIP_BYTES)).unwrap();
        let mut hasher = Sha256::new();
        let mut files = 0;
        for index in 0..archive.len() {
            let mut entry = archive.by_index(index).unwrap();
            if !entry.is_file() {
                continue;
            }
            let name = entry.name().to_owned();
            let mut contents = Vec::new();
            entry.read_to_end(&mut contents).unwrap();
            hasher.update((name.len() as u64).to_le_bytes());
            hasher.update(name.as_bytes());
            hasher.update((contents.len() as u64).to_le_bytes());
            hasher.update(&contents);
            files += 1;
        }
        let hash = format!("{:x}", hasher.finalize());

        assert!(files > 100, "only {files} files in the vendored zip");
        assert_eq!(hash, CONTENT_HASH);
    }

    #[test]
    fn typeshed_vfs_consistent_with_vendored_stubs() {
        let vendored_typeshed_dir = Path::new("vendor/typeshed").canonicalize().unwrap();
        let vendored_typeshed_stubs = file_system();

        let mut empty_iterator = true;
        for entry in walkdir::WalkDir::new(&vendored_typeshed_dir).min_depth(1) {
            empty_iterator = false;
            let entry = entry.unwrap();
            let absolute_path = entry.path();
            let file_type = entry.file_type();

            let relative_path = absolute_path
                .strip_prefix(&vendored_typeshed_dir)
                .unwrap_or_else(|_| {
                    panic!("Expected {absolute_path:?} to be a child of {vendored_typeshed_dir:?}")
                });

            let vendored_path = <&VendoredPath>::try_from(relative_path)
                .unwrap_or_else(|_| panic!("Expected {relative_path:?} to be valid UTF-8"));

            assert!(
                vendored_typeshed_stubs.exists(vendored_path),
                "Expected {vendored_path:?} to exist in the `VendoredFileSystem`!

                Vendored file system:

                {vendored_typeshed_stubs:#?}
                "
            );

            let vendored_path_kind = vendored_typeshed_stubs
                .metadata(vendored_path)
                .unwrap_or_else(|_| {
                    panic!(
                        "Expected metadata for {vendored_path:?} to be retrievable from the `VendoredFileSystem!

                        Vendored file system:

                        {vendored_typeshed_stubs:#?}
                        "
                    )
                })
                .kind();

            assert_eq!(
                vendored_path_kind.is_directory(),
                file_type.is_dir(),
                "{vendored_path:?} had type {vendored_path_kind:?}, inconsistent with fs path {relative_path:?}: {file_type:?}"
            );
        }

        assert!(
            !empty_iterator,
            "Expected there to be at least one file or directory in the vendored typeshed stubs!"
        );
    }
}
