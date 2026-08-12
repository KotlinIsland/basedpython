//! the basedpython native runtime
//!
//! the runtime itself is C. this crate exists to carry it: the header is embedded
//! in the binary rather than read from disk, so a `by` installed anywhere can
//! write it into a build directory without shipping a separate data file.

/// the runtime header, written next to the generated C at build time
pub const BY_H: &str = include_str!("../include/by.h");

/// the file name the header must be written under, since generated C includes it
/// by that name
pub const BY_H_NAME: &str = "by.h";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_header_is_embedded_and_self_guarded() {
        assert!(BY_H.contains("#ifndef BY_RT_H"));
        assert!(BY_H.contains("#endif"));
    }

    #[test]
    fn the_header_defines_the_types_codegen_names() {
        // codegen writes these spellings; a rename here without one there would
        // only show up as a C compile error deep in a build
        for symbol in [
            "ByTagged",
            "BY_INT_ERROR",
            "BY_FLOAT_ERROR",
            "By_BoxInt",
            "By_UnboxInt",
            "By_UnboxFloat",
            "By_IntAdd",
            "By_DecRefTagged",
            "By_ObjAdd",
            "By_ObjCompare",
            "By_Truthy",
            "By_ApplyMethodDecorators",
        ] {
            assert!(BY_H.contains(symbol), "the runtime is missing {symbol}");
        }
    }
}
