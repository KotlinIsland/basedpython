//! concrete typeshed patches, one module per semantic adjustment. register
//! legacy-form patches in `all_patches()` and post-conversion beautifiers in
//! `all_post_patches()`, both in the crate root

pub mod any_to_dynamic;
pub mod arrow_callable;
pub mod builtins_tweaks;
pub mod cleanup;
pub mod collections_abc_home;
pub mod container_overlapping;
pub mod context_manager_abstract;
pub mod dead_symbols;
pub mod dead_typevars;
pub mod final_annotation;
pub mod final_modifier;
pub mod functools_cache;
pub mod homogeneous_tuple;
pub mod init_shorthand;
pub mod literal_unwrap;
pub mod mapping;
pub mod numeric_promotion;
pub mod output_widening;
pub(crate) mod private_names;
pub mod private_protocols;
pub mod private_type_aliases;
pub mod property_to_let;
pub mod protocol_keyword;
pub mod re_optional_groups;
pub mod redundant_none_return;
pub mod redundant_overloads;
pub mod stray_comments;
pub mod strip_typing_imports;
pub mod type_aliases;
