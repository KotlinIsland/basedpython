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
            "By_SpecClass",
            "By_SpecSubclass",
            "By_InterpreterMatches",
        ] {
            assert!(BY_H.contains(symbol), "the runtime is missing {symbol}");
        }
    }

    /// both constructions for a class whose fields sit past a base's instance answer
    /// with nothing rather than with something else, because the caller's only move is
    /// to leave the whole module as its interpreted definition already built it. one
    /// that raised instead would make module init fail the import
    #[test]
    fn a_layout_that_does_not_hold_up_is_refused_rather_than_raised() {
        for construction in ["By_SpecClass", "By_SpecSubclass"] {
            let body = BY_H
                .split_once(&format!("static inline PyObject *{construction}("))
                .expect("the construction is defined")
                .1
                .split_once("\n}\n")
                .expect("it ends")
                .0;
            assert!(
                body.contains("if (!By_OffsetsHoldUp((PyTypeObject *)cls"),
                "{construction} checks the finished type's offsets"
            );
            assert!(
                !body.contains("PyErr_Set") && !body.contains("PyErr_Format"),
                "{construction} raises nothing: {body}"
            );
            assert!(
                body.contains("return NULL;"),
                "{construction} answers with nothing"
            );
        }
    }
}
