//! BIR → C11
//!
//! the emitted C is deliberately dull: one C local per BIR register, one C label
//! per block, and `goto` for every edge. all of the interesting decisions were
//! made in BIR, and the C compiler is left to do register allocation and
//! instruction selection, which it does far better than we would.
//!
//! ## ownership
//!
//! until a refcount pass lands, codegen uses a simple sound discipline:
//! **every register owns the value it holds**, with one exception —
//! **parameters are borrowed**. the caller keeps ownership of an argument for the
//! duration of the call, so a native call site needs no retain and the callee's
//! cleanup must not release a parameter.
//!
//! the exception has an exception: a parameter the body *reassigns* would have its
//! incoming value released by that write, so such a parameter is retained on entry
//! and released like any other register. `owned_registers` decides which.
//!
//! a write computes into a temporary first (the destination is frequently also an
//! operand), then releases, then stores. a returned value is retained once more so
//! the caller receives an owned reference. this is not the code an optimizer would
//! produce; it is code whose correctness can be read off locally.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fmt::Write;
use std::ptr;

use by_ir::function::{
    Binding, ClassBase, ClassIr, Function, KeywordValue, ModuleIr, PropertyIr, RegisterDecl,
    SlotAlias, Surface, cleaned_doc,
};
use by_ir::ops::{BinOp, BlockId, CmpOp, Mutation, Op, RegisterId, Terminator, UnaryOp, Value};
use by_ir::rtype::{Primitive, RType, tuple_mangle};

/// emit a complete C translation unit for a module
/// a default's value in the parameter's representation
///
/// the wrapper owns its arguments, so a refcounted default has to be a *new*
/// reference — an interned literal handed straight over would be released twice
/// the C expression that fills a place with its default
///
/// an immediate has no representation of its own, so it has to be written in the
/// one the *place* has. into an object that means boxing: `Value::None` is the
/// unboxed `None`, a bare byte, and handing that to `By_NewRef` produced a NULL
/// the error check read as a failure with no exception behind it
fn default_expr(ty: &RType, default: &Value) -> String {
    let expr = value_expr(default);
    if !matches!(
        ty,
        RType::Primitive(Primitive::Str | Primitive::Object | Primitive::List | Primitive::Dict)
    ) {
        return expr;
    }
    match default {
        Value::None => "By_NewRef(Py_None)".to_string(),
        Value::Bool(value) | Value::Bit(value) => {
            format!("By_NewRef(Py_{})", if *value { "True" } else { "False" })
        }
        Value::Int(_) => format!("By_BoxInt({expr})"),
        Value::Fixed(value) => format!("By_BoxInt(By_ShortFrom({value}))"),
        Value::Float(_) => format!("By_BoxFloat({expr})"),
        Value::Str(_) | Value::Bytes(_) | Value::Register(_) => format!("By_NewRef({expr})"),
    }
}

/// the C member name for a field, matching `FieldDecl::member`
/// the declaration for `class.field`, where the module emits that class
///
/// `GetField` and `SetField` name their class, so the presence byte an optional field
/// carries is reachable from the op alone
fn field_decl<'a>(
    module: &'a ModuleIr,
    class: &str,
    field: &str,
) -> Option<&'a by_ir::function::FieldDecl> {
    module
        .classes
        .iter()
        .find(|candidate| candidate.name == class)
        .and_then(|candidate| candidate.fields.iter().find(|decl| decl.name == field))
}

/// whether `function` is the method a resumable class steps through
///
/// its `return` is not an ordinary one: it is the end of a generator or a coroutine,
/// and how it is reported is what the send slot and the iterator protocol disagree
/// about
fn resumes(module: &ModuleIr, function: &Function) -> bool {
    let Some(owner) = &function.owner else {
        return false;
    };
    module.classes.iter().any(|class| {
        &class.name == owner
            && class
                .resume
                .as_ref()
                .is_some_and(|resume| resume.method == function.name)
    })
}

/// which surface the runtime should word a pep 479 conversion after.
///
/// the runtime cannot work this out for itself: an emitted state object is an ordinary
/// static type, so there is no `PyCoro_CheckExact` to ask the way cpython asks its own
fn frame_kind(surface: Surface) -> &'static str {
    match surface {
        Surface::Generator => "BY_FRAME_GENERATOR",
        Surface::Coroutine => "BY_FRAME_COROUTINE",
        Surface::AsyncGenerator => "BY_FRAME_ASYNC_GENERATOR",
    }
}

fn mangle_member(name: &str) -> String {
    by_ir::function::FieldDecl {
        name: name.to_string(),
        ty: RType::OBJECT,
        default: None,
        optional: false,
        defaulted_by: None,
    }
    .member()
}

/// the state field of a generator's state object, as the frontend names it
const GENERATOR_STATE: &str = "$state";
/// the field holding what `send` passed in
const GENERATOR_SENT: &str = "$sent";
/// the field carrying an exception `throw` or `close` wants raised at the suspension
const GENERATOR_THROWN: &str = "$thrown";

/// a `#line` pointing at `range`'s start, when the module carries a line table
fn line_directive(module: &ModuleIr, range: Option<(u32, u32)>) -> String {
    let (Some(lines), Some((start, _))) = (&module.lines, range) else {
        return String::new();
    };
    // the path goes through the C string escaper: a windows path is full of
    // backslashes, and each one is an escape to a C compiler
    format!("#line {} {}\n", lines.line(start), c_string(&lines.path))
}

pub fn emit_module(module: &ModuleIr) -> String {
    let mut out = String::new();
    out.push_str("/* generated by basedpython — do not edit */\n");
    out.push_str("#include \"by.h\"\n\n");
    // a native function resolves an unowned name through this, the way
    // `LOAD_GLOBAL` does. declared here because the bodies below read it; set
    // once, from the exec slot
    out.push_str("static PyObject *by_module_dict = NULL;\n\n");

    // a function with a deferring boundary keeps a handle to its interpreted
    // definition. it has to be taken before `PyModule_AddFunctions` installs the
    // compiled name over it, which is the only moment the twin is reachable
    let mut any_defers = false;
    for function in module.all_functions() {
        if function.defers() {
            any_defers = true;
            let _ = writeln!(
                out,
                "static PyObject *{} = NULL;",
                function.interpreted_symbol(module.name.dotted())
            );
        }
    }
    if any_defers {
        out.push('\n');
    }

    // the names of the instance structs, ahead of everything that spells one. an
    // instance is only ever a *pointer* to its struct, so a name is all any of them
    // needs, and the definitions cannot come first: a tuple struct may hold an
    // instance in a slot and a class field may be a tuple, so neither group can be
    // written before the other
    for class in &module.classes {
        let name = class.struct_name(module.name.dotted());
        let _ = writeln!(out, "typedef struct {name} {name};");
    }
    if !module.classes.is_empty() {
        out.push('\n');
    }

    for tuple in collect_tuples(module).values() {
        out.push_str(&emit_tuple_struct(module, tuple));
    }

    // a string literal is interned once at module init and read as a *borrowed*
    // reference from then on. materializing it per use with
    // `PyUnicode_FromString` created a reference nobody released — a leak that
    // `gc.get_objects` cannot see, because `str` is not GC-tracked
    let literals = collect_string_literals(module);
    LITERALS.with_borrow_mut(|slot| slot.clone_from(&literals));
    for (index, literal) in literals.iter().enumerate() {
        let _ = writeln!(
            out,
            // the literal names itself in a *line* comment: a block one is closed by
            // any `*/` the text contains, and `\"a */ b\"` then broke the emitted C
            // outright. every escape leaves the text on one line, so the comment ends
            // where the declaration does
            "static PyObject *by_str{index} = NULL; // {}",
            c_string(literal)
        );
    }
    if !literals.is_empty() {
        out.push('\n');
    }

    // the same bargain for a bytes literal, which is built rather than interned:
    // cpython interns no `bytes` beyond the empty one and the single characters
    let byte_literals = collect_bytes_literals(module);
    BYTE_LITERALS.with_borrow_mut(|slot| slot.clone_from(&byte_literals));
    for (index, literal) in byte_literals.iter().enumerate() {
        let _ = writeln!(
            out,
            "static PyObject *by_bytes{index} = NULL; // {}",
            c_byte_string(literal)
        );
    }
    if !byte_literals.is_empty() {
        out.push('\n');
    }

    for class in &module.classes {
        out.push_str(&emit_class_struct(module, class));
    }
    for class in &module.classes {
        // every class a name reaches is a heap type built from a spec at module init —
        // so its name is a pointer rather than a static struct. only a generator's state
        // and a closure's environment stay static
        let heap = heap_type(module, class);
        let type_name = class.type_name(module.name.dotted());
        let _ = writeln!(
            out,
            "{} {type_name};",
            if heap {
                "static PyObject *"
            } else {
                "static PyTypeObject"
            }
        );
        // with the declaration, because an adapter emitted before the type itself
        // still has to name it — an arithmetic slot asks which operand is ours
        let _ = writeln!(
            out,
            "#define {type_name}_OBJ {}",
            if heap {
                type_name.clone()
            } else {
                format!("((PyObject *)&{type_name})")
            }
        );
    }
    // refused until import arms it, which is the answer that keeps every dispatch site on
    // the protocol call — so a module whose init failed part way through is slow rather
    // than wrong
    for (class, method) in dispatch_licences(module) {
        let _ = writeln!(
            out,
            "static ByMethodLicence {} = BY_METHOD_LICENCE_INIT;",
            dispatch_licence(module, &class, &method)
        );
    }
    if !module.classes.is_empty() {
        out.push('\n');
    }

    for function in module.all_functions() {
        let _ = writeln!(out, "{};", signature(module, function));
    }
    // the wrappers are declared before the method tables that name them, and the
    // tables before the bodies — because a `MakeClosure` in a body names a table.
    // interleaving per class made that a forward reference
    for class in &module.classes {
        for method in &class.methods {
            let _ = writeln!(
                out,
                "static PyObject *{}(PyObject *self, PyObject *const *args, Py_ssize_t nargs, PyObject *kwnames);",
                method.wrapper_symbol(module.name.dotted())
            );
        }
    }
    out.push('\n');

    for class in &module.classes {
        out.push_str(&emit_class_type(module, class));
    }
    for class in &module.classes {
        for method in &class.methods {
            out.push_str(&emit_function(module, method));
            out.push('\n');
            // a method's wrapper takes the receiver from the `self` slot rather
            // than from the argument vector, which is how `METH_FASTCALL` on a
            // type presents it. a `staticmethod` has nothing there and binds every
            // parameter it declares, exactly as a module-level function does
            out.push_str(&emit_wrapper(
                module,
                method,
                method.binding.takes_slot_zero_from_self(),
            ));
            out.push('\n');
        }
    }

    for function in &module.functions {
        out.push_str(&emit_function(module, function));
        out.push('\n');
    }

    for function in &module.functions {
        if function.exported {
            out.push_str(&emit_wrapper(module, function, false));
            out.push('\n');
        }
    }

    out.push_str(&emit_module_init(module));
    out
}

/// every distinct fixed-length tuple layout the module mentions, so each gets one
/// struct definition
///
/// keyed by the name the struct will carry rather than by the slot types, because one
/// name can only be defined once. the two part company at an instance slot, which is
/// the same pointer to the same struct whether or not the register type it came from
/// was the exact one — a set of slot types would ask for that struct twice
fn collect_tuples(module: &ModuleIr) -> BTreeMap<String, Vec<RType>> {
    let mut tuples = BTreeMap::new();
    let visit = |ty: &RType, tuples: &mut BTreeMap<String, Vec<RType>>| {
        if let RType::Tuple(items) = ty {
            tuples.insert(tuple_mangle(items), items.to_vec());
        }
    };
    // every function, methods included: a struct the emitter never declares is a
    // compile error in generated C rather than a missing optimization
    for function in module.all_functions() {
        visit(&function.ret, &mut tuples);
        for decl in &function.registers {
            visit(&decl.ty, &mut tuples);
        }
    }
    for class in &module.classes {
        for field in &class.fields {
            visit(&field.ty, &mut tuples);
        }
    }
    tuples
}

/// every immediate the module mentions
///
/// every function, methods included, and the *defaults* as well as the ops — a
/// literal a collector misses is an undefined static in the generated C
fn module_values(module: &ModuleIr) -> impl Iterator<Item = &Value> {
    module.all_functions().flat_map(|function| {
        let defaults = function.defaults.iter().flatten();
        let body = function.blocks.iter().flat_map(|block| {
            block
                .ops
                .iter()
                .flat_map(Op::operands)
                .chain(block.terminator.operands())
        });
        defaults.chain(body)
    })
}

/// every distinct string literal in the module, in a stable order
fn collect_string_literals(module: &ModuleIr) -> Vec<String> {
    let mut seen = BTreeSet::new();
    for value in module_values(module) {
        if let Value::Str(text) = value {
            seen.insert(text.clone());
        }
    }
    seen.into_iter().collect()
}

/// every distinct bytes literal in the module, in a stable order
fn collect_bytes_literals(module: &ModuleIr) -> Vec<Box<[u8]>> {
    let mut seen = BTreeSet::new();
    for value in module_values(module) {
        if let Value::Bytes(data) = value {
            seen.insert(data.clone());
        }
    }
    seen.into_iter().collect()
}

fn emit_tuple_struct(module: &ModuleIr, items: &[RType]) -> String {
    let mut out = String::from("typedef struct {");
    if items.is_empty() {
        out.push_str(" char _empty;");
    }
    for (index, item) in items.iter().enumerate() {
        let _ = write!(out, " {} f{index};", ctype(module, item));
    }
    let name = format!("ByTuple{}", tuple_mangle(items));
    let _ = writeln!(out, " }} {name};");
    out.push_str(&emit_tuple_box(items, &name));
    out
}

/// the two ways a fixed-length tuple held in registers becomes the real `tuple` a
/// python caller reads back
///
/// this is the only place the object is built for a value that crossed a native
/// boundary, so it is built exactly once per call — which is what makes it the same
/// fresh object the display in the body would have handed back. the borrowing form
/// leaves the caller's fields alone; the owning form releases them, which is what a
/// wrapper returning the result of a native call needs
fn emit_tuple_box(items: &[RType], name: &str) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "static inline PyObject *{name}_box({name} v) {{");
    if items.is_empty() {
        // `()` is one shared object, so there is nothing to fill and no array to
        // declare — C has no zero-length one
        out.push_str("    (void)v; return PyTuple_New(0);\n}\n");
    } else {
        let count = items.len();
        let _ = writeln!(out, "    PyObject *by_i[{count}];");
        for (index, item) in items.iter().enumerate() {
            let _ = writeln!(
                out,
                "    by_i[{index}] = {};",
                box_borrowed(item, &format!("v.f{index}"))
            );
        }
        let any_null = (0..count)
            .map(|index| format!("by_i[{index}] == NULL"))
            .collect::<Vec<_>>()
            .join(" || ");
        let release = (0..count)
            .map(|index| format!("Py_XDECREF(by_i[{index}]);"))
            .collect::<Vec<_>>()
            .join(" ");
        // only the *failed* build releases them here: `By_BuildTuple` takes the
        // references it is handed, so releasing them after a successful one would
        // leave the tuple holding a count it no longer owns
        let _ = writeln!(out, "    if ({any_null}) {{ {release} return NULL; }}");
        let _ = writeln!(out, "    return By_BuildTuple(by_i, {count});");
        out.push_str("}\n");
    }
    let _ = writeln!(out, "static inline PyObject *{name}_box_owned({name} v) {{");
    let _ = writeln!(out, "    PyObject *by_t = {name}_box(v);");
    for (index, item) in items.iter().enumerate() {
        if let Some(release) = dec_ref(item, &format!("v.f{index}")) {
            let _ = writeln!(out, "    {release}");
        }
    }
    out.push_str("    return by_t;\n}\n");
    out
}

/// whether the collector can follow this field, which it can where the field is held as
/// a plain `PyObject *`
///
/// a tagged `int` is either not an object at all or a `PyLong`, and neither can be part
/// of a cycle; an unboxed buffer holds no references either
fn collectable(field: &by_ir::function::FieldDecl) -> bool {
    matches!(
        field.ty,
        RType::Instance { .. }
            | RType::Primitive(Primitive::Object | Primitive::Str | Primitive::List)
    )
}

/// `tp_traverse` and `tp_clear` for a class that owns its layout and keeps an instance
/// dict beside it
///
/// the fields are this class's whole struct — a subclass's begins with its base's and is
/// cloned into it, so there is no base to chain to and nothing of the base's is missed.
/// the type is visited because an instance of a heap type holds a reference to it, and
/// the dict is visited because that is the whole reason the type is collected at all
fn emit_collected_instance(module: &ModuleIr, class: &ClassIr) -> String {
    let struct_name = class.struct_name(module.name.dotted());
    let type_name = class.type_name(module.name.dotted());
    let mut visits = String::new();
    let mut clears = String::new();
    for field in class.fields.iter().filter(|field| collectable(field)) {
        let _ = writeln!(
            out_slot(&mut visits),
            "    Py_VISIT(self->{});",
            field.member()
        );
        let _ = writeln!(
            out_slot(&mut clears),
            "    Py_CLEAR(self->{});",
            field.member()
        );
    }
    format!(
        "static int {type_name}_traverse({struct_name} *self, visitproc visit, void *arg) {{\n\
         \x20   Py_VISIT(Py_TYPE(self));\n\
         {visits}\
         \x20   Py_VISIT(self->{BY_DICT_MEMBER});\n\
         \x20   return 0;\n}}\n\n\
         static int {type_name}_clear({struct_name} *self) {{\n\
         {clears}\
         \x20   By_ReleaseInstanceDict(&self->{BY_DICT_MEMBER});\n\
         \x20   return 0;\n}}\n\n"
    )
}

/// `tp_dealloc`, `tp_traverse` and `tp_clear` for a class whose fields sit past a
/// base's instance, and for a base such a class stands on
///
/// everything else about inheriting a layout says "supply no slots, the base allocates
/// and frees" — but the base cannot know about storage appended after its own data, so
/// without these three the appended fields simply leak. the traverse and clear are not
/// optional either: a base like `Exception` is a GC type, so ours is, and a field the
/// collector cannot see holds its cycle alive forever.
///
/// a base holding nothing of its own gets the same three with nothing in them: what it
/// is here for is to *be* the rung the appending class chains to, which python's own
/// three cannot be — see [`built_from_a_spec_ahead`]. such a class binds no storage at
/// all, because it declared no region for `By_TypeData` to find
fn emit_appended_storage(module: &ModuleIr, class: &ClassIr) -> String {
    let struct_name = class.struct_name(module.name.dotted());
    let type_name = class.type_name(module.name.dotted());
    // the *declaring* type, not `Py_TYPE(self)`: a python subclass of this class is a
    // different type whose data area is somewhere else again, and the base to chain to
    // is this class's base rather than that subclass's
    let declared = format!("((PyTypeObject *){type_name}_OBJ)");
    // and only where there is a region to reach: a base standing here to carry the three
    // slots declared none, and `By_TypeData` against it would be a pointer past the end
    let bind = |touched: &str| {
        if touched.is_empty() {
            String::new()
        } else {
            format!("    {struct_name} *by_f = ({struct_name} *)By_TypeData(self, {declared});\n")
        }
    };
    let mut out = String::new();

    let mut visits = String::new();
    let mut clears = String::new();
    for field in own_fields(module, class)
        .iter()
        .filter(|field| collectable(field))
    {
        let _ = writeln!(
            out_slot(&mut visits),
            "    Py_VISIT(by_f->{});",
            field.member()
        );
        let _ = writeln!(
            out_slot(&mut clears),
            "    Py_CLEAR(by_f->{});",
            field.member()
        );
    }
    let (bound_visits, bound_clears) = (bind(&visits), bind(&clears));

    // an instance of a heap type counts as a reference to that type, and the collector
    // has to see it or a cycle through the type is never broken. exactly one traverse in
    // the chain reports it, which is `subtype_traverse`'s own rule: the one whose base
    // does not itself carry the link. a base out of this module is not a heap type — the
    // construction refuses one that is — so this traverse is that one; a base *this*
    // module appends to is, and its traverse has already reported it. counting it twice
    // would tell the collector the instance holds two references where it holds one
    let _ = write!(
        out,
        "static int {type_name}_traverse(PyObject *self, visitproc visit, void *arg) {{\n\
         {bound_visits}\
         {visits}\
         \x20   PyTypeObject *by_base = {declared}->tp_base;\n\
         \x20   if (by_base->tp_traverse) {{\n\
         \x20       int by_r = by_base->tp_traverse(self, visit, arg);\n\
         \x20       if (by_r) return by_r;\n\
         \x20   }}\n\
         \x20   if (!(by_base->tp_flags & Py_TPFLAGS_HEAPTYPE)) Py_VISIT(Py_TYPE(self));\n\
         \x20   return 0;\n}}\n\n\
         static int {type_name}_clear(PyObject *self) {{\n\
         {bound_clears}\
         {clears}\
         \x20   PyTypeObject *by_base = {declared}->tp_base;\n\
         \x20   if (by_base->tp_clear) return by_base->tp_clear(self);\n\
         \x20   return 0;\n}}\n\n"
    );

    // the base frees the instance, so this releases only what it cannot see and then
    // chains. the type reference has to be read *before* the base frees the object and
    // dropped after, because the base does not know this type is a heap type
    // every refcounted field, not only the collectable ones — a tagged `int` cannot be
    // in a cycle but it still holds a reference, and `dec_ref` is what knows the
    // difference between releasing one of those and releasing an object
    let mut releases = String::new();
    for field in own_fields(module, class) {
        if let Some(release) = dec_ref(&field.ty, &format!("by_f->{}", field.member())) {
            let _ = writeln!(out_slot(&mut releases), "    {release}");
        }
    }
    let released = if releases.is_empty() {
        String::new()
    } else {
        format!(
            "\x20   {{ {struct_name} *by_f = ({struct_name} *)By_TypeData(self, {declared});\n\
             {releases}\
             \x20   }}\n"
        )
    };
    // the instance comes off the collector's list for the release, because releasing a
    // field can run arbitrary python and a collection walking a half-cleared object would
    // read a field that is already gone — and then goes straight back on, because a
    // collected base takes it off again itself and some of them do it without checking.
    // `OSError`'s deallocator is one of those: handed an object already off the list it
    // unlinks it a second time and corrupts the list, which is a segfault at the first
    // deallocation. `subtype_dealloc` re-tracks in the same place and for the same reason
    //
    // and the type reference is dropped by exactly one rung, the same one the traverse
    // reports it from: a base that is itself a heap type has a deallocator of its own that
    // drops it, and two drops for the one reference free the type underneath everything
    // still using it
    let _ = write!(
        out,
        "static void {type_name}_dealloc(PyObject *self) {{\n\
         \x20   PyTypeObject *by_type = Py_TYPE(self);\n\
         \x20   PyTypeObject *by_base = {declared}->tp_base;\n\
         \x20   int by_tracked = PyType_HasFeature(by_type, Py_TPFLAGS_HAVE_GC);\n\
         \x20   if (by_tracked) PyObject_GC_UnTrack(self);\n\
         {released}\
         \x20   if (by_tracked && PyType_IS_GC(by_base)) PyObject_GC_Track(self);\n\
         \x20   by_base->tp_dealloc(self);\n\
         \x20   if (!(by_base->tp_flags & Py_TPFLAGS_HEAPTYPE)\n\
         \x20       && (by_type->tp_flags & Py_TPFLAGS_HEAPTYPE)) Py_DECREF(by_type);\n}}\n\n"
    );
    out
}

/// the instance struct: a `PyObject` header plus one field per attribute
///
/// a class appending to a base's layout gets the fields *without* a header — it is the
/// area past the base's instance, not an instance itself, and the base owns the header
fn emit_class_struct(module: &ModuleIr, class: &ClassIr) -> String {
    let header = if external_storage(module, class) {
        ""
    } else {
        "    PyObject_HEAD\n"
    };
    // the tag only — the typedef of the same name was written ahead of the tuple
    // structs, and repeating it is not something every C dialect accepts
    let mut out = format!(
        "struct {} {{\n{header}",
        class.struct_name(module.name.dotted())
    );
    // before the fields, so that every class in one chain agrees on where they start:
    // a subclass's struct is a clone of its base's with its own fields after, and a
    // base-typed pointer to a subclass instance reads the base's fields at the base's
    // offsets. reserved rather than declared, because it is python's rule about
    // `__slots__` that decides which of them the word is *reachable* on
    if reserves_dict_word(module, class) {
        let _ = writeln!(out, "    PyObject *{BY_DICT_MEMBER};");
    }
    // where a `return` puts its value. a resumable frame reports finishing by writing
    // here and handing back nothing, so that the slot python asks with — `am_send` —
    // can say what the frame returned without an exception ever being built. it is not
    // one of the frontend's fields because no python code can name it and nothing
    // parks across a suspension in it: it is written once, on the way out
    if class.resume.is_some() {
        out.push_str("    PyObject *by_returned;\n");
    }
    for field in own_fields(module, class) {
        let _ = writeln!(out, "    {} {};", ctype(module, &field.ty), field.member());
        // `tp_alloc` zeroes the instance, so "never written" is the state an object
        // starts in and the constructor has nothing to do
        if field.optional {
            let _ = writeln!(out, "    char {};", field.presence());
        }
    }
    out.push_str("};\n");
    out
}

/// `tp_new` allocates and zeroes, `tp_init` fills every field, `tp_dealloc`
/// releases them. every field is written by `__init__`, which is what makes them
/// *always defined* — no bitfield and no per-read check
fn emit_class_type(module: &ModuleIr, class: &ClassIr) -> String {
    let struct_name = class.struct_name(module.name.dotted());
    let type_name = class.type_name(module.name.dotted());
    let mut out = String::new();

    let keeps_a_dict = instance_dict(module, class);
    if frees_its_instances(module, class) {
        out.push_str(&emit_appended_storage(module, class));
    } else {
        // the pair only exists where the dict does: a class without one asks to be
        // collected for no reason, and a function nothing reaches is a warning the build
        // turns into an error
        if keeps_a_dict {
            out.push_str(&emit_collected_instance(module, class));
        }
        // dealloc releases each refcounted field, then the object
        let _ = writeln!(
            out,
            "static void {type_name}_dealloc({struct_name} *self) {{"
        );
        // a collected instance is on the collector's list until it says otherwise, and
        // a list holding a half-freed object is what the next collection walks. a class
        // without a dict is not a collected type, and asking about an object with no
        // collector header in front of it reads memory that is not there
        if keeps_a_dict {
            out.push_str("    PyObject_GC_UnTrack(self);\n");
        }
        // a finalizer does not run itself: `subtype_dealloc` calls it, and a type that
        // writes its own dealloc has to do the same or the cleanups never happen. a
        // negative answer means the finalizer resurrected the object, and freeing it
        // then would be freeing something live.
        //
        // a subclass writes a dealloc of its own, so it has to make the call for the
        // finalizer it inherited — which is why this asks the whole chain rather than
        // only this class
        if class.resume.is_some() || finalizes(module, class) {
            out.push_str(
                "    if (PyObject_CallFinalizerFromDealloc((PyObject *)self) < 0) return;\n",
            );
        }
        if keeps_a_dict {
            let _ = writeln!(out, "    By_ReleaseInstanceDict(&self->{BY_DICT_MEMBER});");
        }
        // a return nobody asked for still owns its value: a generator dropped between
        // its frame finishing and the finish being read leaves one here
        if class.resume.is_some() {
            out.push_str("    Py_XDECREF(self->by_returned);\n");
        }
        for field in &class.fields {
            if let Some(release) = dec_ref(&field.ty, &format!("self->{}", field.member())) {
                let _ = writeln!(out, "    {release}");
            }
        }
        // a heap type is refcounted, and an instance holds a reference to it
        out.push_str(
            "    PyTypeObject *by_type = Py_TYPE(self);\n\
             \x20   by_type->tp_free((PyObject *)self);\n\
             \x20   if (by_type->tp_flags & Py_TPFLAGS_HEAPTYPE) Py_DECREF(by_type);\n}\n\n",
        );
    }

    // a class with nothing to initialize publishes no `__init__` at all, so that
    // `object.__init__` is what a construction reaches — exactly as in the source
    if !initializes(module, class) {
        out.push_str(&emit_class_members(module, class));
        return out;
    }

    let _ = writeln!(
        out,
        "static int {type_name}_init(PyObject *selfobj, PyObject *args, PyObject *kwds) {{"
    );
    // a class that wrote its own `__init__` is initialized by running it: the
    // fields are whatever it assigns, so nothing else could fill them
    if let Some(init) = class
        .methods
        .iter()
        .find(|method| method.name == "__init__")
    {
        out.push_str(&emit_written_init(module, init));
        out.push_str("    return 0;\n}\n\n");
        out.push_str(&emit_class_members(module, class));
        return out;
    }
    // a generated constructor fills every field the class declares, and where those sit
    // in a chain of appended storage each rung keeps its own in a region of its own — so
    // it binds one pointer per rung rather than the single `self` everything else needs
    let _ = writeln!(out, "    {}", bind_storage_chain(module, class));
    // the fields this constructor *takes*, which for a generated one are the class's own
    // — a `data class` is its annotations, and each becomes a parameter.
    //
    // a class that wrote no `__init__` at all takes none of them. its fields are storage
    // the source never gave a constructor: a `__slots__` declares attributes an instance
    // has room for and nothing fills, and an inherited layout is filled by the base's
    // `__init__`. a parameter per field would be a signature the source never wrote, so
    // this one binds nothing and rejects an argument exactly as `object.__init__` does
    let taken: &[by_ir::function::FieldDecl] = if class.inherited_init {
        &[]
    } else {
        &class.fields
    };
    let names = taken
        .iter()
        .map(|field| c_string(&field.name))
        .collect::<Vec<_>>()
        .join(", ");
    let required = taken
        .iter()
        .map(|field| i32::from(field.default.is_none()).to_string())
        .collect::<Vec<_>>()
        .join(", ");
    if taken.is_empty() {
        out.push_str("    (void)self;\n");
    }
    let _ = writeln!(
        out,
        "    static const char *const by_names[] = {{ {names} }};\n\
         \x20   static const unsigned char by_required[] = {{ {required} }};\n\
         \x20   PyObject *by_bound[{}];\n\
         \x20   if (By_BindInit(args, kwds, by_names, {}, by_required, 0, 0, by_bound, 0, 0, {}, {}) < 0) return -1;",
        taken.len().max(1),
        taken.len(),
        // a written `__init__` took the branch above, so a class taking nothing here has
        // no constructor of its own and `object.__init__` is what rejects the call —
        // python names the *class* in that message, not a method it does not have
        c_string(&if class.inherited_init {
            class.name.clone()
        } else {
            format!("{}.__init__", class.name)
        }),
        i32::from(class.inherited_init)
    );
    for (index, field) in taken.iter().enumerate() {
        let _ = writeln!(out, "    {{ {} by_v;", ctype(module, &field.ty));
        match &field.default {
            Some(default) => {
                let _ = writeln!(out, "      if (by_bound[{index}] != NULL) {{");
                let _ = writeln!(
                    out,
                    "          by_v = {};",
                    unbox_checked(module, &field.ty, &format!("by_bound[{index}]"))
                );
                let _ = writeln!(out, "      }} else {{");
                let _ = writeln!(
                    out,
                    "          by_v = {};",
                    default_expr(&field.ty, default)
                );
                let _ = writeln!(out, "      }}");
            }
            None => {
                let _ = writeln!(
                    out,
                    "      by_v = {};",
                    unbox_checked(module, &field.ty, &format!("by_bound[{index}]"))
                );
            }
        }
        let _ = writeln!(
            out,
            "      if ({}) return -1;",
            error_check(&field.ty, "by_v")
        );
        let storage = storage_name(module, class, &field.name);
        let release = release_old(field, &storage);
        if !release.is_empty() {
            let _ = writeln!(out, "      {release}");
        }
        let _ = writeln!(out, "      {storage}->{} = by_v;", field.member());
        // the byte beside an optional field is what a later read and the deallocation
        // both ask, so filling one here has to answer them
        if field.optional {
            let _ = writeln!(out, "      {storage}->{} = 1;", field.presence());
        }
        // a fresh instance has published nothing, but `o.__init__(...)` runs this a
        // second time over an object that may have handed a mapping out
        out.push_str(&publish_field(
            module,
            class,
            "self",
            field,
            &format!("{storage}->{}", field.member()),
        ));
        out.push_str("    }\n");
    }
    out.push_str("    return 0;\n}\n\n");
    out.push_str(&emit_class_members(module, class));
    out
}

/// what a boundary binds: the parameters it fills from the call, which are neither
/// the receiver it is handed separately nor the `*args`/`**kwargs` it *builds*
///
/// [`Function::posonly`] and [`Function::kwonly`] count from the function's own first
/// parameter, so a boundary that does not bind the receiver shifts them into the index
/// space of the names it declares — leaving them unshifted makes an ordinary parameter
/// unreachable by name
struct Bound<'a> {
    params: &'a [RegisterDecl],
    /// whether the caller must supply each of `params`, in the same order
    ///
    /// a keyword-only parameter may be required *after* one with a default, so this is
    /// a mask rather than a count. a computed default leaves the parameter optional: it
    /// is the value that is unavailable, not the arity
    required: Vec<bool>,
    posonly: usize,
    kwonly: usize,
    vararg: bool,
    kwarg: bool,
}

impl<'a> Bound<'a> {
    fn of(function: &'a Function, receiver: bool) -> Self {
        let skip = usize::from(receiver);
        let packed = usize::from(function.vararg) + usize::from(function.kwarg);
        let named = function.param_count.saturating_sub(packed);
        let params = function.params().get(skip..named).unwrap_or_default();
        let required = (skip..skip + params.len())
            .map(|position| {
                !(function.defaults.get(position).is_some_and(Option::is_some)
                    || function.computed_defaults.contains(&position))
            })
            .collect();
        Self {
            params,
            required,
            posonly: function.posonly.saturating_sub(skip),
            kwonly: function.kwonly,
            vararg: function.vararg,
            kwarg: function.kwarg,
        }
    }

    /// the `by_names`, `by_required` and `by_bound` the binding reads
    ///
    /// c has no zero-length array, so a boundary that binds nothing still declares one
    /// of each — the count it passes is what says they hold nothing
    fn declare(&self) -> String {
        if self.params.is_empty() {
            return "    static const char *const by_names[1] = { \"\" };\n\
                    \x20   static const unsigned char by_required[1] = { 0 };\n\
                    \x20   PyObject *by_bound[1];\n"
                .to_string();
        }
        let names = self
            .params
            .iter()
            .map(|decl| c_string(decl.name.as_deref().unwrap_or("")))
            .collect::<Vec<_>>()
            .join(", ");
        let required = self
            .required
            .iter()
            .map(|required| i32::from(*required).to_string())
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "    static const char *const by_names[] = {{ {names} }};\n\
             \x20   static const unsigned char by_required[] = {{ {required} }};\n\
             \x20   PyObject *by_bound[{}];\n",
            self.params.len()
        )
    }
}

/// `tp_init` for a class that wrote its own `__init__`: bind the arguments the
/// method declares and run it
///
/// the same binding the method's own wrapper does, against the same names — the
/// only difference is where the arguments arrive from, since a type slot is handed
/// a tuple and a dict rather than a vector
fn emit_written_init(module: &ModuleIr, init: &Function) -> String {
    let mut out = String::new();
    let fname = c_string(&format!("{}.__init__", init.owner.as_deref().unwrap_or("")));
    // the receiver is the object being initialized, not an argument
    let bound = Bound::of(init, true);
    let explicit = bound.params.len();
    out.push_str(&bound.declare());
    let _ = writeln!(
        out,
        "    if (By_BindInit(args, kwds, by_names, {explicit}, by_required, {}, {}, by_bound, {}, {}, {fname}, 0) < 0) return -1;",
        bound.posonly,
        bound.kwonly,
        i32::from(bound.vararg),
        i32::from(bound.kwarg),
    );
    // the same boundary the method's own wrapper has, and the same hand-over: a slot
    // is handed a tuple and a dict rather than a vector, which is all that differs
    let tests = defer_tests(init, true);
    if !tests.is_empty() {
        let _ = writeln!(
            out,
            "    if ({}) return By_InitInterpreted({}, {fname}, selfobj, args, kwds);",
            tests.join(" || "),
            init.interpreted_symbol(module.name.dotted())
        );
    }
    let params = init.params().get(1..).unwrap_or_default();
    for (index, decl) in params.iter().enumerate() {
        let _ = writeln!(
            out,
            "    {} a{index} = {};",
            ctype(module, &decl.ty),
            decl.ty.undefined()
        );
    }
    for (index, decl) in params.iter().enumerate() {
        // the packed parameters are *built* from what is left over, not bound
        if index >= explicit {
            let call = if bound.vararg && index == explicit {
                // the surplus starts where the run a caller may fill positionally
                // ends, which keyword-only parameters move back
                format!("By_PackInitArgs(args, {})", explicit - bound.kwonly)
            } else {
                format!(
                    "By_PackInitKwargs(kwds, by_names, {explicit}, {})",
                    bound.posonly
                )
            };
            let _ = writeln!(out, "    a{index} = {call};");
            let _ = writeln!(out, "    if (a{index} == NULL) goto by_init_error;");
            continue;
        }
        let slot = format!("by_bound[{index}]");
        let unbox = unbox_checked(module, &decl.ty, &slot);
        match init.defaults.get(index + 1).and_then(Option::as_ref) {
            Some(default) => {
                let _ = writeln!(out, "    if ({slot} != NULL) {{");
                let _ = writeln!(out, "        a{index} = {unbox};");
                let _ = writeln!(out, "    }} else {{");
                let _ = writeln!(
                    out,
                    "        a{index} = {};",
                    default_expr(&decl.ty, default)
                );
                let _ = writeln!(out, "    }}");
            }
            None => {
                let _ = writeln!(out, "    a{index} = {unbox};");
            }
        }
        let _ = writeln!(
            out,
            "    if ({}) goto by_init_error;",
            error_check(&decl.ty, &format!("a{index}"))
        );
    }
    let mut arguments = String::new();
    for index in 0..params.len() {
        let _ = write!(arguments, ", a{index}");
    }
    // the slot hands over a `PyObject *`, and the receiver parameter is whatever a
    // register of the class's type is — a struct pointer, where the class owns its layout
    let receiver = init
        .params()
        .first()
        .map_or_else(|| "PyObject *".to_string(), |decl| ctype(module, &decl.ty));
    let _ = writeln!(
        out,
        "    {{ {} by_result = {}(({receiver})selfobj{arguments});",
        ctype(module, &init.ret),
        init.native_symbol(module.name.dotted())
    );
    if init.convention.can_fail() {
        let _ = writeln!(
            out,
            "      if ({}) goto by_init_error;",
            error_check(&init.ret, "by_result")
        );
    }
    if let Some(release) = dec_ref(&init.ret, "by_result") {
        let _ = writeln!(out, "      {release}");
    }
    out.push_str("    }\n");
    for (index, decl) in params.iter().enumerate() {
        if let Some(release) = dec_ref(&decl.ty, &format!("a{index}")) {
            let _ = writeln!(out, "    {release}");
        }
    }
    out.push_str("    goto by_init_done;\nby_init_error: ;\n");
    for (index, decl) in params.iter().enumerate() {
        if let Some(release) = dec_ref(&decl.ty, &format!("a{index}")) {
            let _ = writeln!(out, "    {release}");
        }
    }
    out.push_str("    return -1;\nby_init_done: ;\n");
    out
}

/// `Box[int]` on a class this module emits
///
/// a class written in python answers a subscript through the `__class_getitem__`
/// the `class Box[T]:` syntax gives it. a type built in C has to say so itself,
/// and the alias it hands back is the same one `list[int]` produces
fn emit_class_getitem(module: &ModuleIr, class: &ClassIr, type_name: &str) -> String {
    if !class.generic {
        return String::new();
    }
    let _ = module;
    format!(
        "static PyObject *{type_name}_class_getitem(PyObject *cls, PyObject *item) {{\n\
         \x20   return Py_GenericAlias(cls, item);\n}}\n"
    )
}

/// releasing what a field held before a setter overwrites it
///
/// an optional one may hold nothing at all, and the byte beside it is what says so —
/// releasing a value that was never written would be releasing whatever `tp_alloc`
/// left there
fn release_old(field: &by_ir::function::FieldDecl, storage: &str) -> String {
    let Some(release) = dec_ref(&field.ty, &format!("{storage}->{}", field.member())) else {
        return String::new();
    };
    if field.optional {
        format!("if ({storage}->{}) {{ {release} }}", field.presence())
    } else {
        release
    }
}

/// the assignment that records an optional field as written
fn mark_present(field: &by_ir::function::FieldDecl, storage: &str) -> String {
    if field.optional {
        format!("\x20   {storage}->{} = 1;\n", field.presence())
    } else {
        String::new()
    }
}

/// `del obj.field`, which python reaches through the setter with no value
///
/// the presence byte is what makes this expressible at all: clearing it puts the field
/// back into the state `tp_alloc` left it in, which a read already answers with
/// `AttributeError` and a later write already sets again. so the delete is the write
/// read backwards, and a field with no byte — one `__init__` assigns on every path and
/// nothing deletes — has no absent state to return to and keeps refusing.
///
/// the order is the one `PyObject_GenericSetAttr` uses on a slot, and it is not
/// cosmetic: releasing the old value can run a `__del__` that reaches back into this
/// same instance, so the field has to already read as gone by the time anything else
/// can look at it. zeroing the member rather than only the byte is what keeps the
/// deallocation safe — it releases every field unconditionally, relying on the zero
/// `tp_alloc` left being harmless for each representation, and a member left dangling
/// behind a cleared byte would be released a second time there
fn delete_field(
    module: &ModuleIr,
    owner: &ClassIr,
    field: &by_ir::function::FieldDecl,
    storage: &str,
) -> String {
    if !field.optional {
        return "\x20   if (by_value == NULL) {\n\
                \x20       PyErr_SetString(PyExc_AttributeError, \"cannot delete an attribute\");\n\
                \x20       return -1;\n\
                \x20   }\n"
            .to_string();
    }
    let member = field.member();
    let release = dec_ref(&field.ty, "by_old")
        .map_or_else(String::new, |release| format!("\x20       {release}\n"));
    format!(
        "\x20   if (by_value == NULL) {{\n\
         \x20       if (!{storage}->{presence}) {{ PyErr_Format(PyExc_AttributeError, \
         \"'%s' object has no attribute '%s'\", By_TypeName(selfobj), {name}); return -1; }}\n\
         \x20       {ty} by_old = {storage}->{member};\n\
         \x20       memset(&{storage}->{member}, 0, sizeof({storage}->{member}));\n\
         \x20       {storage}->{presence} = 0;\n\
         {unpublish}\
         {release}\
         \x20       return 0;\n\
         \x20   }}\n",
        presence = field.presence(),
        name = c_string(&field.name),
        ty = ctype(module, &field.ty),
        // before the release, for the reason the order above is what it is: a `__del__`
        // the release runs can read this instance, and it must not find the mapping
        // still naming an attribute the object has already given up
        unpublish = if reserves_dict_word(module, owner) {
            format!(
                "\x20       if (BY_UNLIKELY(By_HasPublishedDict({storage}->{BY_DICT_MEMBER})))\n\
                 \x20           By_UnpublishedField({storage}->{BY_DICT_MEMBER}, {});\n",
                c_string(&field.name)
            )
        } else {
            String::new()
        },
    )
}

/// the C identifier the two halves of a property are reached through
fn property_symbol(type_name: &str, name: &str) -> String {
    format!("{type_name}_prop_{name}")
}

/// the three halves of a property, each as the C identifier its `PyMethodDef` goes under
/// and the body the class holds for it
///
/// a half the class holds no body for has to read as absent at both sites — where the
/// definition is emitted and where the property is published — or the second would name a
/// symbol the first never wrote. so both read the pair off this one walk
fn property_halves<'a>(
    class: &'a ClassIr,
    property: &'a PropertyIr,
) -> impl Iterator<Item = (&'static str, Option<&'a Function>)> {
    [
        ("get", &property.getter),
        ("set", &property.setter),
        ("del", &property.deleter),
    ]
    .into_iter()
    .map(|(half, written)| {
        let body = written
            .as_deref()
            .and_then(|name| class.methods.iter().find(|method| method.name == name));
        (half, body)
    })
}

/// the getters, setters, slot table and type spec python sees
fn emit_class_members(module: &ModuleIr, class: &ClassIr) -> String {
    let struct_name = class.struct_name(module.name.dotted());
    let type_name = class.type_name(module.name.dotted());
    let mut out = String::new();

    // the class-level value a field falls back to, held for as long as the module is. it
    // stands here because a subclass reads the cell of whichever class wrote the value,
    // and a base is always emitted before the classes that extend it
    for field in &class.fields {
        if let Some((cell, true)) = field_default(module, class, field) {
            let _ = writeln!(out, "static PyObject *{cell} = NULL;");
        }
    }

    // getters and setters, so python sees ordinary attributes
    for field in &class.fields {
        // python's own answer for an attribute the instance never got, and the
        // getter is where a caller from python meets it
        let absent = if field.optional {
            format!(
                "\x20   if (!self->{}) {{ PyErr_Format(PyExc_AttributeError, \
                 \"'%s' object has no attribute '%s'\", By_TypeName(selfobj), {}); return NULL; }}\n",
                field.presence(),
                c_string(&field.name)
            )
        } else {
            String::new()
        };
        let _ = writeln!(
            out,
            "static PyObject *{type_name}_get_{}(PyObject *selfobj, void *closure) {{\n                 (void)closure;\n                 {}\n{absent}                 return {};\n}}",
            field.name,
            bind_self(module, field_owner(module, class, &field.name), "selfobj"),
            box_borrowed(&field.ty, &format!("self->{}", field.member()))
        );
    }
    // whether an instance has an answer of its own, for the descriptor a defaulted field
    // publishes to ask. it reaches the byte exactly as the getter beside it does, which is
    // the whole point of emitting it: a class appending its storage past an outside base
    // keeps that storage somewhere the instance pointer does not begin
    for field in &class.fields {
        if field_default(module, class, field).is_none() {
            continue;
        }
        let _ = writeln!(
            out,
            "static int {type_name}_has_{}(PyObject *selfobj) {{\n\
             \x20   {}\n\
             \x20   return self->{} != 0;\n}}",
            field.name,
            bind_self(module, field_owner(module, class, &field.name), "selfobj"),
            field.presence()
        );
    }
    // a mutable field needs a setter, and the setter has to check its value for
    // the same reason the constructor does: an unboxed field cannot hold the
    // wrong representation
    for field in &class.fields {
        if !class.writable() {
            continue;
        }
        let _ = writeln!(
            out,
            "static int {type_name}_set_{}(PyObject *selfobj, PyObject *by_value, void *closure) {{\n\
             \x20   (void)closure;\n\
             \x20   {}\n\
             {}\
             \x20   {} by_v = {};\n\
             \x20   if ({}) return -1;\n\
             \x20   {}\n\
             \x20   self->{} = by_v;\n\
             {}\
             {}\
             \x20   return 0;\n}}\n",
            field.name,
            bind_self(module, field_owner(module, class, &field.name), "selfobj"),
            delete_field(
                module,
                field_owner(module, class, &field.name),
                field,
                "self"
            ),
            ctype(module, &field.ty),
            unbox_checked(module, &field.ty, "by_value"),
            error_check(&field.ty, "by_v"),
            release_old(field, "self"),
            field.member(),
            mark_present(field, "self"),
            publish_field(
                module,
                field_owner(module, class, &field.name),
                "self",
                field,
                &format!("self->{}", field.member())
            )
        );
    }
    // the halves of a `@property`, as the two or three callables python builds one
    // `property` out of at module init. each is reached through its own wrapper rather
    // than its native entry, so the arguments are bound exactly as a call through the
    // name would have bound them
    for property in &class.properties {
        let symbol = property_symbol(&type_name, &property.name);
        for (half, body) in property_halves(class, property) {
            let Some(function) = body else { continue };
            // the *property's* name, not the half's: this def is what `__name__` and
            // `__qualname__` are read off, and both halves were written under the one
            // name the class publishes them under
            let _ = writeln!(
                out,
                "static PyMethodDef {symbol}_{half}_def =\n\
                 \x20   {{{}, (PyCFunction)(void(*)(void)){}, METH_FASTCALL | METH_KEYWORDS, NULL}};",
                c_string(&property.name),
                function.wrapper_symbol(module.name.dotted())
            );
        }
    }
    // the layout names an instance's `__dict__` has to answer with, and how to ask each
    // whether the instance has one of its own. a field standing beside a class-level value
    // carries its own presence predicate, because reading such an attribute answers with
    // the *class's* value where the instance never wrote one and python's `__dict__` names
    // no class attribute; every other field reports its absence by raising
    if publishes_a_dict_view(module, class) {
        let _ = writeln!(
            out,
            "static const By_DictField {type_name}_dictfields[] = {{"
        );
        for field in &class.fields {
            let present = if field_default(module, class, field).is_some() {
                format!("{type_name}_has_{}", field.name)
            } else {
                "NULL".to_string()
            };
            let _ = writeln!(out, "    {{{}, {present}}},", c_string(&field.name));
        }
        let _ = write!(
            out,
            "    {{NULL, NULL}}\n}};\n\
             static PyObject *{type_name}_get___dict__(PyObject *selfobj, void *closure) {{\n\
             \x20   (void)closure;\n\
             \x20   return By_InstanceDict(selfobj, {type_name}_dictfields);\n}}\n\
             static int {type_name}_set___dict__(PyObject *selfobj, PyObject *by_value, void *closure) {{\n\
             \x20   (void)closure;\n\
             \x20   return By_InstanceDictReplace(selfobj, {type_name}_dictfields, by_value);\n}}\n"
        );
        // python's own `__getstate__` reads the dict word directly, which on an emitted
        // instance names the extra attributes and none of the class's own — so `copy` and
        // `pickle` would be handed half an object's state, quietly
        if publishes_a_state_method(module, class) {
            let _ = write!(
                out,
                "static PyObject *{type_name}_getstate(PyObject *selfobj, PyObject *by_unused) {{\n\
                 \x20   (void)by_unused;\n\
                 \x20   return By_InstanceState(selfobj, {type_name}_dictfields);\n}}\n"
            );
        }
    }
    let _ = writeln!(out, "static PyGetSetDef {type_name}_getset[] = {{");
    for field in &class.fields {
        // a defaulted field's entry is a descriptor of ours, written into the type's dict
        // at init over whatever stands there. an entry here as well would be replaced by
        // it and change nothing — but only while the two agree about which fields are
        // defaulted, so both ask the one question rather than two that could drift.
        // leaving it out is also what makes an install that never happened *loud*: the
        // attribute is then missing outright, rather than quietly answering a read off the
        // class with a descriptor that hands back itself
        if field_default(module, class, field).is_some() {
            continue;
        }
        let setter = if !class.writable() {
            "NULL".to_string()
        } else {
            format!("{type_name}_set_{}", field.name)
        };
        let _ = writeln!(
            out,
            "    {{\"{}\", {type_name}_get_{}, {setter}, NULL, NULL}},",
            field.name, field.name
        );
    }
    // a class whose whole state is the dict answers `__dict__` with the dict itself,
    // which is exactly what python answers with. a class with fields of its own keeps
    // them in its layout, where that dict names none of them — so it answers with a view
    // over both halves instead. handing back the dict there would be an *empty* mapping
    // where the interpreted class gives a full one: quiet, and wrong
    if instance_dict(module, class) {
        if class.fields.is_empty() {
            out.push_str(
                "    {\"__dict__\", PyObject_GenericGetDict, PyObject_GenericSetDict, NULL, NULL},\n",
            );
        } else {
            let _ = writeln!(
                out,
                "    {{\"__dict__\", {type_name}_get___dict__, {type_name}_set___dict__, NULL, NULL}},"
            );
        }
    }
    out.push_str("    {NULL, NULL, NULL, NULL, NULL}\n};\n\n");

    // `__dictoffset__` is not an attribute the table binds: `PyType_FromSpec` lifts it
    // out and writes it into `tp_dictoffset`, which is the only way a type built from a
    // spec can say where its instances keep a dict
    if instance_dict(module, class) {
        let _ = write!(
            out,
            "static PyMemberDef {type_name}_members[] = {{\n\
             \x20   {{\"__dictoffset__\", BY_DICT_OFFSET_MEMBER,\n\
             \x20    offsetof({struct_name}, {BY_DICT_MEMBER}), BY_DICT_OFFSET_FLAGS}},\n\
             \x20   {{NULL, 0, 0, 0}}\n}};\n\n"
        );
    }

    // the names an instance of this class keeps in its layout, published so that an
    // instance the module body built can be moved onto this type — `By_MovedInstance`
    // says what the move is and why it needs them
    if let Some(symbol) = instance_layout_symbol(module, class) {
        let _ = writeln!(out, "static const By_Field {symbol}[] = {{");
        for field in &class.fields {
            let _ = writeln!(
                out,
                "    {{{}, {}}},",
                c_string(&field.name),
                i32::from(field.optional)
            );
        }
        out.push_str("    {NULL, 0}\n};\n\n");
    }

    // the method table, using each method's python wrapper
    // `send` stores the value the `yield` expression evaluates to, then resumes.
    // `close` marks the machine exhausted — with `yield` inside `try` declined there
    // is no handler to run first
    if let Some(resume) = &class.resume {
        let frame = frame_kind(resume.surface);
        // the *native* entry point, not the wrapper: `tp_iternext` already has the
        // receiver, and the wrapper's argument binding is pure overhead on what is a
        // generator's hottest path
        let symbol = class
            .methods
            .iter()
            .find(|method| method.name == resume.method)
            .map(|method| method.native_symbol(module.name.dotted()))
            .unwrap_or_default();
        let _ = writeln!(
            out,
            "static PyObject *{type_name}_send(PyObject *selfobj, PyObject *const *args, Py_ssize_t nargs) {{\n\
             \x20   if (nargs != 1) {{\n\
             \x20       PyErr_SetString(PyExc_TypeError, \"send() takes exactly one argument\");\n\
             \x20       return NULL;\n\
             \x20   }}\n\
             \x20   {struct_name} *self = ({struct_name} *)selfobj;\n\
             \x20   return By_StepGenerator(selfobj, &self->{sent}, &self->by_returned,\n\
             \x20                           &self->{state}, {frame}, args[0],\n\
             \x20                           (PyObject *(*)(PyObject *)){symbol});\n}}",
            sent = mangle_member(crate::GENERATOR_SENT),
            state = mangle_member(crate::GENERATOR_STATE)
        );
        // `close` throws `GeneratorExit` in, which runs every enclosing `finally`, then
        // marks the machine exhausted whatever came back
        let _ = writeln!(
            out,
            "static PyObject *{type_name}_close(PyObject *selfobj, PyObject *const *args, Py_ssize_t nargs) {{\n\
             \x20   (void)args; (void)nargs;\n\
             \x20   {struct_name} *self = ({struct_name} *)selfobj;\n\
             \x20   int by_r = By_CloseGenerator(selfobj, &self->{sent}, &self->{thrown},\n\
             \x20                                &self->by_returned, &self->{state}, {frame},\n\
             \x20                                (PyObject *(*)(PyObject *)){symbol});\n\
             \x20   By_FinishGenerator(&self->{state});\n\
             \x20   if (by_r < 0) return NULL;\n\
             \x20   Py_RETURN_NONE;\n}}",
            sent = mangle_member(crate::GENERATOR_SENT),
            thrown = mangle_member(crate::GENERATOR_THROWN),
            state = mangle_member(crate::GENERATOR_STATE)
        );
        // `throw` resumes *by raising*, at the suspension point — so a `yield` inside
        // `try` enters its own handler, and `close()` can run a `finally`
        let _ = writeln!(
            out,
            "static PyObject *{type_name}_throw(PyObject *selfobj, PyObject *const *args, Py_ssize_t nargs) {{\n\
             \x20   if (nargs < 1) {{\n\
             \x20       PyErr_SetString(PyExc_TypeError, \"throw() takes at least one argument\");\n\
             \x20       return NULL;\n\
             \x20   }}\n\
             \x20   return By_ThrowInto(selfobj, &(({struct_name} *)selfobj)->{sent},\n\
             \x20                       &(({struct_name} *)selfobj)->{thrown},\n\
             \x20                       &(({struct_name} *)selfobj)->by_returned,\n\
             \x20                       &(({struct_name} *)selfobj)->{state}, {frame}, args[0],\n\
             \x20                       (PyObject *(*)(PyObject *)){symbol});\n}}",
            sent = mangle_member(crate::GENERATOR_SENT),
            thrown = mangle_member(crate::GENERATOR_THROWN),
            state = mangle_member(crate::GENERATOR_STATE)
        );
    }

    out.push_str(&emit_class_getitem(module, class, &type_name));
    // an abandoned generator still has to run its cleanups: python finalizes one
    // by closing it, which resumes the frame *by raising* at the suspension and so
    // runs every `finally` and every `__exit__` the body was inside. without this a
    // context manager the frame never left would simply never be exited
    if class.resume.is_some() {
        let _ = writeln!(
            out,
            "static void {type_name}_finalize(PyObject *selfobj) {{\n\
             \x20   /* only a *suspended* frame has anything to unwind: state 0 never\n\
             \x20    * started and -1 already finished, and resuming either would run\n\
             \x20    * the body a second time */\n\
             \x20   if (By_ShortValue((({struct_name} *)selfobj)->{state}) <= 0) return;\n\
             \x20   PyObject *by_type, *by_value, *by_tb;\n\
             \x20   PyErr_Fetch(&by_type, &by_value, &by_tb);\n\
             \x20   PyObject *by_r = {type_name}_close(selfobj, NULL, 0);\n\
             \x20   if (by_r == NULL) PyErr_WriteUnraisable(selfobj);\n\
             \x20   else Py_DECREF(by_r);\n\
             \x20   PyErr_Restore(by_type, by_value, by_tb);\n}}",
            state = mangle_member(crate::GENERATOR_STATE)
        );
    }
    // the awaitable half is emitted below, with the rest of the async surface, so
    // the table that names it needs the prototype first
    if class
        .resume
        .as_ref()
        .is_some_and(|resume| resume.surface == Surface::AsyncGenerator)
    {
        let _ = writeln!(
            out,
            "static PyObject *{type_name}_aclose(PyObject *, PyObject *const *, Py_ssize_t);\n\
             static PyObject *{type_name}_do_asend(PyObject *, PyObject *const *, Py_ssize_t);\n\
             static PyObject *{type_name}_do_athrow(PyObject *, PyObject *const *, Py_ssize_t);"
        );
    }
    let _ = writeln!(out, "static PyMethodDef {type_name}_methods[] = {{");
    for entry in synthetic_table_entries(class, &type_name) {
        let _ = writeln!(out, "{entry}");
    }
    for method in class.table_methods() {
        // `METH_STATIC` and `METH_CLASS` are masked off before the calling convention
        // is read, so either combines with the fastcall the wrapper is written for.
        // what they change is the descriptor the type publishes — a `staticmethod` or a
        // `classmethod_descriptor` rather than a plain `method_descriptor`
        let _ = writeln!(
            out,
            "    {{\"{}\", (PyCFunction)(void(*)(void)){}, METH_FASTCALL | METH_KEYWORDS{}, {}}},",
            method.name,
            method.wrapper_symbol(module.name.dotted()),
            method
                .binding
                .method_flag()
                .map(|flag| format!(" | {flag}"))
                .unwrap_or_default(),
            method_doc(method)
        );
    }
    // after the class's own, so that nothing counting entries in the table moves —
    // `MakeClosure` takes the address of one
    if publishes_a_state_method(module, class) {
        let _ = writeln!(
            out,
            "    {{\"__getstate__\", (PyCFunction){type_name}_getstate, METH_NOARGS, NULL}},"
        );
    }
    out.push_str("    {NULL, NULL, 0, NULL}\n};\n\n");

    // `__new__` is left out of the table above and given a definition of its own, which
    // module init binds onto the finished type — see `By_PublishNew` for why the slot
    // cannot come from the spec
    if let Some(new) = publishes_new(class) {
        let _ = writeln!(
            out,
            "static PyMethodDef {type_name}_new_def =\n\
             \x20   {{\"__new__\", (PyCFunction)(void(*)(void)){}, METH_FASTCALL | METH_KEYWORDS, NULL}};\n",
            new.wrapper_symbol(module.name.dotted())
        );
    }

    // a generator's state object *is* the iterator: `tp_iternext` drives `$resume`,
    // which returns the next yielded value or raises `StopIteration`
    let iterator = match &class.resume {
        None => String::new(),
        Some(resume) => {
            let frame = frame_kind(resume.surface);
            let symbol = class
                .methods
                .iter()
                .find(|method| method.name == resume.method)
                .map(|method| method.native_symbol(module.name.dotted()))
                .unwrap_or_default();
            let _ = writeln!(
                out,
                "static PyObject *{type_name}_iternext(PyObject *self) {{\n\
                 \x20   return By_StepGenerator(self, &(({struct_name} *)self)->{sent},\n\
                 \x20                           &(({struct_name} *)self)->by_returned,\n\
                 \x20                           &(({struct_name} *)self)->{state}, {frame}, Py_None,\n\
                 \x20                           (PyObject *(*)(PyObject *)){symbol});\n}}",
                sent = mangle_member(crate::GENERATOR_SENT),
                state = mangle_member(crate::GENERATOR_STATE)
            );
            // the slot `PyIter_Send` prefers, and the reason a `return` is reported by
            // writing it down rather than by raising: an `await` that completes gets its
            // answer without an exception being built and immediately unpacked again.
            //
            // an async generator's state object deliberately has none — python's own
            // has none either. what a caller sends into one goes through `asend`, whose
            // awaitable is a different object with a different suspension to report
            if resume.surface != Surface::AsyncGenerator {
                let _ = writeln!(
                    out,
                    "static PySendResult {type_name}_send_slot(PyObject *self, PyObject *by_arg,\n\
                     \x20                                     PyObject **by_result) {{\n\
                     \x20   return By_SendGenerator(self, &(({struct_name} *)self)->{sent},\n\
                     \x20                           &(({struct_name} *)self)->by_returned,\n\
                     \x20                           &(({struct_name} *)self)->{state}, {frame},\n\
                     \x20                           (PyObject *(*)(PyObject *)){symbol},\n\
                     \x20                           by_arg, by_result);\n}}",
                    sent = mangle_member(crate::GENERATOR_SENT),
                    state = mangle_member(crate::GENERATOR_STATE)
                );
            }
            // the member the frontend writes the suspension kind into
            let kind_member = class
                .fields
                .iter()
                .find(|field| field.name == "$kind")
                .map(by_ir::FieldDecl::member)
                .unwrap_or_default();
            let dotted = module.name.dotted();
            if resume.surface == Surface::AsyncGenerator {
                // `__anext__` hands back an awaitable rather than an item, because the
                // body may `await` before it reaches its next `yield`. one `resume`
                // serves both, and `$kind` is what tells the two suspensions apart
                let _ = writeln!(
                    out,
                    "typedef struct {{\n\
                     \x20   PyObject_HEAD\n\
                     \x20   {struct_name} *by_gen;\n\
                     \x20   /* 0 anext, 1 aclose, 2 asend, 3 athrow. the value is what the\n\
                     \x20    * last two carry, consumed on the first step so a resumption\n\
                     \x20    * after it behaves like a plain one */\n\
                     \x20   char by_mode;\n\
                     \x20   PyObject *by_value;\n}} {type_name}_asend;\n\
                     static PyTypeObject {type_name}_asend_type;\n\
                     static void {type_name}_asend_dealloc(PyObject *self) {{\n\
                     \x20   Py_XDECREF((PyObject *)(({type_name}_asend *)self)->by_gen);\n\
                     \x20   Py_XDECREF((({type_name}_asend *)self)->by_value);\n\
                     \x20   Py_TYPE(self)->tp_free(self);\n}}\n\
                     static PyObject *{type_name}_asend_await(PyObject *self) {{\n\
                     \x20   return By_NewRef(self);\n}}\n\
                     static PyObject *{type_name}_asend_next(PyObject *self) {{\n\
                     \x20   {type_name}_asend *by_self = ({type_name}_asend *)self;\n\
                     \x20   {struct_name} *by_gen = by_self->by_gen;\n\
                     \x20   if (by_self->by_mode == 1) {{\n\
                     \x20       PyObject *by_done = {type_name}_close((PyObject *)by_gen, NULL, 0);\n\
                     \x20       if (by_done == NULL) return NULL;\n\
                     \x20       PyErr_SetObject(PyExc_StopIteration, by_done);\n\
                     \x20       Py_DECREF(by_done);\n\
                     \x20       return NULL;\n\
                     \x20   }}\n\
                     \x20   PyObject *by_carried = by_self->by_value;\n\
                     \x20   by_self->by_value = NULL;\n\
                     \x20   PyObject *by_step;\n\
                     \x20   /* an `athrow` into a frame that has already finished neither\n\
                     \x20    * resumes it nor re-raises: python ends the await with `None`.\n\
                     \x20    * one that never *started* is the other case, and raises at the\n\
                     \x20    * call site — which is what `By_ThrowInto` does with it */\n\
                     \x20   if (by_carried != NULL && by_self->by_mode == 3\n\
                     \x20       && By_ShortValue(by_gen->{state}) < 0) {{\n\
                     \x20       Py_DECREF(by_carried);\n\
                     \x20       PyErr_SetNone(PyExc_StopIteration);\n\
                     \x20       return NULL;\n\
                     \x20   }}\n\
                     \x20   if (by_carried != NULL && by_self->by_mode == 3) {{\n\
                     \x20       by_step = By_ThrowInto((PyObject *)by_gen, &by_gen->{sent},\n\
                     \x20                              &by_gen->{thrown}, &by_gen->by_returned,\n\
                     \x20                              &by_gen->{state}, {frame}, by_carried,\n\
                     \x20                              (PyObject *(*)(PyObject *)){symbol});\n\
                     \x20       Py_DECREF(by_carried);\n\
                     \x20   }} else {{\n\
                     \x20       /* `__anext__` carries nothing, which is `None` and not\n\
                     \x20        * whatever the last `asend` left standing */\n\
                     \x20       by_step = By_StepGenerator((PyObject *)by_gen, &by_gen->{sent},\n\
                     \x20                                  &by_gen->by_returned, &by_gen->{state},\n\
                     \x20                                  {frame},\n\
                     \x20                                  by_carried != NULL ? by_carried : Py_None,\n\
                     \x20                                  (PyObject *(*)(PyObject *)){symbol});\n\
                     \x20       Py_XDECREF(by_carried);\n\
                     \x20   }}\n\
                     \x20   if (by_step == NULL) return By_EndAsyncIteration();\n\
                     \x20   if (by_gen->{kind} != By_ShortFrom(1)) return by_step;\n\
                     \x20   /* a yield finishes *this* await, carrying the item */\n\
                     \x20   PyErr_SetObject(PyExc_StopIteration, by_step);\n\
                     \x20   Py_DECREF(by_step);\n\
                     \x20   return NULL;\n}}\n\
                     static PyAsyncMethods {type_name}_asend_async = {{\n\
                     \x20   .am_await = {type_name}_asend_await,\n}};\n\
                     static PyTypeObject {type_name}_asend_type = {{\n\
                     \x20   PyVarObject_HEAD_INIT(NULL, 0)\n\
                     \x20   .tp_name = \"{dotted}.{}.ascend\",\n\
                     \x20   .tp_basicsize = sizeof({type_name}_asend),\n\
                     \x20   .tp_dealloc = (destructor){type_name}_asend_dealloc,\n\
                     \x20   .tp_flags = Py_TPFLAGS_DEFAULT,\n\
                     \x20   .tp_as_async = &{type_name}_asend_async,\n\
                     \x20   .tp_iternext = {type_name}_asend_next,\n\
                     \x20   .tp_new = PyType_GenericNew,\n}};\n\
                     static PyObject *{type_name}_aiter(PyObject *self) {{\n\
                     \x20   return By_NewRef(self);\n}}\n\
                     static PyObject *{type_name}_anext(PyObject *self) {{\n\
                     \x20   {type_name}_asend *by_send =\n\
                     \x20       PyObject_New({type_name}_asend, &{type_name}_asend_type);\n\
                     \x20   if (by_send == NULL) return NULL;\n\
                     \x20   by_send->by_gen = ({struct_name} *)By_NewRef(self);\n\
                     \x20   by_send->by_mode = 0;\n\
                     \x20   by_send->by_value = NULL;\n\
                     \x20   return (PyObject *)by_send;\n}}\n\
                     static PyObject *{type_name}_await_step(PyObject *self, char by_mode,\n\
                     \x20                                   PyObject *by_value) {{\n\
                     \x20   {type_name}_asend *by_send =\n\
                     \x20       PyObject_New({type_name}_asend, &{type_name}_asend_type);\n\
                     \x20   if (by_send == NULL) return NULL;\n\
                     \x20   by_send->by_gen = ({struct_name} *)By_NewRef(self);\n\
                     \x20   by_send->by_mode = by_mode;\n\
                     \x20   by_send->by_value = By_NewRef(by_value);\n\
                     \x20   return (PyObject *)by_send;\n}}\n\
                     static PyObject *{type_name}_do_asend(PyObject *self, PyObject *const *args,\n\
                     \x20                              Py_ssize_t nargs) {{\n\
                     \x20   if (nargs != 1) {{\n\
                     \x20       PyErr_SetString(PyExc_TypeError, \"asend() takes exactly one argument\");\n\
                     \x20       return NULL;\n\
                     \x20   }}\n\
                     \x20   /* `asend(None)` *is* `__anext__`: nothing to carry in */\n\
                     \x20   if (args[0] == Py_None) return {type_name}_anext(self);\n\
                     \x20   return {type_name}_await_step(self, 2, args[0]);\n}}\n\
                     static PyObject *{type_name}_do_athrow(PyObject *self, PyObject *const *args,\n\
                     \x20                               Py_ssize_t nargs) {{\n\
                     \x20   if (nargs < 1) {{\n\
                     \x20       PyErr_SetString(PyExc_TypeError, \"athrow() takes at least one argument\");\n\
                     \x20       return NULL;\n\
                     \x20   }}\n\
                     \x20   return {type_name}_await_step(self, 3, args[0]);\n}}\n\
                     static PyObject *{type_name}_aclose(PyObject *self, PyObject *const *args,\n\
                     \x20                               Py_ssize_t nargs) {{\n\
                     \x20   (void)args; (void)nargs;\n\
                     \x20   {type_name}_asend *by_send =\n\
                     \x20       PyObject_New({type_name}_asend, &{type_name}_asend_type);\n\
                     \x20   if (by_send == NULL) return NULL;\n\
                     \x20   by_send->by_gen = ({struct_name} *)By_NewRef(self);\n\
                     \x20   by_send->by_mode = 1;\n\
                     \x20   by_send->by_value = NULL;\n\
                     \x20   return (PyObject *)by_send;\n}}\n\
                     static PyAsyncMethods {type_name}_async = {{\n\
                     \x20   .am_aiter = {type_name}_aiter,\n\
                     \x20   .am_anext = {type_name}_anext,\n}};",
                    class.name,
                    kind = kind_member,
                    sent = mangle_member(crate::GENERATOR_SENT),
                    thrown = mangle_member(crate::GENERATOR_THROWN),
                    state = mangle_member(crate::GENERATOR_STATE),
                );
                format!(
                    "             .tp_as_async = &{type_name}_async,\n             .tp_finalize = {type_name}_finalize,\n"
                )
            } else if resume.surface == Surface::Coroutine {
                // a coroutine is awaitable, not iterable: `__await__` hands back an
                // iterator, and `for x in coro()` has to stay a `TypeError`
                let _ = writeln!(
                    out,
                    "static PyObject *{type_name}_await(PyObject *self) {{\n\
                     \x20   return By_NewRef(self);\n}}\n\
                     static PyAsyncMethods {type_name}_async = {{\n\
                     \x20   .am_await = {type_name}_await,\n\
                     \x20   .am_send = {type_name}_send_slot,\n}};"
                );
                format!(
                    "             .tp_as_async = &{type_name}_async,\n             .tp_iternext = {type_name}_iternext,\n             .tp_finalize = {type_name}_finalize,\n"
                )
            } else {
                // a generator answers the send slot too — a `yield from` reaches it the
                // same way an `await` does. the table exists for that one entry, and the
                // awaitable slots stay empty so a generator is still not awaitable
                let _ = writeln!(
                    out,
                    "static PyAsyncMethods {type_name}_async = {{\n\
                     \x20   .am_send = {type_name}_send_slot,\n}};"
                );
                format!(
                    "             .tp_as_async = &{type_name}_async,\n             .tp_iter = PyObject_SelfIter,\n             .tp_iternext = {type_name}_iternext,\n             .tp_finalize = {type_name}_finalize,\n"
                )
            }
        }
    };

    out.push_str(&emit_dunder_adapters(module, class, &type_name));
    let dunders = dunder_initializers(class, &type_name);

    let dotted = module.name.dotted();
    // a static struct is what a class no name reaches gets: a generator's state or a
    // closure's environment, neither of which anything can ask about
    if !heap_type(module, class) {
        let _ = write!(
            out,
            "static PyTypeObject {type_name} = {{\n             PyVarObject_HEAD_INIT(NULL, 0)\n             .tp_name = \"{dotted}.{}\",\n             .tp_basicsize = sizeof({struct_name}),\n             .tp_itemsize = 0,\n             .tp_dealloc = (destructor){type_name}_dealloc,\n             .tp_flags = Py_TPFLAGS_DEFAULT,\n{iterator}{dunders}             .tp_methods = {type_name}_methods,\n             .tp_getset = {type_name}_getset,\n             .tp_init = {type_name}_init,\n             .tp_new = PyType_GenericNew,\n         }};\n\
",
            class.name
        );
        return out;
    }
    let mut slots = String::new();
    for (id, value) in iterator_slots(class, &type_name)
        .into_iter()
        .chain(dunder_slots(class, &type_name))
    {
        let _ = writeln!(out_slot(&mut slots), "    {{{id}, (void *){value}}},");
    }
    // a *mutable* heap type has already given up the direct method call — python can
    // rebind a method on one — so letting python subclass it costs nothing more, and an
    // emitted class that cannot be subclassed is a difference from the interpreted
    // one for no gain. a sealed one has given up neither, and both of those rest on the
    // same thing: that nothing can rebind a method and nothing can override one
    let basetype = if mutable_type(module, class) {
        " | Py_TPFLAGS_BASETYPE"
    } else {
        " | Py_TPFLAGS_IMMUTABLETYPE"
    };
    // a type that supplies `tp_traverse` has to *be* a collected type, or its instances
    // are allocated under one discipline and freed under another — which crashes at the
    // first deallocation. saying so unconditionally is consistent either way: `tp_alloc`
    // and `tp_free` are both this type's, so they agree with each other whether or not
    // the base happens to be collected
    let collected = if frees_its_instances(module, class) {
        " | Py_TPFLAGS_HAVE_GC"
    } else if instance_dict(module, class) {
        // the dict holds whatever was put in it, so the collector has to be able to walk
        // it — and a managed one is only allowed on a type it can walk. the pair is one
        // macro because below 3.13 there is no dict and so nothing extra to collect
        " | BY_INSTANCE_DICT_FLAGS"
    } else {
        ""
    };
    // a class that publishes no `__init__` supplies no slot for it either, and the
    // base's is what a construction then reaches — which is the source's own answer.
    // it is the same question whichever layout the class has, so it is asked once
    let init = if initializes(module, class) {
        format!("\x20   {{Py_tp_init, (void *){type_name}_init}},\n")
    } else {
        String::new()
    };
    // the base is *not* a slot: a heap type is a runtime pointer, and a slot table
    // is a static initializer. it goes to `PyType_FromSpecWithBases` at module init
    // a class that reaches an external base inherits its whole instance layout: the
    // base allocates and frees, so this type declares no size of its own and supplies
    // none of the three slots that would take that over. it has to be **transitive** —
    // a subclass of a subclass that declared its own size would declare one smaller
    // than its base, which python rejects outright
    let (basicsize, own_slots) = if external_storage(module, class) {
        // PEP 697: a *negative* size asks for that much room past whatever the base
        // allocated. `tp_new` stays the base's — it is the one that knows how big the
        // whole instance is — while the three slots that touch the appended fields are
        // ours, because the base cannot see them
        (
            format!("-(int)sizeof({struct_name})"),
            format!(
                "\x20   {{Py_tp_dealloc, (void *){type_name}_dealloc}},\n\
                 \x20   {{Py_tp_traverse, (void *){type_name}_traverse}},\n\
                 \x20   {{Py_tp_clear, (void *){type_name}_clear}},\n\
                 {init}"
            ),
        )
    } else if inherits_layout(module, class) {
        // the base allocates and frees an instance of exactly its own size, so this
        // class declares neither. it still supplies `tp_init` where it wrote an
        // `__init__`, because a spec does not run the slot fixup a class statement does
        // — the base's `tp_init` would answer instead and the written one would simply
        // never be called
        //
        // and it supplies the three where a class of ours appends its storage past this
        // one's instance: that class chains its deallocation here, and the chain has to
        // go on down rather than turn around. the size stays the base's, because there
        // is still nothing of this class's own in the instance
        let chained = if chains_a_deallocation(module, class) {
            format!(
                "\x20   {{Py_tp_dealloc, (void *){type_name}_dealloc}},\n\
                 \x20   {{Py_tp_traverse, (void *){type_name}_traverse}},\n\
                 \x20   {{Py_tp_clear, (void *){type_name}_clear}},\n"
            )
        } else {
            String::new()
        };
        ("0".to_string(), format!("{chained}{init}"))
    } else {
        // a class with nothing to initialize supplies neither slot, so both are inherited
        // together — `object.__init__` refuses an argument only while `tp_new` is
        // `object`'s too, and filling one of the pair alone would take one silently.
        //
        // a written `__new__` leaves `tp_new` for the assignment module init makes, which
        // is what installs python's own dispatcher — see [`publishes_new`]. the pair is
        // then what the *source* wrote, and `object.__init__` lifts its refusal for the
        // same reason it does there: the class overrode the allocator
        let construction = if init.is_empty() || constructs_through_a_written_new(module, class) {
            init
        } else {
            format!("{init}\x20   {{Py_tp_new, (void *)PyType_GenericNew}},\n")
        };
        // a collected type has to hand the collector both halves, or an instance in a
        // cycle is never reached at all. a class without a dict has nothing extra to
        // reach, so it is not a collected type and has no pair to hand over. the members
        // table is where the type is told the dict's offset — `PyType_Spec` has no field
        // for `tp_dictoffset` and no slot id for it either
        let walked = if instance_dict(module, class) {
            format!(
                "\x20   {{Py_tp_traverse, (void *){type_name}_traverse}},\n\
                 \x20   {{Py_tp_clear, (void *){type_name}_clear}},\n\
                 \x20   {{Py_tp_members, (void *){type_name}_members}},\n"
            )
        } else {
            String::new()
        };
        (
            format!("sizeof({struct_name})"),
            format!(
                "\x20   {{Py_tp_dealloc, (void *){type_name}_dealloc}},\n{walked}{construction}"
            ),
        )
    };
    let _ = write!(
        out,
        "static PyType_Slot {type_name}_slots[] = {{\n\
         {own_slots}\
         \x20   {{Py_tp_methods, (void *){type_name}_methods}},\n\
         \x20   {{Py_tp_getset, (void *){type_name}_getset}},\n\
         {slots}\x20   {{0, NULL}},\n}};\n\
         static PyType_Spec {type_name}_spec = {{\n\
         \x20   \"{dotted}.{}\",\n\
         \x20   {basicsize},\n\
         \x20   0,\n\
         \x20   Py_TPFLAGS_DEFAULT{basetype}{collected},\n\
         \x20   {type_name}_slots,\n}};\n\
\n",
        class.name
    );
    out.push_str(&emit_class_keywords(class, &type_name));
    out
}

/// whether this class or an in-module base of it carries a class decorator
///
/// the base chain, because an instance discipline is not a per-class answer: a subclass
/// allocates and frees instances of a shape its base decided
fn decorated_chain(module: &ModuleIr, class: &ClassIr) -> bool {
    let mut current = class;
    // bounded by the class count, for the reason `inherits_layout` gives
    for _ in 0..=module.classes.len() {
        if !current.decorators.is_empty() {
            return true;
        }
        match current
            .base
            .as_ref()
            .and_then(ClassBase::in_module)
            .and_then(|name| class_named(module, name))
        {
            Some(next) => current = next,
            None => return false,
        }
    }
    false
}

/// whether an emitted instance keeps a `__dict__` of its own
///
/// python gives an instance somewhere to put a name its class never mentioned, and an
/// emitted class **is** its layout — so `o.brand_new = 7`, which the interpreted twin
/// stores, refused. a class has no runtime fallback to decline into, so that refusal is
/// not a decline but an `AttributeError` in the middle of a working program.
///
/// `__slots__` is python's own way of saying an instance's attributes are exactly the
/// declared ones, and a class that says it wants precisely the layout an emitted class
/// has anyway. so it is what decides here: a chain that declares it throughout keeps the
/// bare layout, and anything else takes a dict. that also keeps the *opposite* answer
/// right — a compiled class that accepted what its twin refuses would be a divergence in
/// the other direction.
///
/// a class decorator is a second reason, and an older one. what a decorator hands back
/// is often code it *generated* from what it read — `@dataclass` writes an `__init__`
/// that assigns one attribute per annotation — and that code is ordinary python which
/// assumes an ordinary instance, so on a bare layout every assignment fell off and
/// `E(3)` raised.
///
/// the dict is a word in the class's own struct, at an offset the type names through a
/// `__dictoffset__` member. that is what lets a call site ask whether an instance shadows
/// a method with a single load: a *managed* dict lives in a pre-header, and the only way
/// to reach one is `_PyObject_GetDictPtr`, which lives in libpython and so is a call the
/// C compiler cannot see through — on a loop calling one method that call was 89 per cent
/// of the running time. what the word costs the instance is nothing on balance: it is
/// eight bytes where the pre-header a managed dict needs is sixteen.
///
/// only a class that owns its layout from `object`. one standing on a base outside the
/// module takes that base's answer about a dict, and a spec claiming one anyway would be
/// claiming room the base never allocated — which is how 24 of the `encodings` modules
/// once segfaulted. and only an *exported* one: a closure environment and a generator
/// machine are types nothing can name, so no attribute can be written on one
fn instance_dict(module: &ModuleIr, class: &ClassIr) -> bool {
    class.exported
        && heap_type(module, class)
        && !inherits_layout(module, class)
        && (decorated_chain(module, class) || !slots_declared_throughout(module, class))
}

/// whether this class answers `__dict__` with a view over its layout as well as its dict
///
/// a class keeping a dict and no fields has nothing outside that dict, so python's own
/// answer — the dict — is the whole of the mapping. every other class keeps its attributes
/// in two places at once, and only a view over both is the mapping python promises
fn publishes_a_dict_view(module: &ModuleIr, class: &ClassIr) -> bool {
    instance_dict(module, class) && !class.fields.is_empty()
}

/// whether this class answers `__getstate__` over the whole of an instance's state
///
/// only where the object keeps state outside the dict python's own answer reads, and only
/// where the source wrote no answer of its own
fn publishes_a_state_method(module: &ModuleIr, class: &ClassIr) -> bool {
    publishes_a_dict_view(module, class)
        && !class
            .methods
            .iter()
            .any(|method| method.name == "__getstate__")
}

/// the struct member holding an instance's dict
const BY_DICT_MEMBER: &str = "by_dict";

/// tell a published `__dict__` what a layout field now holds
///
/// an emitted instance's storage is its layout, and this changes nothing about that: a
/// field read is still a load at a compile-time offset with no test in front of it. what
/// this keeps right is a mapping somebody is *holding* — `__dict__` hands one out and
/// installs it as the object's dict word, and a field written afterwards has to reach it
/// or the mapping goes on naming what the field held before. that is a wrong answer
/// nothing marks, which is worse than any refusal.
///
/// the word is NULL for an instance nobody has asked for a `__dict__` or written a stray
/// attribute on, which is nearly every instance, so what a write pays there is one load
/// and a branch that is never taken. nothing at all is added to a field *read*.
///
/// `storage` is a pointer to a struct in the receiver's chain, and every rung of a chain
/// puts the word at the same offset — see [`reserves_dict_word`] — so any of them reaches
/// it. `held` is the member expression the write has just filled, read back rather than
/// reused: an unboxed field keeps a representation of its own, and `x = 3` on a float
/// field is `3.0`
fn publish_field(
    module: &ModuleIr,
    owner: &ClassIr,
    storage: &str,
    field: &by_ir::function::FieldDecl,
    held: &str,
) -> String {
    if !reserves_dict_word(module, owner) {
        return String::new();
    }
    format!(
        "    if (BY_UNLIKELY(By_HasPublishedDict({storage}->{BY_DICT_MEMBER})))\n\
         \x20       By_PublishedField({storage}->{BY_DICT_MEMBER}, {}, {});\n",
        c_string(&field.name),
        box_borrowed(&field.ty, held),
    )
}

/// the table naming what an instance of this class keeps, or `None` for a class whose
/// instances cannot be moved onto the emitted type
///
/// the module body builds objects out of the *interpreted* definitions, because that is
/// what stands under each name while it runs — `logging` writes `Logger.root = root`,
/// `_pydatetime` writes `timedelta.max`. every one of those is then an instance of a
/// class nothing else in the process can reach, and `By_MovedInstance` moves its state
/// onto an instance of the emitted type instead. these are the names it reads.
///
/// the one condition is whether the fields really are the whole of an instance. a class
/// whose layout sits past a base python allocates keeps state in that base's part of the
/// object, which nothing here can read back or write — see [`external_storage`].
///
/// an *immutable* class is deliberately not a second condition, though it publishes no
/// setters and so cannot be filled. it needs no rule of its own: the move goes through the
/// type's own setter, so a frozen class with fields refuses at the first of them, while
/// one with no fields has no state to lose and moves soundly. a rule here would only cost
/// the second case
fn instance_layout_symbol(module: &ModuleIr, class: &ClassIr) -> Option<String> {
    if !class.exported || inherits_layout(module, class) {
        return None;
    }
    Some(format!("{}_fields", class.type_name(module.name.dotted())))
}

/// whether this class's struct carries the dict word, whether or not its own instances
/// can reach one
///
/// python's `__slots__` rule works *down* a chain: `class B: __slots__ = ("a",)` gives
/// its instances no dict, and `class C(B): pass` gives its own one anyway. so a base and
/// a subclass can disagree about whether a dict exists — and they cannot disagree about
/// where the fields start, because a subclass's struct is a clone of its base's and every
/// direct call on a base-typed receiver reads the base's offsets out of a subclass
/// instance. so the word is reserved for the whole chain wherever any rung of it keeps a
/// dict, and only the rungs that keep one name the offset to the type
fn reserves_dict_word(module: &ModuleIr, class: &ClassIr) -> bool {
    module.classes.iter().any(|other| {
        instance_dict(module, other)
            && (other.name == class.name || descends_from(module, other, &class.name))
    })
}

/// whether `class` reaches `ancestor` by following in-module bases
fn descends_from(module: &ModuleIr, class: &ClassIr, ancestor: &str) -> bool {
    let mut current = class;
    // bounded by the class count, for the reason [`inherits_layout`] gives
    for _ in 0..=module.classes.len() {
        let Some(name) = current.base.as_ref().and_then(ClassBase::in_module) else {
            return false;
        };
        if name == ancestor {
            return true;
        }
        match class_named(module, name) {
            Some(next) => current = next,
            None => return false,
        }
    }
    false
}

/// whether every class this one takes its layout from declares `__slots__`
///
/// python gives an instance a `__dict__` unless *every* class contributing to its layout
/// declared one: `class C(B): __slots__ = ("a",)` over a `B` that did not still has a
/// dict, through `B`. so a single class without the declaration anywhere in the chain is
/// what puts a dict on the instance
fn slots_declared_throughout(module: &ModuleIr, class: &ClassIr) -> bool {
    let mut current = class;
    // bounded the way [`inherits_layout`] is: a chain that visits a class twice is a
    // cycle, and one here would hang the compiler rather than fail it
    for _ in 0..=module.classes.len() {
        if !current.declares_slots {
            return false;
        }
        let Some(name) = current.base.as_ref().and_then(ClassBase::in_module) else {
            return true;
        };
        match class_named(module, name) {
            Some(next) => current = next,
            // a base this module does not emit after all: nothing can be said about what
            // it gives an instance, so the class is left as it was before a dict existed
            None => return true,
        }
    }
    true
}

/// whether this class takes its instance layout from a base outside the module
///
/// transitively: an in-module base that itself extends an external one has no layout of
/// its own either, and neither does anything under it
fn inherits_layout(module: &ModuleIr, class: &ClassIr) -> bool {
    let mut current = class;
    // bounded by the class count: a base chain cannot visit one twice without being a
    // cycle, and a cycle here would otherwise hang the compiler rather than fail it
    for _ in 0..=module.classes.len() {
        match current.base.as_ref() {
            None => return false,
            Some(ClassBase::External(_)) => return true,
            Some(ClassBase::InModule(name)) => {
                match module.classes.iter().find(|other| other.name == *name) {
                    Some(next) => current = next,
                    // a base the module does not emit after all: nothing can be assumed
                    // about the layout, so assume it is not ours
                    None => return true,
                }
            }
        }
    }
    true
}

/// whether this class or an in-module base of it writes a `__del__`
///
/// a subclass whose struct begins with its base's still declares a layout of its own,
/// so it writes a dealloc of its own — and that dealloc is the only place the finalizer
/// it inherited could be reached from
fn finalizes(module: &ModuleIr, class: &ClassIr) -> bool {
    let mut current = class;
    // bounded the way [`inherits_layout`] is: a chain that visits a class twice is a
    // cycle, and one here would hang the compiler rather than fail it
    for _ in 0..=module.classes.len() {
        if dunder(current, "__del__").is_some() {
            return true;
        }
        let Some(name) = current.base.as_ref().and_then(ClassBase::in_module) else {
            return false;
        };
        // a base the module does not emit after all has no `__del__` of ours to find
        match class_named(module, name) {
            Some(next) => current = next,
            None => return false,
        }
    }
    false
}

/// whether this type publishes an `__init__` of its own
///
/// a class that writes none has nothing to initialize, and one installed anyway is a
/// member the source never wrote. python finds it through the mro of everything built on
/// this class: `class C(int, Ours)` takes its argument through `int.__new__` and leaves
/// `__init__` to `object`, so a zero-argument one of ours standing between them rejects
/// the call outright. leaving `tp_init` and `tp_new` both unfilled inherits the base's
/// pair, which is the source's own answer — and they go together, because
/// `object.__init__` refuses an argument only while `tp_new` is `object`'s too.
///
/// the fields are no evidence either way: a subclass's begin with its base's, so a class
/// that writes no `__init__` still carries every field the base laid out. one synthesized
/// from *those* would take an argument per inherited field and never run the base's
/// `__init__` at all — the base's is what the mro reaches in the source, and inheriting
/// the slot is what reaches it here.
///
/// a class that is not *mutable* fills them whatever it holds: a static type left with
/// an unfilled `tp_new` cannot be instantiated at all, because `PyType_Ready` does not
/// hand a static type `object`'s. nothing is built on such a class — being a base is
/// what makes one mutable — so no other mro can reach an `__init__` of its, and a
/// sealed type answers the question the same way its static edition did
fn initializes(module: &ModuleIr, class: &ClassIr) -> bool {
    !mutable_type(module, class) || !class.inherited_init
}

/// whether this class keeps its fields *after* a base's instance rather than inside one
///
/// PEP 697: a negative `basicsize` asks for that much room past whatever the base
/// allocated, and `PyObject_GetTypeData` is the only way to reach it. so the object
/// pointer and the field storage are two different addresses — which is the whole
/// difference from a class that owns its layout, where they are the same one
fn external_storage(module: &ModuleIr, class: &ClassIr) -> bool {
    inherits_layout(module, class) && !class.fields.is_empty()
}

/// whether this class's type is a spec that appends storage, which is the one shape with
/// no construction to fall back to
///
/// module init builds these ahead of everything else, where a refusal can still leave the
/// whole module interpreted — see `By_SpecClass`
fn appends_storage_from_a_spec(module: &ModuleIr, class: &ClassIr) -> bool {
    external_storage(module, class)
        && class.keywords.is_empty()
        && !stands_on_an_emitted_base(module, class)
}

/// every class module init builds from a type spec ahead of everything else
///
/// two shapes are in here. the first is a class whose fields sit past a base's instance,
/// which has no other construction at all — see [`appends_storage_from_a_spec`]. the
/// second is a base one of those stands on, whatever that base holds: reaching appended
/// storage takes `tp_dealloc`, `tp_traverse` and `tp_clear` of the appending class's
/// own, and each of the three calls the base's. `subtype_dealloc` — what a `class`
/// statement's type carries — reads the base to chain to from `Py_TYPE(self)` rather
/// than from the type that declared it, so it finds the appending class's back and calls
/// it until the stack runs out. having no fields does not save such a base from that:
/// what breaks the chain is carrying the three slots we emit, and a spec that asks for
/// none of them is handed `subtype_dealloc` just the same.
///
/// the classes built there and the classes the rest of module init must *not* build
/// again have to be the same set, so both ask this rather than re-deriving it
fn built_from_a_spec_ahead(module: &ModuleIr) -> HashSet<&str> {
    let mut ahead: HashSet<&str> = module
        .classes
        .iter()
        .filter(|class| appends_storage_from_a_spec(module, class))
        .map(|class| class.name.as_str())
        .collect();
    // a base is declared before the class standing on it, so one pass back through the
    // module's order settles a chain of any depth
    for class in module.classes.iter().rev() {
        if ahead.contains(class.name.as_str())
            && let Some(base) = base_declared_earlier(module, class)
            && a_spec_can_free(module, base)
        {
            ahead.insert(base.name.as_str());
        }
    }
    ahead
}

/// whether module init could build this class from a type spec that frees its own
/// instances — the one shape of heap base a chain of appended storage can stand on
///
/// the whole chain under it is asked, because each rung frees an instance by calling the
/// one below: a rung python built instead is where the walk back up starts
fn a_spec_can_free(module: &ModuleIr, class: &ClassIr) -> bool {
    let mut current = class;
    // bounded by the class count, for the reason [`inherits_layout`] gives
    for _ in 0..=module.classes.len() {
        if !inherits_layout(module, current)
            || !current.keywords.is_empty()
            || stands_on_an_emitted_base(module, current)
        {
            return false;
        }
        // a base from outside is one python allocates and frees, which is where the
        // chain of deallocators was walking to
        let Some(base) = base_declared_earlier(module, current) else {
            return current
                .base
                .as_ref()
                .is_some_and(|base| base.in_module().is_none());
        };
        current = base;
    }
    false
}

/// the in-module base this class's type is built on, where the module declares it first
///
/// module init builds the two in the order the source declares them, and a spec stands
/// on the finished type of the one below. a class statement cannot name a base declared
/// after it, so this only ever rules out a shape the source could not have written
fn base_declared_earlier<'a>(module: &'a ModuleIr, class: &ClassIr) -> Option<&'a ClassIr> {
    let wanted = class.base.as_ref()?.in_module()?;
    module
        .classes
        .iter()
        .take_while(|candidate| candidate.name != class.name)
        .find(|candidate| candidate.name == wanted)
}

/// whether module init builds this class from a type spec ahead of everything else
fn built_ahead(module: &ModuleIr, class: &ClassIr) -> bool {
    built_from_a_spec_ahead(module).contains(class.name.as_str())
}

/// whether this class holds nothing of its own and still has to free its instances
///
/// a class of ours whose fields sit past this one's instance chains its deallocation
/// here, so this class supplies the same three slots one with storage does — with
/// nothing of its own to release. without them the type is handed `subtype_dealloc`,
/// which sends the appending class's deallocator straight back to itself
fn chains_a_deallocation(module: &ModuleIr, class: &ClassIr) -> bool {
    !external_storage(module, class) && built_ahead(module, class)
}

/// whether this class supplies `tp_dealloc`, `tp_traverse` and `tp_clear` of its own
fn frees_its_instances(module: &ModuleIr, class: &ClassIr) -> bool {
    external_storage(module, class) || chains_a_deallocation(module, class)
}

/// the class this one's type is built on, where that is one this module also builds from
/// a spec ahead of everything else
///
/// such a base is the one heap type a spec can be built on: its `tp_dealloc`,
/// `tp_traverse` and `tp_clear` are ones this module emitted, and each of those reads
/// the base to chain to from the type that *declared* it rather than from
/// `Py_TYPE(self)` — so the chain walks down to the outside base and stops, where
/// `subtype_dealloc` would come straight back. see `By_SpecSubclass`.
///
/// where this class holds storage of its own the two field lists have to line up the way
/// the frontend lays a subclass out: a class's declared fields *begin* with its base's,
/// in the same order, and what it adds of its own is the run past them. that run is the
/// whole of what this class stores — the base keeps its own fields in a region of its
/// own, reached through the type that declared them, so a subclass that stored copies of
/// them would give the pair two of each and the base's methods and the subclass's would
/// write different ones. a list that is not its base's followed by something more is
/// some other shape entirely, and this says nothing about it.
///
/// a class holding nothing of its own asks nothing of the base's layout. it declares no
/// region and reads the base's fields through the descriptors the base published, so
/// there is no list of its own to line up — it stands here to carry the three slots and
/// nothing else
fn appended_over_an_emitted_base<'a>(module: &'a ModuleIr, class: &ClassIr) -> Option<&'a ClassIr> {
    let base = base_declared_earlier(module, class)?;
    let extends = class.fields.is_empty()
        || (class.fields.len() > base.fields.len()
            && class
                .fields
                .iter()
                .zip(&base.fields)
                .all(|(field, inherited)| field == inherited));
    (extends && built_ahead(module, base)).then_some(base)
}

/// whether this class names `name` in its own header
///
/// module init builds every class's type whatever else it leaves out, and a class that
/// stands on another is handed that other's type object to build on — the spec chain
/// through `By_SpecSubclass`, and the ordinary construction which packs it into a bases
/// tuple. so this reference is live even where nothing ever constructs either class
fn stands_on(class: &ClassIr, name: &str) -> bool {
    class
        .base
        .as_ref()
        .is_some_and(|base| base.plain_names().any(|written| written == name))
}

/// whether anything that still happens reaches into `name`'s storage
///
/// `unbuilt` is the classes whose types module init is leaving NULL, so their methods can
/// never be called and are not asked. everything else is: this module's own functions, and
/// the methods and fields of every class that still gets a type — and, whatever is in
/// `unbuilt`, every class that [stands on](stands_on) this one, because that read happens
/// while the type is built rather than when an instance is made
fn read_outside(module: &ModuleIr, unbuilt: &[&ClassIr], name: &str) -> bool {
    module
        .classes
        .iter()
        .any(|candidate| stands_on(candidate, name))
        || module
            .functions
            .iter()
            .any(|function| function.names_class(name))
        || module
            .classes
            .iter()
            .filter(|candidate| !unbuilt.iter().any(|held| ptr::eq(*held, *candidate)))
            .any(|candidate| {
                candidate
                    .fields
                    .iter()
                    .any(|field| field.ty.instance_classes().contains(&name))
                    || candidate
                        .methods
                        .iter()
                        .any(|method| method.names_class(name))
            })
}

/// the classes that go unbuilt with `class`, where it is left as its interpreted definition
///
/// a spec class's own methods are not the only compiled code that reads its storage. a
/// generator method's state object and a nested function's closure environment are each a
/// class of their own, and each captures the `self` it was made from — so each names the
/// class exactly as any other reader would. counting those against it is what made the
/// narrower refusal fire on nothing: every spec class with a generator method or a nested
/// function has one.
///
/// but neither is in the module namespace under any name, and neither is built by anything
/// except the methods of the class it belongs to. where that class has no type its methods
/// never run, so these are never constructed — and they are gathered up with it rather
/// than held against it.
///
/// computed by removal, which is also what makes a cycle come out right: two helpers that
/// only ever build each other are reached from nothing and both stay. the set starts as
/// everything that could go unbuilt and gives up whatever some still-running code turns
/// out to reach, until it stops shrinking. that terminates because [`read_outside`] only
/// ever becomes *more* true as the set shrinks, so nothing given up is ever taken back
fn unbuilt_with<'a>(module: &'a ModuleIr, class: &'a ClassIr) -> Vec<&'a ClassIr> {
    let mut held: Vec<&ClassIr> = vec![class];
    held.extend(
        module
            .classes
            .iter()
            // by identity: two classes in one module may be written with the same name
            .filter(|candidate| !ptr::eq(*candidate, class) && !candidate.exported),
    );
    loop {
        let reached = held
            .iter()
            .position(|candidate| read_outside(module, &held, &candidate.name));
        match reached {
            // the class being asked about is reached itself, so there is nothing to
            // decide about the rest: the caller reads the set it is alone in as the
            // whole-module refusal it always had
            Some(0) => return vec![class],
            Some(index) => {
                held.remove(index);
            }
            None => return held,
        }
    }
}

/// whether a class this module could not build may be left out on its own, with the rest
/// of the module still compiled
///
/// a spec that appends storage can refuse — the base may be a heap type, or carry a
/// metaclass, or put its `__dict__` somewhere the spec cannot reach — and there is no
/// second construction to try. the standing answer to that is to refuse the *module*: the
/// interpreted definition already built every class, so leaving it standing is a whole
/// module that is merely slow rather than a half-native mixture that is wrong.
///
/// that is heavier than it needs to be where nothing compiled would ever have touched the
/// class. the reason the refusal has to be whole-module is that a compiled function may
/// read one of these instances as its own struct, at an offset only the emitted type lays
/// out — and against the interpreted definition's instance, which stops where the base's
/// does, that read lands past the end of the object. where no compiled code that can still
/// run names the class, there is no such read to be wrong: the class alone falls back to
/// its interpreted definition and every compiled function in the module goes on standing.
///
/// what counts as reaching into it is deliberately wide, because missing one costs a wrong
/// answer or a segfault where an extra one costs only the whole-module refusal we already
/// had. see [`read_outside`] for the three places, [`stands_on`] for the one that holds
/// however little else runs, and [`unbuilt_with`] for the helper classes that go quiet
/// along with it
fn declines_on_its_own(module: &ModuleIr, class: &ClassIr) -> bool {
    built_ahead(module, class) && answers_for_its_classes(module) && {
        let unbuilt = unbuilt_with(module, class);
        !read_outside(module, &unbuilt, &class.name)
    }
}

/// the emitted class an operand holds an instance of, where the walk in
/// [`answers_for_its_classes`] has seen one put there
fn instance_held<'a>(holds: &[(RegisterId, &'a str)], value: &Value) -> Option<&'a str> {
    let Value::Register(id) = value else {
        return None;
    };
    holds
        .iter()
        .find(|(held, _)| held == id)
        .map(|(_, class)| *class)
}

/// whether every class this module emits can answer for the attributes its own compiled
/// code reaches on an instance of one
///
/// an emitted class **is** its layout and there is nothing behind it: no instance dict, so
/// an attribute the layout does not hold cannot be written at all, and `__dict__` is not
/// there to be read. where the frontend has no field for either it lowers the dynamic
/// form — the receiver boxed to an object and `PyObject_SetAttr` over it — which python
/// then refuses on a type that publishes no dict.
///
/// the frontend answers this now: every shape that gives the receiver an attribute reaches
/// the layout, and a write of a name nothing in the receiver's chain holds declines rather
/// than lowering the dynamic form. `concurrent.futures.process._ThreadWakeup`, which
/// assigns `self._reader, self._writer` from a pair, is laid out with both;
/// `multiprocessing.dummy.Namespace`, which writes through `self.__dict__`, declines.
///
/// so this asks a question the IR reaching it should already have settled, over the
/// emitted IR rather than over the source it came from — which is where a lowering added
/// later would be caught. what it must not do is *install* such a class: a module that
/// held together only because it gave itself up whole would start doing so the moment one
/// refusal narrowed. so while a module holds one, the guard keeps the whole-module answer
/// it already gave
fn answers_for_its_classes(module: &ModuleIr) -> bool {
    !module.all_functions().any(|function| {
        // which register holds an instance of which emitted class. the receiver of the
        // dynamic form has been boxed to an object by then, so the declared type no
        // longer says — where the value came from does
        let mut holds: Vec<(RegisterId, &str)> = Vec::new();
        for (id, decl) in function.registers.iter().enumerate() {
            if let Some(class) = decl.ty.instance_classes().first() {
                holds.push((RegisterId(id), class));
            }
        }
        function.blocks.iter().any(|block| {
            let mut boxed = holds.clone();
            for op in &block.ops {
                match op {
                    Op::Box { dest, src } => {
                        if let Some(class) = instance_held(&boxed, src) {
                            boxed.push((*dest, class));
                        }
                    }
                    // any write of a name the class does not hold, and the one read that
                    // is never a method or a class-level constant somewhere else
                    Op::SetAttr { receiver, name, .. } | Op::GetAttr { receiver, name, .. }
                        if matches!(op, Op::SetAttr { .. }) || name == "__dict__" =>
                    {
                        if let Some(class) = instance_held(&boxed, receiver)
                            && let Some(owner) = class_named(module, class)
                            && !instance_dict(module, owner)
                            && !owner.fields.iter().any(|field| field.name == *name)
                        {
                            return true;
                        }
                    }
                    _ => {}
                }
            }
            false
        })
    })
}

/// the fields this class keeps in storage of its own
///
/// for a class appended over an emitted base that is the run past its base's fields:
/// the base's own storage is a region of its own, and this class's region holds only
/// what it adds. every other class stores the whole of what it declares — the struct
/// extension model, where a subclass's struct begins with its base's so that a pointer
/// to one is a pointer to the other
fn own_fields<'a>(module: &ModuleIr, class: &'a ClassIr) -> &'a [by_ir::function::FieldDecl] {
    match appended_over_an_emitted_base(module, class) {
        Some(base) => class.fields.get(base.fields.len()..).unwrap_or_default(),
        None => &class.fields,
    }
}

/// how far down a chain of appended storage the region holding `field` is, and the class
/// that keeps it there
///
/// each rung of such a chain keeps its own fields in a region of its own, so a field a
/// base declared is reached through the base's type rather than through this one's.
/// everywhere else the answer is this class at rung zero, which is what every generated
/// body already assumes
fn field_rung<'a>(module: &'a ModuleIr, class: &'a ClassIr, field: &str) -> (usize, &'a ClassIr) {
    let mut current = class;
    // bounded by the class count, for the reason [`inherits_layout`] gives
    for rung in 0..=module.classes.len() {
        if own_fields(module, current)
            .iter()
            .any(|decl| decl.name == field)
        {
            return (rung, current);
        }
        match appended_over_an_emitted_base(module, current) {
            Some(base) => current = base,
            None => return (rung, current),
        }
    }
    (0, class)
}

/// the class whose storage holds `field` for a receiver this class's methods see
fn field_owner<'a>(module: &'a ModuleIr, class: &'a ClassIr, field: &str) -> &'a ClassIr {
    field_rung(module, class, field).1
}

/// whether this class can be built the way a `class` statement builds one — by calling
/// its metaclass — rather than from a type spec
///
/// `meta(name, bases, namespace, **keywords)` takes a base whose metaclass is not
/// `type`, which a spec cannot, and it takes the keywords, which a spec has nowhere to
/// put. what it gives up is the instance layout: how big an instance is becomes the
/// metaclass's answer, so a class with **any** field has nowhere to keep it — its own
/// or one it inherits, since a subclass's struct begins with its base's. everything
/// else a spec would have carried — the methods, and through them the type slots —
/// goes in the namespace, which is where python puts it too
///
/// a class-level constant goes in the namespace as well, so it is not a reason to keep a
/// class off this construction. what it *is* is a reason to check afterwards: the
/// namespace is where a metaclass reinterprets what the body wrote, and an `EnumType`
/// handed `STRICT = 'strict'` builds a member the module body's references do not name.
/// `By_ConstantsHeldUp` is that check, and where it fails the interpreted definition
/// stands — the same answer such a class had when this said no to it outright
///
/// a decorated method rides in beside the constants and for the same reason. it is
/// carried rather than applied: the interpreted body ran the decorator once already, so
/// what it produced is what a `class` statement would have handed the metaclass, and
/// applying the decorator to the method table's own entry instead would both run it a
/// second time and arrive too late for a metaclass reading the namespace. that needs a
/// body to take the value off, which only an exported class has — see
/// [`carried_off_the_body`]
fn metaclass_construction(class: &ClassIr) -> bool {
    class.fields.is_empty()
        // a resumable class is a generator's state object: its state *is* its fields,
        // so this is already false, and nothing in the language can name it as a base
        && class.resume.is_none()
        && (class.exported || !decorates_a_method(class))
}

/// whether any of this class's methods carries a decorator
fn decorates_a_method(class: &ClassIr) -> bool {
    class
        .methods
        .iter()
        .any(|method| !method.decorators.is_empty())
}

/// every namespace entry this class takes off the interpreted body, the ones it cannot do
/// without first
///
/// `By_ClassConstants::required` counts from the front of the list, so a decorated
/// method's name has to stand ahead of every constant's: a constant the body did not write
/// leaves the class without the name, while a decorated method's absence would leave the
/// method table's own undecorated entry answering for it
fn carried_off_the_body(class: &ClassIr) -> (Vec<&str>, usize) {
    let decorated: Vec<&str> = class
        .methods
        .iter()
        .filter(|method| !method.decorators.is_empty())
        .map(|method| method.name.as_str())
        .collect();
    let required = decorated.len();
    let carried = decorated
        .into_iter()
        .chain(class.constants.iter().map(String::as_str))
        .collect();
    (carried, required)
}

/// the class in this module named `name`, when it has one
fn class_named<'a>(module: &'a ModuleIr, name: &str) -> Option<&'a ClassIr> {
    module.classes.iter().find(|class| class.name == name)
}

/// the static holding a dispatch site's licence to reach one compiled method body
/// without the protocol — see `By_ArmMethod`
fn dispatch_licence(module: &ModuleIr, class: &str, method: &str) -> String {
    format!(
        "by_stands_{}_{}_{}",
        mangle(module.name.dotted()),
        mangle(class),
        mangle(method)
    )
}

/// every `(class, method)` pair some dispatch site in this module tests
///
/// one static each, armed once at import. the pairs are collected from the emitted
/// operations rather than from the classes, because a class whose method nothing
/// dispatches on needs no licence and should not pay a lookup for one
fn dispatch_licences(module: &ModuleIr) -> BTreeSet<(String, String)> {
    let mut wanted = BTreeSet::new();
    for function in module.all_functions() {
        for block in &function.blocks {
            for op in &block.ops {
                if let Op::MethodStands { class, method, .. } = op {
                    wanted.insert((class.clone(), method.clone()));
                }
            }
        }
    }
    wanted
}

/// the field storage of `object`, as an expression of type `Fields *`
///
/// for a class that owns its layout the object *is* the storage and this is the cast
/// that has always been emitted. for one appending to a base it is a lookup, because
/// the offset depends on a base size known only at runtime
fn fields_of(module: &ModuleIr, class: &ClassIr, object: &str) -> String {
    let struct_name = class.struct_name(module.name.dotted());
    if external_storage(module, class) {
        return format!(
            "(({struct_name} *)By_TypeData({object}, {}_OBJ))",
            class.type_name(module.name.dotted())
        );
    }
    format!("({struct_name} *){object}")
}

/// declare `self` as this class's field storage, taken from an object pointer
///
/// every generated getter, setter and constructor starts with this line, and it is the
/// only place they need to know whether the storage is the object or sits past it
fn bind_self(module: &ModuleIr, class: &ClassIr, object: &str) -> String {
    format!(
        "{} *self = {};",
        class.struct_name(module.name.dotted()),
        fields_of(module, class, object)
    )
}

/// the field storage a receiver named by the IR keeps `field` in, as an expression
///
/// the class the op names is the receiver's, which for an inherited field is not the
/// class that stores it — a chain of appended storage gives every rung a region of its
/// own, and the field lives in the region of the rung that declared it
fn receiver_fields(module: &ModuleIr, class: &str, field: &str, receiver: &Value) -> String {
    match class_named(module, class) {
        Some(owner) if external_storage(module, owner) => fields_of(
            module,
            field_owner(module, owner, field),
            &value_expr(receiver),
        ),
        // the register already *is* the storage, and saying so again would only add a
        // cast to every field access in the module
        _ => value_expr(receiver),
    }
}

/// the storage bindings a constructor needs, one per rung of appended storage
///
/// a constructor fills every field the class declares, and a chain of appended storage
/// keeps each rung's in a region of its own — so it needs a pointer to each. the class's
/// own is `self`, which is what every other generated body already reads.
///
/// a rung holding nothing is counted and left unbound: it declared no region to point
/// at, and no field can name it — but the numbering is [`field_rung`]'s, which counts
/// every rung the chain passes through
fn bind_storage_chain(module: &ModuleIr, class: &ClassIr) -> String {
    let mut out = bind_self(module, class, "selfobj");
    let mut current = class;
    let mut rung = 0;
    // bounded by the class count, for the reason [`inherits_layout`] gives
    for _ in 0..=module.classes.len() {
        let Some(base) = appended_over_an_emitted_base(module, current) else {
            break;
        };
        rung += 1;
        if !own_fields(module, base).is_empty() {
            let _ = write!(
                out,
                "\n    {} *by_up{rung} = {};",
                base.struct_name(module.name.dotted()),
                fields_of(module, base, "selfobj")
            );
        }
        current = base;
    }
    out
}

/// the name [`bind_storage_chain`] bound for the storage holding `field`
fn storage_name(module: &ModuleIr, class: &ClassIr, field: &str) -> String {
    match field_rung(module, class, field).0 {
        0 => "self".to_string(),
        rung => format!("by_up{rung}"),
    }
}

/// whether a base this module emits stands in this class's list beside one it does not
///
/// a type spec takes its whole instance shape from the one base python picks out of the
/// list, so where that is one of ours the `__dict__` an outside base needs is simply
/// dropped: the type claims a managed dict it has no room for, and the first attribute
/// read on an instance walks off the object. calling the metaclass works the shape out
/// from every base at once, which is why such a class is built that way and no other
fn stands_on_an_emitted_base(module: &ModuleIr, class: &ClassIr) -> bool {
    class.base.as_ref().is_some_and(|base| {
        base.external().is_some()
            && base
                .plain_names()
                .any(|name| class_named(module, name).is_some())
    })
}

/// whether any class in the module is built on this one
///
/// not only the ones that extend its *layout*: a class standing beside names from
/// outside still has to be a type python can derive from, and only a heap type is
fn is_base(module: &ModuleIr, class: &ClassIr) -> bool {
    module.classes.iter().any(|candidate| {
        candidate
            .base
            .as_ref()
            .is_some_and(|base| base.plain_names().any(|name| name == class.name))
    })
}

/// whether a class needs a *mutable* heap type rather than a sealed one
///
/// a sealed type is immutable and cannot be subclassed, which is exactly what
/// licenses the direct method call. a decorator that touches the class, and a
/// subclass, each need the opposite — so those classes pay for it.
///
/// a written `__new__` is the third: it is published by *assigning* it onto the finished
/// type, which is the only way to reach the slot fixup that a class statement runs and a
/// type spec does not — see [`publishes_new`]. an immutable type refuses that assignment
fn mutable_type(module: &ModuleIr, class: &ClassIr) -> bool {
    !class.decorators.is_empty()
        || is_base(module, class)
        || class.base.is_some()
        || publishes_new(class).is_some()
}

/// the written `__new__` this class publishes onto its finished type, where it wrote one
///
/// `tp_new` is deliberately **not** filled from the spec. a C function there is one
/// python reads as a base that owns the allocation, and `object.__new__(cls)` — which is
/// how almost every written `__new__` gets its instance — is then refused as unsafe,
/// because the check walks up from the class looking for the allocator and stops at ours.
///
/// assigning the method onto the type instead is what a class statement does: python's
/// own slot fixup sees a `__new__` in the dict and installs the dispatcher that looks the
/// name up, so the class ends up with exactly the `tp_new` an interpreted one has. the
/// allocation check then walks past it to `object`, and the body's `object.__new__(cls)`
/// is the plain allocation it was written as
fn publishes_new(class: &ClassIr) -> Option<&Function> {
    dunder(class, "__new__")
}

/// whether a construction of this class runs a written `__new__`, its bases' included
///
/// a subclass inherits the slot the assignment installed on its base, so it must not
/// name one of its own — filling `tp_new` with the generic allocation would put back
/// exactly the answer the base overrode, and the subclass would allocate without ever
/// running the constructor it inherited
fn constructs_through_a_written_new(module: &ModuleIr, class: &ClassIr) -> bool {
    let mut current = class;
    // bounded by the class count, for the reason [`inherits_layout`] gives
    for _ in 0..=module.classes.len() {
        if publishes_new(current).is_some() {
            return true;
        }
        let Some(name) = current.base.as_ref().and_then(ClassBase::in_module) else {
            return false;
        };
        match class_named(module, name) {
            Some(next) => current = next,
            None => return false,
        }
    }
    false
}

/// whether a class's type is built from a spec at import rather than being a static
/// struct
///
/// a static type is not a heap type, and python answers `__annotations__` on one by
/// refusing outright — the getset on the metatype reads the flag before it reads the
/// dict, so nothing written into `tp_dict` can be reached through it. a class the
/// source can name has to answer that the way its interpreted twin does, so it is
/// built from a spec whether or not anything is going to mutate it. what a spec
/// costs is only what the flags say it costs: a class that would have been static is
/// sealed — immutable to `setattr` and no base type — so it is the same class it was
/// and the direct method call stands.
///
/// the classes that are *not* exported are the ones no name reaches: a generator's
/// state and a closure's environment. `__annotations__` on one of those is
/// unobservable, and they are the only classes carrying an iterator surface — which a
/// spec spells differently from a static struct.
///
/// a field spelled as a dunder is the other class left alone, and it is the same
/// asymmetry read backwards: an emitted class has no instance dict, so an attribute of
/// the instance is a descriptor in the *type's* dict — and the type machinery reads a
/// heap type's `__module__` and `__doc__` out of that dict while it works a static
/// type's out of `tp_name`. `functools.cached_property` keeps a `__module__` of its own,
/// so as a heap type it would answer its own descriptor where the class should answer
/// the module's name. staying static is what it already was, refusal on
/// `__annotations__` included
fn heap_type(module: &ModuleIr, class: &ClassIr) -> bool {
    mutable_type(module, class)
        || (class.exported
            && !class
                .fields
                .iter()
                .any(|field| field.name.starts_with("__") && field.name.ends_with("__")))
}

/// a mutable heap type's iterator slots, as `(slot id, value)` pairs
/// the type slots a dunder method fills, and how to reach them
///
/// a method table cannot fill a slot: python looks up `__repr__` on a *static*
/// type by reading `tp_repr`, and a name in `tp_methods` is never consulted. so
/// each of these is installed twice — once as an ordinary method, so
/// `obj.__repr__()` works, and once as the slot, so `repr(obj)` does
const DUNDER_SLOTS: &[(&str, &str, &str, SlotShape)] = &[
    ("__repr__", "Py_tp_repr", "tp_repr", SlotShape::Unary),
    ("__str__", "Py_tp_str", "tp_str", SlotShape::Unary),
    (
        "__del__",
        "Py_tp_finalize",
        "tp_finalize",
        SlotShape::Finalize,
    ),
    (
        "__getattr__",
        "Py_tp_getattro",
        "tp_getattro",
        SlotShape::GetAttrHook,
    ),
    (
        "__len__",
        "Py_mp_length",
        "tp_as_mapping",
        SlotShape::Length,
    ),
    ("__bool__", "Py_nb_bool", "tp_as_number", SlotShape::Truth),
    ("__hash__", "Py_tp_hash", "tp_hash", SlotShape::Hash),
    ("__aiter__", "Py_am_aiter", "tp_as_async", SlotShape::Unary),
    ("__anext__", "Py_am_anext", "tp_as_async", SlotShape::Unary),
    ("__await__", "Py_am_await", "tp_as_async", SlotShape::Unary),
    ("__iter__", "Py_tp_iter", "tp_iter", SlotShape::Unary),
    (
        "__next__",
        "Py_tp_iternext",
        "tp_iternext",
        SlotShape::Unary,
    ),
    (
        "__getitem__",
        "Py_mp_subscript",
        "tp_as_mapping",
        SlotShape::Binary,
    ),
    (
        "__setitem__",
        "Py_mp_ass_subscript",
        "tp_as_mapping",
        SlotShape::SetItem,
    ),
    (
        "__contains__",
        "Py_sq_contains",
        "tp_as_sequence",
        SlotShape::Contains,
    ),
    (
        "__neg__",
        "Py_nb_negative",
        "tp_as_number",
        SlotShape::Unary,
    ),
    (
        "__pos__",
        "Py_nb_positive",
        "tp_as_number",
        SlotShape::Unary,
    ),
    (
        "__abs__",
        "Py_nb_absolute",
        "tp_as_number",
        SlotShape::Unary,
    ),
    (
        "__invert__",
        "Py_nb_invert",
        "tp_as_number",
        SlotShape::Unary,
    ),
    ("__int__", "Py_nb_int", "tp_as_number", SlotShape::Unary),
    ("__float__", "Py_nb_float", "tp_as_number", SlotShape::Unary),
    ("__index__", "Py_nb_index", "tp_as_number", SlotShape::Unary),
    ("__call__", "Py_tp_call", "tp_call", SlotShape::Call),
    (
        "__get__",
        "Py_tp_descr_get",
        "tp_descr_get",
        SlotShape::DescrGet,
    ),
    // the in-place forms have no reflected variant and no type check: python only
    // reaches one through the *left* operand's type, which is always ours
    (
        "__iadd__",
        "Py_nb_inplace_add",
        "tp_as_number",
        SlotShape::Binary,
    ),
    (
        "__isub__",
        "Py_nb_inplace_subtract",
        "tp_as_number",
        SlotShape::Binary,
    ),
    (
        "__imul__",
        "Py_nb_inplace_multiply",
        "tp_as_number",
        SlotShape::Binary,
    ),
    (
        "__itruediv__",
        "Py_nb_inplace_true_divide",
        "tp_as_number",
        SlotShape::Binary,
    ),
    (
        "__ifloordiv__",
        "Py_nb_inplace_floor_divide",
        "tp_as_number",
        SlotShape::Binary,
    ),
    (
        "__imod__",
        "Py_nb_inplace_remainder",
        "tp_as_number",
        SlotShape::Binary,
    ),
    (
        "__ilshift__",
        "Py_nb_inplace_lshift",
        "tp_as_number",
        SlotShape::Binary,
    ),
    (
        "__irshift__",
        "Py_nb_inplace_rshift",
        "tp_as_number",
        SlotShape::Binary,
    ),
    (
        "__iand__",
        "Py_nb_inplace_and",
        "tp_as_number",
        SlotShape::Binary,
    ),
    (
        "__ixor__",
        "Py_nb_inplace_xor",
        "tp_as_number",
        SlotShape::Binary,
    ),
    (
        "__ior__",
        "Py_nb_inplace_or",
        "tp_as_number",
        SlotShape::Binary,
    ),
    (
        "__imatmul__",
        "Py_nb_inplace_matrix_multiply",
        "tp_as_number",
        SlotShape::Binary,
    ),
];

/// the arithmetic dunders, each with the reflected method that answers when it
/// was the *other* operand's type python asked
///
/// python hands `nb_add` its operands in the order they were written whichever
/// type it asked, so the adapter has to work out which side is ours before it
/// knows whether this is `__add__` or `__radd__`
const ARITHMETIC: &[(&str, &str, &str, &str)] = &[
    ("__add__", "__radd__", "Py_nb_add", "nb_add"),
    ("__sub__", "__rsub__", "Py_nb_subtract", "nb_subtract"),
    ("__mul__", "__rmul__", "Py_nb_multiply", "nb_multiply"),
    (
        "__truediv__",
        "__rtruediv__",
        "Py_nb_true_divide",
        "nb_true_divide",
    ),
    (
        "__floordiv__",
        "__rfloordiv__",
        "Py_nb_floor_divide",
        "nb_floor_divide",
    ),
    ("__mod__", "__rmod__", "Py_nb_remainder", "nb_remainder"),
    ("__divmod__", "__rdivmod__", "Py_nb_divmod", "nb_divmod"),
    ("__lshift__", "__rlshift__", "Py_nb_lshift", "nb_lshift"),
    ("__rshift__", "__rrshift__", "Py_nb_rshift", "nb_rshift"),
    ("__and__", "__rand__", "Py_nb_and", "nb_and"),
    ("__xor__", "__rxor__", "Py_nb_xor", "nb_xor"),
    ("__or__", "__ror__", "Py_nb_or", "nb_or"),
    (
        "__matmul__",
        "__rmatmul__",
        "Py_nb_matrix_multiply",
        "nb_matrix_multiply",
    ),
    // `__pow__` is absent because its slot is *ternary* — python passes the optional
    // modulus through it — so it does not share this adapter's shape. see [`POWER`]
];

/// `__pow__` and the reflected form, which share the one ternary numeric slot
///
/// `pow(a, b, m)` passes the modulus through `nb_power`, and python spells the
/// two-argument form as a `None` modulus. that is the whole difference from
/// [`ARITHMETIC`]: with a modulus there is no reflected direction at all, because
/// only the left operand's `__pow__` takes three arguments
const POWER: (&str, &str, &str, &str) = ("__pow__", "__rpow__", "Py_nb_power", "nb_power");

/// the branch of a numeric slot that answers when `receiver` is an instance of ours
///
/// python hands `nb_add` and its siblings their operands in the order they were
/// written whichever type it asked, so the adapter has to work out which side is ours
/// before it knows which direction this is
fn on_our_operand(
    module: &ModuleIr,
    type_name: &str,
    filler: &SlotFiller<'_>,
    receiver: &str,
    args: &[&str],
) -> String {
    format!(
        "    if (PyObject_TypeCheck({receiver}, (PyTypeObject *){type_name}_OBJ)) {{\n\
         \x20       PyObject *by_argv[] = {{ {} }};\n\
         \x20       return {};\n    }}\n",
        args.join(", "),
        filler.call(module, receiver, "by_argv", args.len())
    )
}

/// `nb_power`, which `__pow__` and `__rpow__` share with the three-argument `pow`
fn emit_power_adapter(module: &ModuleIr, class: &ClassIr, type_name: &str) -> String {
    let (name, reflected, _, field) = POWER;
    let forward = slot_filler(class, type_name, name);
    let backward = slot_filler(class, type_name, reflected);
    if forward.is_none() && backward.is_none() {
        return String::new();
    }
    let mut binary = String::new();
    for (filler, receiver, other) in [
        (forward.as_ref(), "by_a", "by_b"),
        (backward.as_ref(), "by_b", "by_a"),
    ] {
        let Some(filler) = filler else { continue };
        binary.push_str(&on_our_operand(
            module,
            type_name,
            filler,
            receiver,
            &[other],
        ));
    }
    // a modulus reaches only the left operand's `__pow__`, and a class that wrote a
    // two-parameter one raises there — which is what python raises too
    let ternary = forward.as_ref().map_or_else(String::new, |filler| {
        on_our_operand(module, type_name, filler, "by_a", &["by_b", "by_c"])
    });
    format!(
        "static PyObject *{type_name}_{field}(PyObject *by_a, PyObject *by_b, PyObject *by_c) {{\n\
         \x20   if (by_c == Py_None) {{\n\
         {binary}\x20       Py_RETURN_NOTIMPLEMENTED;\n\
         \x20   }}\n\
         {ternary}\x20   Py_RETURN_NOTIMPLEMENTED;\n}}\n"
    )
}

/// the comparison dunders, which share one slot
///
/// python does not look any of these up by name: it calls `tp_richcompare` with
/// an opcode, so all six are one function that dispatches on it — and one the
/// class does not define answers `NotImplemented`, which is what lets the *other*
/// operand's type try
const COMPARISONS: &[(&str, &str)] = &[
    ("__lt__", "Py_LT"),
    ("__le__", "Py_LE"),
    ("__eq__", "Py_EQ"),
    ("__ne__", "Py_NE"),
    ("__gt__", "Py_GT"),
    ("__ge__", "Py_GE"),
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum SlotShape {
    /// `PyObject *f(PyObject *self)`
    Unary,
    /// `Py_ssize_t f(PyObject *self)`
    Length,
    /// `int f(PyObject *self)`
    Truth,
    /// `Py_hash_t f(PyObject *self)`
    Hash,
    /// `PyObject *f(PyObject *self, PyObject *arg)`
    Binary,
    /// `int f(PyObject *self, PyObject *key, PyObject *value)`
    SetItem,
    /// `int f(PyObject *self, PyObject *value)`
    Contains,
    /// `PyObject *f(PyObject *self, PyObject *args, PyObject *kwargs)`
    Call,
    /// `PyObject *f(PyObject *self, PyObject *obj, PyObject *type)`
    DescrGet,
    /// `void f(PyObject *self)`
    Finalize,
    /// `PyObject *f(PyObject *self, PyObject *name)`, tried only after the ordinary
    /// lookup has failed
    GetAttrHook,
}

/// whether this class has a method for `dunder`
fn dunder<'a>(class: &'a ClassIr, name: &str) -> Option<&'a Function> {
    class.methods.iter().find(|method| method.name == name)
}

/// the value the class body *assigned* to `dunder`, where it assigned one to call
///
/// `__hash__ = None` is left out on purpose: it names nothing to call, and the slot it
/// asks for is python's standing "unhashable" rather than an adapter — see
/// [`hash_slot_override`]
fn assigned_dunder<'a>(class: &'a ClassIr, name: &str) -> Option<&'a SlotAlias> {
    class
        .slot_aliases
        .iter()
        .find(|alias| alias.name == name && !alias.unsupported)
}

/// the C variable holding what the body assigned to `name`, filled by module init
fn alias_cell(type_name: &str, name: &str) -> String {
    format!("{type_name}_alias_{}", name.trim_matches('_'))
}

/// the static holding the class-level value a defaulted field falls back to
fn default_cell(type_name: &str, field: &str) -> String {
    format!("{type_name}_default_{}", mangle(field))
}

/// where a defaulted field's class-level value lives, and whether this class is the one
/// that fills the cell
///
/// the cell belongs to the class whose body wrote the value, so a subclass that inherits
/// the field reads that same cell and fills nothing of its own — which is what makes the
/// base's value the answer for a subclass that binds none
fn field_default(
    module: &ModuleIr,
    class: &ClassIr,
    field: &by_ir::function::FieldDecl,
) -> Option<(String, bool)> {
    let owner = class_named(module, field.defaulted_by.as_deref()?)?;
    Some((
        default_cell(&owner.type_name(module.name.dotted()), &field.name),
        owner.name == class.name,
    ))
}

/// the cell a field named by the IR falls back to, where it has one
fn named_field_default(module: &ModuleIr, class: &str, field: &str) -> Option<String> {
    let decl = field_decl(module, class, field)?;
    let owner = class_named(module, decl.defaulted_by.as_deref()?)?;
    Some(default_cell(
        &owner.type_name(module.name.dotted()),
        &decl.name,
    ))
}

/// what answers a type slot: a method the class defined, or a value its body assigned
///
/// `__repr__ = _repr` has to fill `tp_repr` exactly as a `def __repr__` does, and the
/// only difference is where the callable is found. a method is reached through its own
/// wrapper symbol; an assignment through the cell module init copied its value into
enum SlotFiller<'a> {
    Method(&'a Function),
    Assigned(String),
}

impl SlotFiller<'_> {
    /// the call that answers the slot, with `argv` a `PyObject *const *` expression
    fn call(&self, module: &ModuleIr, receiver: &str, argv: &str, argc: usize) -> String {
        match self {
            SlotFiller::Method(method) => format!(
                "{}({receiver}, {argv}, {argc}, NULL)",
                method.wrapper_symbol(module.name.dotted())
            ),
            SlotFiller::Assigned(cell) => {
                format!("By_CallSlotAlias({cell}, {receiver}, {argv}, {argc})")
            }
        }
    }

    /// the call for a slot handed a tuple and a dict rather than a vector
    fn call_with_tuple(&self, module: &ModuleIr, receiver: &str, args: &str, kw: &str) -> String {
        match self {
            SlotFiller::Method(method) => format!(
                "By_CallSlot({}, {receiver}, {args}, {kw})",
                method.wrapper_symbol(module.name.dotted())
            ),
            SlotFiller::Assigned(cell) => {
                format!("By_CallSlotAliasTuple({cell}, {receiver}, {args}, {kw})")
            }
        }
    }
}

/// what fills this slot for this class, where anything does
fn slot_filler<'a>(class: &'a ClassIr, type_name: &str, name: &str) -> Option<SlotFiller<'a>> {
    if let Some(method) = dunder(class, name) {
        return Some(SlotFiller::Method(method));
    }
    assigned_dunder(class, name)
        .map(|alias| SlotFiller::Assigned(alias_cell(type_name, &alias.name)))
}

/// whether anything the class body wrote answers this slot
fn answers_slot(class: &ClassIr, name: &str) -> bool {
    dunder(class, name).is_some() || assigned_dunder(class, name).is_some()
}

/// the other method that fills the same slot as `name`
///
/// `del obj[k]` is `mp_ass_subscript` with a NULL value, so `__setitem__` and
/// `__delitem__` are one slot between them and either alone still has to fill it
fn slot_companion(name: &str) -> Option<&'static str> {
    match name {
        "__setitem__" => Some("__delitem__"),
        _ => None,
    }
}

/// whether the class fills this slot, by either of the two that can
fn fills_slot(class: &ClassIr, name: &str) -> bool {
    answers_slot(class, name)
        || slot_companion(name).is_some_and(|other| answers_slot(class, other))
}

/// the names an emitted type publishes that the class body never wrote
///
/// `PyType_Ready` adds a wrapper descriptor for every name a filled slot backs, and three
/// slots back more than one name: `tp_richcompare` backs all six comparisons, each binary
/// number slot backs an operator and its reflection, and `mp_ass_subscript` backs
/// `__setitem__` along with `__delitem__`. so a class writing `__lt__` is published as
/// having all six, and anything reading the class *by name* is told about methods the
/// `class` statement never wrote — which is not what an interpreted class of the same
/// body would have said
fn published_beyond_the_body(class: &ClassIr) -> Vec<&'static str> {
    let mut names = Vec::new();
    if COMPARISONS
        .iter()
        .any(|(name, _)| answers_slot(class, name))
    {
        names.extend(
            COMPARISONS
                .iter()
                .map(|(name, _)| *name)
                .filter(|name| !answers_slot(class, name)),
        );
    }
    for (name, reflected, _, _) in ARITHMETIC.iter().copied().chain([POWER]) {
        for (written, published) in [(name, reflected), (reflected, name)] {
            if answers_slot(class, written) && !answers_slot(class, published) {
                names.push(published);
            }
        }
    }
    for name in ["__setitem__"] {
        let Some(companion) = slot_companion(name) else {
            continue;
        };
        if answers_slot(class, name) && !answers_slot(class, companion) {
            names.push(companion);
        } else if answers_slot(class, companion) && !answers_slot(class, name) {
            names.push(name);
        }
    }
    names
}

/// `mp_ass_subscript`, which `__setitem__` and `__delitem__` share
///
/// a NULL value is `del obj[key]`. a class with only one of the two still fills
/// the slot, and the half it does not have raises what python raises when a slot
/// looks a missing method up — an `AttributeError` naming the method, rather than
/// the `TypeError` an absent protocol would give
fn emit_ass_subscript_adapter(
    module: &ModuleIr,
    class: &ClassIr,
    type_name: &str,
    symbol: &str,
) -> String {
    let half = |name: &str, argc: usize| match slot_filler(class, type_name, name) {
        Some(filler) => format!(
            "        PyObject *by_r = {};\n\
             \x20       if (by_r == NULL) return -1;\n\
             \x20       Py_DECREF(by_r);\n\
             \x20       return 0;\n",
            filler.call(module, "self", "by_argv", argc)
        ),
        None => format!(
            "        PyErr_SetString(PyExc_AttributeError, {});\n\x20       return -1;\n",
            c_string(name)
        ),
    };
    format!(
        "static int {symbol}(PyObject *self, PyObject *by_key, PyObject *by_value) {{\n\
         \x20   PyObject *by_argv[] = {{ by_key, by_value }};\n\
         \x20   if (by_value == NULL) {{\n\
         {}\x20   }}\n\
         {}}}\n",
        half("__delitem__", 1),
        half("__setitem__", 2)
    )
}

/// the adapters that give each dunder method the signature its slot wants
///
/// each is a call into the method's own wrapper, so the argument binding, the
/// representation checks and the boxing are the ones every other call gets
fn emit_dunder_adapters(module: &ModuleIr, class: &ClassIr, type_name: &str) -> String {
    let mut out = String::new();
    // an assigned dunder is reached through a cell rather than a symbol, so the cell has
    // to stand before the adapters that read it. module init fills it from the type's
    // dict, once the constant copy has put the assigned value there
    for alias in &class.slot_aliases {
        if alias.unsupported {
            continue;
        }
        let _ = writeln!(
            out,
            "static PyObject *{} = NULL;",
            alias_cell(type_name, &alias.name)
        );
    }
    for (name, _, _, shape) in DUNDER_SLOTS {
        if !fills_slot(class, name) {
            continue;
        }
        let symbol = format!("{type_name}_{}", name.trim_matches('_'));
        // the shared slot is built from the *class*, because either of the two
        // methods that fill it may be the absent one
        if matches!(shape, SlotShape::SetItem) {
            out.push_str(&emit_ass_subscript_adapter(
                module, class, type_name, &symbol,
            ));
            continue;
        }
        let Some(filler) = slot_filler(class, type_name, name) else {
            continue;
        };
        let call = filler.call(module, "self", "NULL", 0);
        match shape {
            SlotShape::Unary => {
                let _ = writeln!(
                    out,
                    "static PyObject *{symbol}(PyObject *self) {{\n\x20   return {call};\n}}"
                );
            }
            SlotShape::Length => {
                let _ = writeln!(
                    out,
                    "static Py_ssize_t {symbol}(PyObject *self) {{\n\
                     \x20   PyObject *by_r = {call};\n\
                     \x20   if (by_r == NULL) return -1;\n\
                     \x20   Py_ssize_t by_n = PyNumber_AsSsize_t(by_r, PyExc_OverflowError);\n\
                     \x20   Py_DECREF(by_r);\n\
                     \x20   return by_n;\n}}"
                );
            }
            SlotShape::Binary => {
                let _ = writeln!(
                    out,
                    "static PyObject *{symbol}(PyObject *self, PyObject *by_arg) {{\n\
                     \x20   PyObject *by_argv[] = {{ by_arg }};\n\
                     \x20   return {};\n}}",
                    filler.call(module, "self", "by_argv", 1)
                );
            }
            // emitted above, from both of the methods that fill this slot
            SlotShape::SetItem => {}
            SlotShape::Call => {
                // the slot is handed a tuple and a dict, where the method wrapper
                // wants a vector — so this is the one adapter that has to bind
                // rather than forward
                let _ = writeln!(
                    out,
                    "static PyObject *{symbol}(PyObject *self, PyObject *by_args, PyObject *by_kw) {{\n\
                     \x20   return {};\n}}",
                    filler.call_with_tuple(module, "self", "by_args", "by_kw")
                );
            }
            SlotShape::GetAttrHook => {
                // `__getattr__` does not *replace* the lookup, it stands behind it: the
                // ordinary one runs first and only an `AttributeError` falls through to
                // the method. anything else it raised is the answer
                let _ = writeln!(
                    out,
                    "static PyObject *{symbol}(PyObject *self, PyObject *by_name) {{\n\
                     \x20   PyObject *by_r = PyObject_GenericGetAttr(self, by_name);\n\
                     \x20   if (by_r != NULL) return by_r;\n\
                     \x20   if (!PyErr_ExceptionMatches(PyExc_AttributeError)) return NULL;\n\
                     \x20   PyErr_Clear();\n\
                     \x20   PyObject *by_argv[] = {{ by_name }};\n\
                     \x20   return {};\n}}",
                    filler.call(module, "self", "by_argv", 1)
                );
            }
            SlotShape::Finalize => {
                // a finalizer answers nothing and runs while an exception may already
                // be on its way out, so what it raises has to be reported and dropped
                // rather than replacing it — which is what python does for a `__del__`
                let _ = writeln!(
                    out,
                    "static void {symbol}(PyObject *self) {{\n\
                     \x20   PyObject *by_type, *by_value, *by_tb;\n\
                     \x20   PyErr_Fetch(&by_type, &by_value, &by_tb);\n\
                     \x20   PyObject *by_r = {call};\n\
                     \x20   if (by_r == NULL) PyErr_WriteUnraisable(self);\n\
                     \x20   else Py_DECREF(by_r);\n\
                     \x20   PyErr_Restore(by_type, by_value, by_tb);\n}}"
                );
            }
            SlotShape::DescrGet => {
                // the slot passes NULL for the half of the pair that does not apply:
                // no instance when the attribute was read off the class, no owner
                // when it was read off an instance. python's own wrapper substitutes
                // `None` for either, which is what the two-parameter method expects
                let _ = writeln!(
                    out,
                    "static PyObject *{symbol}(PyObject *self, PyObject *by_obj, PyObject *by_type) {{\n\
                     \x20   PyObject *by_argv[] = {{ by_obj ? by_obj : Py_None,\n\
                     \x20                          by_type ? by_type : Py_None }};\n\
                     \x20   return {};\n}}",
                    filler.call(module, "self", "by_argv", 2)
                );
            }
            SlotShape::Contains => {
                let _ = writeln!(
                    out,
                    "static int {symbol}(PyObject *self, PyObject *by_value) {{\n\
                     \x20   PyObject *by_argv[] = {{ by_value }};\n\
                     \x20   PyObject *by_r = {};\n\
                     \x20   if (by_r == NULL) return -1;\n\
                     \x20   int by_v = PyObject_IsTrue(by_r);\n\
                     \x20   Py_DECREF(by_r);\n\
                     \x20   return by_v;\n}}",
                    filler.call(module, "self", "by_argv", 1)
                );
            }
            SlotShape::Hash => {
                let _ = writeln!(
                    out,
                    "static Py_hash_t {symbol}(PyObject *self) {{\n\
                     \x20   PyObject *by_r = {call};\n\
                     \x20   if (by_r == NULL) return -1;\n\
                     \x20   Py_hash_t by_h = By_HashResult(by_r);\n\
                     \x20   Py_DECREF(by_r);\n\
                     \x20   return by_h;\n}}"
                );
            }
            SlotShape::Truth => {
                let _ = writeln!(
                    out,
                    "static int {symbol}(PyObject *self) {{\n\
                     \x20   PyObject *by_r = {call};\n\
                     \x20   if (by_r == NULL) return -1;\n\
                     \x20   int by_v = PyObject_IsTrue(by_r);\n\
                     \x20   Py_DECREF(by_r);\n\
                     \x20   return by_v;\n}}"
                );
            }
        }
    }
    if COMPARISONS
        .iter()
        .any(|(name, _)| answers_slot(class, name))
    {
        let _ = writeln!(
            out,
            "static PyObject *{type_name}_richcompare(PyObject *self, PyObject *other, int op) {{\n\
             \x20   PyObject *by_argv[] = {{ other }};\n\
             \x20   switch (op) {{"
        );
        for (name, opcode) in COMPARISONS {
            let Some(filler) = slot_filler(class, type_name, name) else {
                continue;
            };
            let _ = writeln!(
                out,
                "    case {opcode}: return {};",
                filler.call(module, "self", "by_argv", 1)
            );
        }
        // a comparison the class does not define belongs to whatever the base would have
        // answered it with, because this slot took the whole group over from the base by
        // being filled at all. `object`'s is where `!=` gets its meaning — negate
        // `__eq__` — so a class writing only `__eq__` answered `x != x` as `True` here
        // while the interpreted one answered `False`
        let _ = write!(
            out,
            "    }}\n    return By_BaseRichCompare({type_name}_OBJ, self, other, op);\n}}\n"
        );
    }

    for (name, reflected, _, field) in ARITHMETIC {
        let forward = slot_filler(class, type_name, name);
        let backward = slot_filler(class, type_name, reflected);
        if forward.is_none() && backward.is_none() {
            continue;
        }
        let _ = writeln!(
            out,
            "static PyObject *{type_name}_{field}(PyObject *by_a, PyObject *by_b) {{"
        );
        for (filler, receiver, other) in [(forward, "by_a", "by_b"), (backward, "by_b", "by_a")] {
            let Some(filler) = filler else { continue };
            out.push_str(&on_our_operand(
                module,
                type_name,
                &filler,
                receiver,
                &[other],
            ));
        }
        // neither side is ours, or ours has no method for this direction — and
        // then `NotImplemented` is what lets python try the other operand
        out.push_str("    Py_RETURN_NOTIMPLEMENTED;\n}\n");
    }
    out.push_str(&emit_power_adapter(module, class, type_name));

    // a static type reaches `nb_bool` and `mp_length` through a sub-table, which
    // has to exist before the type that points at it
    let mut mapping = String::new();
    if answers_slot(class, "__len__") {
        let _ = writeln!(mapping, "    .mp_length = {type_name}_len,");
    }
    if answers_slot(class, "__getitem__") {
        let _ = writeln!(mapping, "    .mp_subscript = {type_name}_getitem,");
    }
    if fills_slot(class, "__setitem__") {
        let _ = writeln!(mapping, "    .mp_ass_subscript = {type_name}_setitem,");
    }
    if !mapping.is_empty() {
        let _ = writeln!(
            out,
            "static PyMappingMethods {type_name}_mapping = {{\n{mapping}}};"
        );
    }
    if answers_slot(class, "__contains__") {
        let _ = writeln!(
            out,
            "static PySequenceMethods {type_name}_sequence = {{\n\
             \x20   .sq_contains = {type_name}_contains,\n}};"
        );
    }
    // `am_aiter`, `am_anext` and `am_await` share one async sub-table
    let asynchronous = sub_table_fields(class, type_name, "tp_as_async");
    if !asynchronous.is_empty() {
        let _ = writeln!(
            out,
            "static PyAsyncMethods {type_name}_async = {{\n{asynchronous}}};"
        );
    }
    let number = number_fields(class, type_name);
    if !number.is_empty() {
        let _ = writeln!(
            out,
            "static PyNumberMethods {type_name}_number = {{\n{number}}};"
        );
    }
    out
}

/// the initializers for one `tp_as_*` sub-table a static type points at
///
/// the C member is the slot id without its prefix, so a new [`DUNDER_SLOTS`] entry
/// wires itself
fn sub_table_fields(class: &ClassIr, type_name: &str, table: &str) -> String {
    let mut out = String::new();
    for (name, slot, field, _) in DUNDER_SLOTS {
        if *field == table
            && fills_slot(class, name)
            && let Some(member) = slot.strip_prefix("Py_")
        {
            let _ = writeln!(
                out,
                "    .{member} = {type_name}_{},",
                name.trim_matches('_')
            );
        }
    }
    out
}

/// the initializers for the number sub-table a static type points at
fn number_fields(class: &ClassIr, type_name: &str) -> String {
    let mut out = String::new();
    for (name, reflected, _, field) in ARITHMETIC.iter().copied().chain([POWER]) {
        if answers_slot(class, name) || answers_slot(class, reflected) {
            let _ = writeln!(out, "    .{field} = {type_name}_{field},");
        }
    }
    out.push_str(&sub_table_fields(class, type_name, "tp_as_number"));
    out
}

/// the value `tp_hash` takes, where it is not an adapter of this class's own
///
/// two ways a class ends up unhashable. it can say so outright, by writing
/// `__hash__ = None` — `numbers.Number` is the corpus's example — and python spells
/// that in the slot as `PyObject_HashNotImplemented`. or it can define `__eq__` and
/// no `__hash__`, and then python makes it unhashable for it: two objects that
/// compare equal have to hash equal, and an inherited hash cannot promise that.
/// `type_new` does the second for a class written in python; a type built from a spec
/// has to do both here or the compiled class would be hashable where the interpreted
/// one is not
fn hash_slot_override(class: &ClassIr) -> Option<&'static str> {
    let disowned = class
        .slot_aliases
        .iter()
        .any(|alias| alias.name == "__hash__" && alias.unsupported);
    let defines_equality = COMPARISONS
        .iter()
        .any(|(name, _)| matches!(*name, "__eq__" | "__ne__") && answers_slot(class, name));
    (disowned || (defines_equality && !answers_slot(class, "__hash__")))
        .then_some("PyObject_HashNotImplemented")
}

/// the slot table entries for a heap type
fn dunder_slots(class: &ClassIr, type_name: &str) -> Vec<(String, String)> {
    let mut slots: Vec<(String, String)> = DUNDER_SLOTS
        .iter()
        .filter(|(name, _, _, _)| fills_slot(class, name))
        .map(|(name, slot, _, _)| {
            (
                (*slot).to_string(),
                format!("{type_name}_{}", name.trim_matches('_')),
            )
        })
        .collect();
    if COMPARISONS
        .iter()
        .any(|(name, _)| answers_slot(class, name))
    {
        slots.push((
            "Py_tp_richcompare".to_string(),
            format!("{type_name}_richcompare"),
        ));
    }
    if let Some(hash) = hash_slot_override(class) {
        slots.push(("Py_tp_hash".to_string(), hash.to_string()));
    }
    for (name, reflected, slot, field) in ARITHMETIC.iter().copied().chain([POWER]) {
        if answers_slot(class, name) || answers_slot(class, reflected) {
            slots.push((slot.to_string(), format!("{type_name}_{field}")));
        }
    }
    slots
}

/// the designated initializers for a static type
fn dunder_initializers(class: &ClassIr, type_name: &str) -> String {
    let mut out = String::new();
    for (name, _, field, _) in DUNDER_SLOTS {
        if !fills_slot(class, name) {
            continue;
        }
        // the shape settles the adapter's signature; where the pointer *goes* is
        // settled by the field alone — a `tp_as_*` is a sub-table, named once
        // however many of its fields are filled, and anything else is the slot
        let value = match field.strip_prefix("tp_as_") {
            Some(table) => format!("&{type_name}_{table}"),
            None => format!("{type_name}_{}", name.trim_matches('_')),
        };
        let _ = writeln!(out, "             .{field} = {value},");
    }
    if COMPARISONS
        .iter()
        .any(|(name, _)| answers_slot(class, name))
    {
        let _ = writeln!(
            out,
            "             .tp_richcompare = {type_name}_richcompare,"
        );
    }
    if let Some(hash) = hash_slot_override(class) {
        let _ = writeln!(out, "             .tp_hash = {hash},");
    }
    // `__bool__` names the number table already; an arithmetic method without one
    // still needs it pointed at
    if !answers_slot(class, "__bool__") && !number_fields(class, type_name).is_empty() {
        let _ = writeln!(out, "             .tp_as_number = &{type_name}_number,");
    }
    out
}

fn iterator_slots(class: &ClassIr, type_name: &str) -> Vec<(String, String)> {
    if class.resume.is_none() {
        return Vec::new();
    }
    let mut slots = vec![
        (
            "Py_tp_iternext".to_string(),
            format!("{type_name}_iternext"),
        ),
        (
            "Py_tp_finalize".to_string(),
            format!("{type_name}_finalize"),
        ),
    ];
    if class
        .resume
        .as_ref()
        .is_some_and(|resume| resume.surface == Surface::Coroutine)
    {
        slots.push(("Py_am_await".to_string(), format!("{type_name}_await")));
    } else {
        slots.push(("Py_tp_iter".to_string(), "PyObject_SelfIter".to_string()));
    }
    slots
}

fn out_slot(buffer: &mut String) -> &mut String {
    buffer
}

/// the function a `CallNative` names, if this module emits it
fn native_callee<'a>(
    module: &'a ModuleIr,
    owner: Option<&str>,
    name: &str,
) -> Option<&'a Function> {
    match owner {
        None => module
            .functions
            .iter()
            .find(|function| function.name == name),
        Some(owner) => module
            .classes
            .iter()
            .find(|class| class.name == owner)
            .and_then(|class| class.methods.iter().find(|method| method.name == name)),
    }
}

/// the C type a register of this type is emitted as
///
/// this is the only renderer there is, and it takes the module because an instance's
/// answer depends on which classes the module lays out. an `RType` deliberately has no
/// method of its own — one existed, produced a placeholder for the instance case that
/// nothing ever replaced, and the undefined struct it named went unnoticed for as long
/// as the C compiler treated an incompatible pointer as a warning
fn ctype(module: &ModuleIr, ty: &RType) -> String {
    match ty {
        RType::Instance { class, .. } => match class_named(module, class) {
            // a class whose fields sit past a base's instance is *not* its field struct:
            // the two addresses differ, and only the object pointer identifies the value
            Some(owner) if !external_storage(module, owner) => {
                format!("{} *", owner.struct_name(module.name.dotted()))
            }
            _ => "PyObject *".to_string(),
        },
        // one header type for every element type: the element's width is known
        // statically at each op, so a struct per element gains nothing
        RType::Array(_) => "ByArrayHeader *".to_string(),
        RType::Primitive(primitive) => match primitive {
            Primitive::Object
            | Primitive::Str
            | Primitive::Bytes
            | Primitive::List
            | Primitive::Dict
            | Primitive::Tuple => "PyObject *".to_string(),
            Primitive::Int => "ByTagged".to_string(),
            Primitive::Fixed(width) => width.ctype().to_string(),
            Primitive::Float => "double".to_string(),
            Primitive::Bool | Primitive::Bit | Primitive::None => "char".to_string(),
        },
        RType::Tuple(items) => format!("ByTuple{}", tuple_mangle(items)),
    }
}

/// unbox `expr` into the representation `ty`, checking that it is one
///
/// this is the module boundary: everything crossing it comes from python and may
/// be anything at all, so the representation invariant has to be *established*
/// here rather than assumed
fn unbox_checked(module: &ModuleIr, ty: &RType, expr: &str) -> String {
    match ty {
        RType::Instance { class, .. } => {
            match module
                .classes
                .iter()
                .find(|candidate| candidate.name == *class)
            {
                // the cast is whatever a register of this type *is*, which for a class
                // appending to a base's layout is the object rather than its fields
                Some(owner) => format!(
                    "({})By_UnboxInstance({expr}, (PyTypeObject *){}_OBJ)",
                    ctype(module, ty),
                    owner.type_name(module.name.dotted())
                ),
                // a class with no emitted layout is represented as a plain object
                None => unbox_expr(ty, expr),
            }
        }
        _ => unbox_expr(ty, expr),
    }
}

/// check that `expr` has the representation `ty`, answering with it unretained
///
/// only the narrowings that are a *test* have one of these, because only those hand
/// back the very object they were given: a `str`, a `list` and a native instance are
/// all a `PyObject *` already, where an `int` or a `float` is a machine value the
/// unbox has to build. `None` for anything else, which leaves the caller on the
/// owning path — and a register whose unbox changes the representation can then
/// never be silently taken as a borrow
fn check_expr(module: &ModuleIr, ty: &RType, expr: &str) -> Option<String> {
    match ty {
        RType::Primitive(Primitive::Str) => Some(format!("By_CheckStr({expr})")),
        RType::Primitive(Primitive::List) => Some(format!("By_CheckList({expr})")),
        RType::Instance { class, .. } => module
            .classes
            .iter()
            .find(|candidate| candidate.name == *class)
            .map(|owner| {
                format!(
                    "({})By_CheckInstance({expr}, (PyTypeObject *){}_OBJ)",
                    ctype(module, ty),
                    owner.type_name(module.name.dotted())
                )
            }),
        _ => None,
    }
}

/// unbox `expr` into the representation `ty`
fn unbox_expr(ty: &RType, expr: &str) -> String {
    match ty {
        RType::Primitive(Primitive::Int) => format!("By_UnboxInt({expr})"),
        RType::Primitive(Primitive::Float) => format!("By_UnboxFloat({expr})"),
        RType::Primitive(Primitive::Bool) => format!("By_UnboxBool({expr})"),
        RType::Primitive(Primitive::None) => format!("By_UnboxNone({expr})"),
        RType::Primitive(Primitive::Str) => format!("By_UnboxStr({expr})"),
        RType::Primitive(Primitive::List) => format!("By_UnboxList({expr})"),
        _ => format!("By_NewRef({expr})"),
    }
}

/// box `expr`, which the caller already owns
///
/// an unboxed value becomes a fresh object; an already-boxed one is handed on as
/// it is. taking a *second* reference here is the difference between a correct
/// wrapper and one that leaks on every call
fn box_owned(ty: &RType, expr: &str) -> String {
    match ty {
        RType::Primitive(Primitive::Int) => format!("By_BoxInt({expr})"),
        RType::Primitive(Primitive::Float) => format!("By_BoxFloat({expr})"),
        RType::Primitive(Primitive::Bool | Primitive::Bit) => format!("By_BoxBool({expr})"),
        RType::Primitive(Primitive::None) => "By_BoxNone()".to_string(),
        RType::Tuple(items) => {
            format!("ByTuple{}_box_owned({expr})", tuple_mangle(items))
        }
        _ => format!("(PyObject *)({expr})"),
    }
}

/// box `expr`, which the caller only borrows — a field read, say
fn box_borrowed(ty: &RType, expr: &str) -> String {
    match ty {
        RType::Primitive(Primitive::Int) => format!("By_BoxInt({expr})"),
        RType::Primitive(Primitive::Float) => format!("By_BoxFloat({expr})"),
        RType::Primitive(Primitive::Bool | Primitive::Bit) => format!("By_BoxBool({expr})"),
        RType::Primitive(Primitive::None) => "By_BoxNone()".to_string(),
        RType::Tuple(items) => format!("ByTuple{}_box({expr})", tuple_mangle(items)),
        _ => format!("By_NewRef((PyObject *)({expr}))"),
    }
}

/// the value a place of this representation holds before anything has been written
/// to it, and the one a fallible function hands back on its error path
///
/// [`RType::undefined`] is written as an *initializer*, and a struct's `{0}` is only
/// one where a declaration takes it. a `return` or an assignment needs the compound
/// literal, which is that initializer with the type named in front of it
fn undefined(module: &ModuleIr, ty: &RType) -> String {
    match ty {
        RType::Tuple(_) => format!("({}){}", ctype(module, ty), ty.undefined()),
        other => other.undefined().to_string(),
    }
}

fn signature(module: &ModuleIr, function: &Function) -> String {
    let params = if function.param_count == 0 {
        "void".to_string()
    } else {
        function
            .params()
            .iter()
            .enumerate()
            .map(|(index, decl)| {
                format!("{} {}", ctype(module, &decl.ty), local(RegisterId(index)))
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    format!(
        "static {} {}({})",
        ctype(module, &function.ret),
        function.native_symbol(module.name.dotted()),
        params
    )
}

fn local(id: RegisterId) -> String {
    format!("r{}", id.0)
}

/// which registers the frame owns, and so must release on the way out
///
/// every register except a parameter the body never writes: the caller owns an
/// argument for the duration of the call
fn owned_registers(function: &Function) -> Vec<bool> {
    let mut owned: Vec<bool> = (0..function.registers.len())
        .map(|index| index >= function.param_count)
        .collect();
    for block in &function.blocks {
        for op in &block.ops {
            if let Some(dest) = op.dest()
                && let Some(slot) = owned.get_mut(dest.index())
            {
                *slot = true;
            }
        }
    }
    // a borrow is the one thing that overrides a write: the borrow pass proved the
    // value outlives every use, so the frame must not release it
    for (index, decl) in function.registers.iter().enumerate() {
        if decl.borrowed
            && let Some(slot) = owned.get_mut(index)
        {
            *slot = false;
        }
    }
    owned
}

fn emit_function(module: &ModuleIr, function: &Function) -> String {
    let mut out = line_directive(module, function.range);
    let _ = writeln!(out, "{} {{", signature(module, function));
    let owned = owned_registers(function);

    for (index, decl) in function.registers.iter().enumerate() {
        // a parameter is already declared — it is the signature — and it arrives
        // bound, so its byte starts at 1. it only has one at all because `del n` can
        // unbind a parameter like any other local
        if index >= function.param_count {
            let _ = writeln!(
                out,
                "    {} {} = {};",
                ctype(module, &decl.ty),
                local(RegisterId(index)),
                decl.ty.undefined()
            );
        }
        // a local some path reaches without writing carries the answer to whether it
        // was written. it starts at 0 because no path has written it yet
        if decl.may_be_unassigned {
            let _ = writeln!(
                out,
                "    char {} = {};",
                by_ir::function::RegisterDecl::presence(RegisterId(index)),
                u8::from(index < function.param_count)
            );
        }
    }

    // a parameter the body reassigns has to be owned, or the first write would
    // release a reference the caller still holds
    for (index, decl) in function.params().iter().enumerate() {
        if owned.get(index).copied() == Some(true)
            && let Some(retain) = inc_ref(&decl.ty, &local(RegisterId(index)))
        {
            let _ = writeln!(out, "    {retain}");
        }
    }
    if !function.registers.is_empty() {
        out.push('\n');
    }

    for (index, block) in function.blocks.iter().enumerate() {
        // a label immediately before a declaration is invalid in C89 and merely
        // ugly later; the empty statement keeps it valid everywhere
        let _ = writeln!(out, "b{index}: ;");
        out.push_str(&line_directive(module, block.range));
        for op in &block.ops {
            out.push_str(&guard_unassigned(
                function,
                &op.operands(),
                block.error_target,
            ));
            out.push_str(&emit_op(module, function, op, block.error_target));
            out.push_str(&mark_assigned(function, op));
        }
        out.push_str(&guard_unassigned(
            function,
            &block.terminator.operands(),
            block.error_target,
        ));
        out.push_str(&emit_terminator(
            module,
            function,
            &block.terminator,
            block.owned_at_exit.as_deref(),
        ));
    }

    if function.convention.can_fail() {
        // the error label is shared by every block, so it cannot use a
        // block-specific live set
        out.push_str("by_error: ;\n");
        // a coroutine's body raising `StopIteration` is forging the exhaustion the
        // await protocol reports with one, and python replaces it with `RuntimeError`
        // as the frame leaves. this body has no frame to leave — an `await` of it is a
        // plain call — so the conversion happens on the way out instead, which is the
        // same point in the same program
        if function.coroutine_body.is_some() {
            let _ = writeln!(
                out,
                "    By_ConvertStopIteration({});",
                frame_kind(by_ir::function::Surface::Coroutine)
            );
        }
        out.push_str(&emit_cleanup(function, "    ", None, None));
        let _ = writeln!(out, "    return {};", undefined(module, &function.ret));
    }

    out.push_str("}\n");
    out
}

/// release the registers an exit in `block` owns
///
/// `live` is the refcount pass's answer for this block, when it ran. without it
/// every owned register is released, which is correct and merely wasteful
///
/// `moved` is a register whose reference the exit is handing to the caller instead
/// of releasing — see [`returned_by_move`]
fn emit_cleanup(
    function: &Function,
    indent: &str,
    live: Option<&[RegisterId]>,
    moved: Option<RegisterId>,
) -> String {
    let owned = owned_registers(function);
    let mut out = String::new();
    for (index, decl) in function.registers.iter().enumerate() {
        if owned.get(index).copied() != Some(true) {
            continue; // a borrowed parameter belongs to the caller
        }
        if moved == Some(RegisterId(index)) {
            continue; // the caller is taking this one
        }
        if let Some(live) = live
            && !live.contains(&RegisterId(index))
        {
            continue; // provably dead here
        }
        if let Some(release) = dec_ref(&decl.ty, &local(RegisterId(index))) {
            let _ = writeln!(out, "{indent}{release}");
        }
    }
    out
}

/// the register a `return` hands its own reference to the caller, rather than
/// retaining a second one and releasing its first
///
/// the reference a frame already holds is exactly what the caller has to be given,
/// so the retain and the release that follows it cancel — and they are not free.
/// on a small method called in a loop the pair is *fifteen per cent* of the call:
/// `By_IncRefTagged` and `By_DecRefTagged` each branch on the tag, and each carries
/// a `Py_INCREF`/`Py_DECREF` whose slow half is a call, which is enough to keep the
/// c compiler from inlining a body that would otherwise be two instructions.
///
/// moving is only sound where the frame was going to release that very register on
/// this path, so this asks the same three questions [`emit_cleanup`] asks and
/// answers `None` to anything else. a borrowed register, a parameter the caller
/// still owns, and one the refcount pass proved dead all keep the retain.
///
/// nothing runs between the two in the first place except other releases, and those
/// can run `__del__` — but a moved reference is one the frame never drops, so the
/// value it names is held across them by the very reference being handed on
fn returned_by_move(
    function: &Function,
    value: &Value,
    live: Option<&[RegisterId]>,
) -> Option<RegisterId> {
    let Value::Register(id) = value else {
        return None;
    };
    if !owned_registers(function).get(id.index()).copied()? {
        return None;
    }
    if let Some(live) = live
        && !live.contains(id)
    {
        return None;
    }
    let decl = function.register(*id)?;
    // the retain is written from the *return* type and the release from the
    // register's, so they only cancel where the two agree
    (decl.ty == function.ret && decl.ty.is_refcounted()).then_some(*id)
}

/// the C type of an array operand's elements
fn element_ctype(module: &ModuleIr, function: &Function, array: &Value) -> String {
    match operand_type(function, array) {
        Some(RType::Array(element)) => ctype(module, &element),
        _ => "char".to_string(),
    }
}

/// the C type of an array operand itself
fn array_ctype(module: &ModuleIr, function: &Function, array: &Value) -> String {
    operand_type(function, array).map_or_else(|| "void *".to_string(), |ty| ctype(module, &ty))
}

/// the representation of an operand, where it is a register
fn operand_type(function: &Function, value: &Value) -> Option<RType> {
    match value {
        Value::Register(id) => function.register(*id).map(|decl| decl.ty.clone()),
        other => other.immediate_type(),
    }
}

fn dec_ref(ty: &RType, expr: &str) -> Option<String> {
    if !ty.is_refcounted() {
        return None;
    }
    Some(match ty {
        RType::Primitive(Primitive::Int) => format!("By_DecRefTagged({expr});"),
        RType::Tuple(items) => items
            .iter()
            .enumerate()
            .filter_map(|(index, item)| dec_ref(item, &format!("{expr}.f{index}")))
            .collect::<Vec<_>>()
            .join(" "),
        // the buffer carries its own count, so releasing it is the same shape as
        // releasing anything else — which is the whole reason it carries one
        RType::Array(_) => format!("By_ArrayDecRef((ByArrayHeader *){expr});"),
        RType::Primitive(_) | RType::Instance { .. } => format!("Py_XDECREF({expr});"),
    })
}

fn inc_ref(ty: &RType, expr: &str) -> Option<String> {
    if !ty.is_refcounted() {
        return None;
    }
    Some(match ty {
        RType::Primitive(Primitive::Int) => format!("By_IncRefTagged({expr});"),
        RType::Tuple(items) => items
            .iter()
            .enumerate()
            .filter_map(|(index, item)| inc_ref(item, &format!("{expr}.f{index}")))
            .collect::<Vec<_>>()
            .join(" "),
        RType::Array(_) => format!("By_ArrayIncRef((ByArrayHeader *){expr});"),
        RType::Primitive(_) | RType::Instance { .. } => format!("Py_XINCREF({expr});"),
    })
}

/// the `Py_*` constant naming a comparison to the rich-compare protocol
fn rich_compare_op(op: CmpOp) -> &'static str {
    match op {
        CmpOp::Eq => "Py_EQ",
        CmpOp::Ne => "Py_NE",
        CmpOp::Lt => "Py_LT",
        CmpOp::Le => "Py_LE",
        CmpOp::Gt => "Py_GT",
        CmpOp::Ge => "Py_GE",
    }
}

/// a C expression for an operand. registers are read as-is; a string literal is
/// materialized where it is used
fn value_expr(value: &Value) -> String {
    match value {
        Value::Register(id) => local(*id),
        Value::Int(v) => format!("By_ShortFrom({v})"),
        Value::Fixed(v) => format!("INT64_C({v})"),
        Value::Float(v) => format!("{v:?}"),
        Value::Bool(v) | Value::Bit(v) => i32::from(*v).to_string(),
        Value::None => "0".to_string(),
        // the interning table is keyed by content, so the index is derivable from
        // the literal alone — `LITERALS` is set once per module emission
        Value::Str(v) => {
            LITERALS.with_borrow(|literals| match literals.iter().position(|l| l == v) {
                Some(index) => format!("by_str{index}"),
                // only reachable if a literal appeared after collection, which the
                // collector's single pass rules out
                None => format!("PyUnicode_FromStringAndSize({})", c_string_sized(v)),
            })
        }
        Value::Bytes(v) => BYTE_LITERALS.with_borrow(|literals| {
            match literals.iter().position(|l| l.as_ref() == v.as_ref()) {
                Some(index) => format!("by_bytes{index}"),
                None => format!(
                    "PyBytes_FromStringAndSize({}, {})",
                    c_byte_string(v),
                    v.len()
                ),
            }
        }),
    }
}

thread_local! {
    /// the module's interned string literals, for the duration of one emission
    static LITERALS: std::cell::RefCell<Vec<String>> =
        const { std::cell::RefCell::new(Vec::new()) };
    /// the module's bytes literals, likewise
    static BYTE_LITERALS: std::cell::RefCell<Vec<Box<[u8]>>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// `value` as a `PyObject *`
///
/// an instance register is a struct pointer, which C treats as a different type from
/// `PyObject *` however identical the address is
fn object_expr(function: &Function, value: &Value) -> String {
    let expr = value_expr(value);
    match function.value_type(value) {
        Some(RType::Instance { .. }) => format!("(PyObject *)({expr})"),
        _ => expr,
    }
}

/// the entries the emitted method table carries *before* the class's own methods
///
/// the table is these followed by [`ClassIr::table_methods`], which drops `__new__` and the
/// halves of a property. anything that wants to *point* at an entry has to count the same
/// way — `MakeClosure` takes the address of one — so both the emission and the index read
/// this one list rather than each deriving the shape from the class. they did not, and the
/// index was taken from the unfiltered `methods` with no prefix at all: right only for a
/// class that is neither generic nor resumable and whose methods are all in the table, which
/// is every closure environment built so far and is not a rule anything states
fn synthetic_table_entries(class: &by_ir::function::ClassIr, type_name: &str) -> Vec<String> {
    let mut out = Vec::new();
    if class.generic {
        out.push(format!(
            "    {{\"__class_getitem__\", (PyCFunction){type_name}_class_getitem, METH_O | METH_CLASS, NULL}},"
        ));
    }
    if class.resume.is_some() {
        for (name, symbol) in [("send", "send"), ("throw", "throw"), ("close", "close")] {
            out.push(format!(
                "    {{\"{name}\", (PyCFunction)(void(*)(void)){type_name}_{symbol}, METH_FASTCALL, NULL}},"
            ));
        }
    }
    // an async generator's cleanup is asked for through an *awaitable*, which is the same
    // shape `__anext__` hands back
    if class
        .resume
        .as_ref()
        .is_some_and(|resume| resume.surface == Surface::AsyncGenerator)
    {
        for (name, symbol) in [
            ("aclose", "aclose"),
            ("asend", "do_asend"),
            ("athrow", "do_athrow"),
        ] {
            out.push(format!(
                "    {{\"{name}\", (PyCFunction)(void(*)(void)){type_name}_{symbol}, METH_FASTCALL, NULL}},"
            ));
        }
    }
    out
}

fn c_string(text: &str) -> String {
    c_byte_string(text.as_bytes())
}

/// a method table entry's `ml_doc`, which is `NULL` where the definition had no
/// docstring
///
/// both spellings where the two differ, because what `__doc__` holds is the literal on
/// python 3.12 and the cleaned form from 3.13 — the emitted C says the same thing
/// whichever interpreter builds it, and `BY_DOC` is where the version chooses.
///
/// chunked, because a docstring is the one piece of text here with no bound on its
/// length and MSVC stops a single literal at 16380 bytes
fn method_doc(function: &Function) -> String {
    let Some(raw) = &function.doc else {
        return "NULL".to_string();
    };
    let cleaned = cleaned_doc(raw);
    if cleaned == *raw {
        return c_string_chunked(raw);
    }
    format!(
        "BY_DOC({},\n{})",
        c_string_chunked(raw),
        c_string_chunked(&cleaned)
    )
}

/// a C string literal *and* the byte count it stands for, as one argument pair
///
/// every constructor that takes text has to be handed the length: a python string
/// may contain a NUL, and the C-string forms stop there. the count is of the utf-8
/// the literal encodes, which is what the escape above writes byte for byte
fn c_string_sized(text: &str) -> String {
    format!("{}, {}", c_string(text), text.len())
}

/// a C string literal for arbitrary bytes
///
/// the non-printing escape is octal with a fixed three digits, which C reads no
/// further than — so a digit following an escaped byte stays a digit
fn c_byte_string(bytes: &[u8]) -> String {
    let mut out = String::from("\"");
    for byte in bytes {
        match byte {
            b'"' => out.push_str("\\\""),
            b'\\' => out.push_str("\\\\"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            0x20..=0x7e => out.push(*byte as char),
            other => {
                let _ = write!(out, "\\{other:03o}");
            }
        }
    }
    out.push('"');
    out
}

/// the test that says an operation of this type failed
fn error_check(ty: &RType, expr: &str) -> String {
    match ty {
        RType::Array(_) => format!("{expr} == NULL"),
        RType::Primitive(Primitive::Int) => format!("{expr} == BY_INT_ERROR"),
        RType::Primitive(Primitive::Float) => {
            // the sentinel overlaps a valid double, so an error has to be
            // confirmed with the thread's exception state
            format!("{expr} == BY_FLOAT_ERROR && PyErr_Occurred()")
        }
        RType::Primitive(Primitive::Bool | Primitive::Bit | Primitive::None) => {
            format!("{expr} == 2")
        }
        RType::Tuple(_) => format!("PyErr_Occurred() != NULL && ((void)({expr}), 1)"),
        RType::Primitive(_) | RType::Instance { .. } => format!("{expr} == NULL"),
    }
}

/// assign an already-owned value into a register, releasing what was there
///
/// the value is computed into a temporary *before* the old one is released,
/// because the destination is frequently also an operand — `acc = acc + n` would
/// otherwise free `acc` and then read it
fn assign_owned(module: &ModuleIr, function: &Function, dest: RegisterId, expr: &str) -> String {
    let Some(decl) = function.register(dest) else {
        return String::new();
    };
    let target = local(dest);
    match dec_ref(&decl.ty, &target) {
        Some(release) => format!(
            "    {{ {ctype} by_t = {expr}; {release} {target} = by_t; }}\n",
            ctype = ctype(module, &decl.ty)
        ),
        None => format!("    {target} = {expr};\n"),
    }
}

/// as [`assign_owned`], for an operation that can fail: the error edge is taken
/// *before* the destination is touched
///
/// a failing operation must not rebind its destination. python leaves the name bound
/// to what it was, and an `except` handler in the same function goes on to read it —
/// so releasing and storing first hands that handler a register the operation never
/// wrote, and a released value it still believes it owns
fn assign_checked(
    module: &ModuleIr,
    function: &Function,
    dest: RegisterId,
    expr: &str,
    error_target: Option<BlockId>,
) -> String {
    let Some(decl) = function.register(dest) else {
        return String::new();
    };
    format!(
        "    {{ {ctype} by_t = {expr};\n{}",
        commit_checked(function, dest, error_target),
        ctype = ctype(module, &decl.ty),
    )
}

/// the tail of an operation that has computed its result into `by_t`: the error
/// edge, then the release and the store
///
/// the operations that build an argument vector emit their own block, so they reach
/// this point rather than going through [`assign_checked`]. the rule is the same one
fn commit_checked(function: &Function, dest: RegisterId, error_target: Option<BlockId>) -> String {
    let Some(decl) = function.register(dest) else {
        return String::new();
    };
    let target = local(dest);
    let release = dec_ref(&decl.ty, &target).map_or(String::new(), |release| format!("{release} "));
    format!(
        "      if (BY_UNLIKELY({check})) goto {label};\n      {release}{target} = by_t; }}\n",
        check = error_check(&decl.ty, "by_t"),
        label = error_label(error_target),
    )
}

/// open a block that has resolved the name `str` into `by_fn`
///
/// this is the same resolution [`Op::CallPython`] emits, through a memo of the same
/// shape: the name is what a module rebinds, so the answer is only kept for as long
/// as no namespace has been written to. what the operations that use it differ in is
/// only what they then do with the answer — the caller closes the block itself, and
/// owes `by_fn` a release
fn resolve_str(error_target: Option<BlockId>) -> String {
    let label = error_label(error_target);
    format!(
        "    {{ static PyObject *by_g_str = NULL; PyObject *by_fn = NULL;\n      \
         if (by_g_str == NULL) by_g_str = By_InternedStr(\"str\", 3);\n      \
         if (by_g_str == NULL) goto {label};\n      \
         static ByGlobalSite by_gs_str = BY_GLOBAL_SITE_INIT;\n      \
         by_fn = By_LookupGlobalSite(&by_gs_str, by_module_dict, by_g_str);\n      \
         if (by_fn == NULL) goto {label};\n"
    )
}

/// an operation on this module's own namespace, keyed by an interned name
///
/// the write and the delete want the name in exactly the form the read does — an
/// interned `str` held in a static, so the key carries its hash — and both answer
/// with a status rather than a value
fn global_namespace_op(
    module: &ModuleIr,
    function: &Function,
    dest: RegisterId,
    name: &str,
    call: &dyn Fn(&str) -> String,
    error_target: Option<BlockId>,
) -> String {
    let slot = format!("by_g_{}", mangle(name));
    let mut out = format!("    {{ static PyObject *{slot} = NULL;\n");
    let _ = writeln!(
        out,
        "      if ({slot} == NULL) {slot} = By_InternedStr({});",
        c_string_sized(name)
    );
    let _ = writeln!(
        out,
        "      if ({slot} == NULL) goto {};",
        error_label(error_target)
    );
    out.push_str(&assign_checked(
        module,
        function,
        dest,
        &call(&slot),
        error_target,
    ));
    out.push_str("    }\n");
    out
}

/// the tests that turn a read of an unwritten local into `UnboundLocalError`
///
/// every read of a flagged register is guarded, not only the ones the analysis found
/// reachable while unwritten: a read after a write finds the byte set, so guarding it
/// costs a predicted branch and asks nothing of the emitter about control flow
fn guard_unassigned(
    function: &Function,
    values: &[&Value],
    error_target: Option<BlockId>,
) -> String {
    let registers: Vec<RegisterId> = values
        .iter()
        .filter_map(|value| match value {
            Value::Register(id) => Some(*id),
            _ => None,
        })
        .collect();
    guard_unassigned_registers(function, &registers, error_target)
}

/// as [`guard_unassigned`], for a register an operation names as a place rather than
/// reading as a value — the destination of a `del`
fn guard_unassigned_registers(
    function: &Function,
    registers: &[RegisterId],
    error_target: Option<BlockId>,
) -> String {
    let mut out = String::new();
    for id in registers {
        let Some(decl) = function.register(*id) else {
            continue;
        };
        if !decl.may_be_unassigned {
            continue;
        }
        let _ = writeln!(
            out,
            "    if (BY_UNLIKELY(!{})) {{ By_RaiseUnboundLocal({}); goto {}; }}",
            by_ir::function::RegisterDecl::presence(*id),
            c_string(decl.name.as_deref().unwrap_or("")),
            error_label(error_target)
        );
    }
    out
}

/// the assignment that records a flagged register as written
///
/// `del` is the one operation whose destination comes out *un*bound, and it writes
/// the byte itself — so it is excluded here rather than having its own store undone
fn mark_assigned(function: &Function, op: &Op) -> String {
    let Some(dest) = op.dest().filter(|_| op.unbinds().is_none()) else {
        return String::new();
    };
    if !function
        .register(dest)
        .is_some_and(|decl| decl.may_be_unassigned)
    {
        return String::new();
    }
    format!(
        "    {} = 1;\n",
        by_ir::function::RegisterDecl::presence(dest)
    )
}

fn emit_op(
    module: &ModuleIr,
    function: &Function,
    op: &Op,
    error_target: Option<BlockId>,
) -> String {
    match op {
        Op::Assign { dest, src } => {
            let Some(decl) = function.register(*dest) else {
                return String::new();
            };
            let expr = value_expr(src);
            let mut out = String::new();
            // an operand is always borrowed — a literal is a static and a register is
            // the frame's — so an assignment never has an error edge of its own.
            //
            // a destination that holds no reference is a plain store, and so is a
            // borrowed one, for the other reason: the borrow pass proved the source
            // goes on holding the value across every use, so there is nothing to
            // retain and nothing here that was ever owned to give back
            if decl.borrowed {
                let _ = writeln!(out, "    {} = {expr};", local(*dest));
            } else if !decl.ty.is_refcounted() {
                out.push_str(&assign_owned(module, function, *dest, &expr));
            } else {
                // copying a register: retain the new value before releasing the
                // old, so `a = a` is safe
                let temp = format!("{}_t", local(*dest));
                let _ = writeln!(out, "    {{ {} {temp} = {expr};", ctype(module, &decl.ty));
                if let Some(retain) = inc_ref(&decl.ty, &temp) {
                    let _ = writeln!(out, "      {retain}");
                }
                if let Some(release) = dec_ref(&decl.ty, &local(*dest)) {
                    let _ = writeln!(out, "      {release}");
                }
                let _ = writeln!(out, "      {} = {temp}; }}", local(*dest));
            }
            out
        }
        Op::IntBinary { dest, op, lhs, rhs } => {
            // a fixed width is plain machine arithmetic: the C compiler emits one
            // instruction, and there is no tag to strip
            if let Some(RType::Primitive(Primitive::Fixed(_))) = operand_type(function, lhs) {
                let (l, r) = (value_expr(lhs), value_expr(rhs));
                // the three that can leave the width are checked. the builtin is one
                // instruction and a branch that never goes anywhere, which measured as
                // free — so it is unconditional rather than a case the frontend has to
                // prove away, and the representation is sound wherever it is chosen
                let expr = match op {
                    BinOp::Add | BinOp::Sub | BinOp::Mul => {
                        let builtin = match op {
                            BinOp::Sub => "__builtin_sub_overflow",
                            BinOp::Mul => "__builtin_mul_overflow",
                            _ => "__builtin_add_overflow",
                        };
                        return format!(
                            "    if (BY_UNLIKELY({builtin}({l}, {r}, &{}))) {{ \
                             PyErr_SetString(PyExc_OverflowError, \
                             \"machine integer overflow\"); goto {}; }}\n",
                            local(*dest),
                            error_label(error_target)
                        );
                    }
                    BinOp::BitAnd => format!("({l} & {r})"),
                    BinOp::BitOr => format!("({l} | {r})"),
                    BinOp::BitXor => format!("({l} ^ {r})"),
                    BinOp::Shl => format!("({l} << {r})"),
                    BinOp::Shr => format!("({l} >> {r})"),
                    // division by zero still has to raise, so it keeps a helper
                    BinOp::FloorDiv => format!("By_FixedFloorDiv({l}, {r})"),
                    BinOp::Mod => format!("By_FixedMod({l}, {r})"),
                    BinOp::TrueDiv | BinOp::Pow => {
                        return String::new();
                    }
                };
                return if matches!(op, BinOp::FloorDiv | BinOp::Mod) {
                    assign_checked(module, function, *dest, &expr, error_target)
                } else {
                    assign_owned(module, function, *dest, &expr)
                };
            }
            let call = match op {
                BinOp::Add => "By_IntAdd",
                BinOp::Sub => "By_IntSub",
                BinOp::Mul => "By_IntMul",
                BinOp::FloorDiv => "By_IntFloorDiv",
                BinOp::Mod => "By_IntMod",
                BinOp::TrueDiv => "By_IntTrueDiv",
                BinOp::Pow => "By_IntPow",
                BinOp::BitAnd => "By_IntAnd",
                BinOp::BitOr => "By_IntOr",
                BinOp::BitXor => "By_IntXor",
                BinOp::Shl => "By_IntShl",
                BinOp::Shr => "By_IntShr",
            };
            let expr = format!("{call}({}, {})", value_expr(lhs), value_expr(rhs));
            assign_checked(module, function, *dest, &expr, error_target)
        }
        Op::IsInstance { dest, src, class } => {
            let expr = format!("By_IsInstance({}, {})", value_expr(src), value_expr(class));
            assign_checked(module, function, *dest, &expr, error_target)
        }
        Op::MatchKey { dest, map, key } => {
            let expr = format!("By_MatchKey({}, {})", value_expr(map), value_expr(key));
            assign_checked(module, function, *dest, &expr, error_target)
        }
        Op::MatchRest { dest, map, keys } => {
            let expr = format!(
                "By_MatchRestMapping({}, {})",
                value_expr(map),
                value_expr(keys)
            );
            assign_checked(module, function, *dest, &expr, error_target)
        }
        Op::AsyncContext {
            dest,
            manager,
            exception,
        } => {
            let expr = match exception {
                Some(exception) => format!(
                    "By_AsyncExit({}, {})",
                    value_expr(manager),
                    value_expr(exception)
                ),
                None => format!("By_AsyncEnter({})", value_expr(manager)),
            };
            assign_checked(module, function, *dest, &expr, error_target)
        }
        Op::AsyncIter { dest, src, next } => {
            let expr = format!("By_AsyncIter({}, {})", value_expr(src), i32::from(*next));
            assign_checked(module, function, *dest, &expr, error_target)
        }
        Op::IsMapping { dest, src } => format!(
            "    {} = By_IsMatchMapping({});\n",
            local(*dest),
            value_expr(src)
        ),
        Op::MatchAttr {
            dest,
            subject,
            name,
            class,
            index,
            count,
        } => {
            let expr = match (name, class) {
                (Some(name), _) => format!(
                    "By_MatchAttr({}, By_InternedStr({}))",
                    value_expr(subject),
                    c_string_sized(name)
                ),
                (None, Some(class)) => format!(
                    "By_MatchPositional({}, {}, {index}, {count})",
                    value_expr(subject),
                    value_expr(class)
                ),
                // the frontend emits one form or the other
                (None, None) => return String::new(),
            };
            assign_checked(module, function, *dest, &expr, error_target)
        }
        Op::MethodStands {
            dest,
            src,
            class,
            method,
        } => {
            let Some(owner) = class_named(module, class) else {
                return format!("    {} = 0;\n", local(*dest));
            };
            format!(
                "    {} = By_MethodStands({}, {}_OBJ, &{});\n",
                local(*dest),
                value_expr(src),
                owner.type_name(module.name.dotted()),
                dispatch_licence(module, class, method),
            )
        }
        Op::DictShadows {
            dest,
            src,
            class,
            method,
        } => {
            // a class whose instances have no dict has nowhere to hold a shadowing
            // value, so the question is settled here and the arm taking the protocol
            // call becomes unreachable
            let Some(owner) =
                class_named(module, class).filter(|owner| instance_dict(module, owner))
            else {
                return format!("    {} = 0;\n", local(*dest));
            };
            let struct_name = owner.struct_name(module.name.dotted());
            let slot = format!("by_m_{}", mangle(method));
            let mut out = String::new();
            let _ = writeln!(out, "    {{ static PyObject *{slot} = NULL;");
            let _ = writeln!(
                out,
                "      if ({slot} == NULL) {slot} = By_InternedStr({});",
                c_string_sized(method)
            );
            let _ = writeln!(
                out,
                "      if ({slot} == NULL) goto {};",
                error_label(error_target)
            );
            // the receiver arrives as a pointer to the class's own struct, which begins
            // with the object header — so the cast is the whole of the conversion, and
            // no reference is taken for a test that stores nothing. the offset is written
            // out rather than read off the type, which keeps the whole test to two loads
            let _ = writeln!(
                out,
                "      {} = (char)By_DictShadowsAt((PyObject *){}, \
                 offsetof({struct_name}, {BY_DICT_MEMBER}), {slot}); }}",
                local(*dest),
                value_expr(src)
            );
            out
        }
        Op::IsMissing { dest, src } => format!(
            "    {} = (char)({} == By_MatchMissing());\n",
            local(*dest),
            value_expr(src)
        ),
        Op::MatchSlice {
            dest,
            sequence,
            start,
            after,
            rest,
        } => {
            let expr = if *rest {
                format!("By_MatchRest({}, {start}, {after})", value_expr(sequence))
            } else {
                format!("By_MatchFromEnd({}, {after})", value_expr(sequence))
            };
            assign_checked(module, function, *dest, &expr, error_target)
        }
        Op::IsSequence { dest, src } => format!(
            "    {} = By_IsMatchSequence({});\n",
            local(*dest),
            value_expr(src)
        ),
        Op::Contains {
            dest,
            value,
            container,
            negated,
        } => {
            let expr = format!(
                "By_Contains({}, {}, {})",
                value_expr(container),
                value_expr(value),
                i32::from(*negated)
            );
            assign_checked(module, function, *dest, &expr, error_target)
        }
        Op::Identity {
            dest,
            lhs,
            rhs,
            negated,
        } => format!(
            "    {} = (char)({} {} {});\n",
            local(*dest),
            value_expr(lhs),
            if *negated { "!=" } else { "==" },
            value_expr(rhs)
        ),
        Op::FloatObjectCompare { dest, op, lhs, rhs } => {
            // whichever side is the object is the one tested
            let reflected = function.value_type(lhs) == Some(RType::OBJECT);
            let call = match op {
                CmpOp::Eq => "By_FloatObjEq",
                CmpOp::Ne => "By_FloatObjNe",
                CmpOp::Lt => "By_FloatObjLt",
                CmpOp::Le => "By_FloatObjLe",
                CmpOp::Gt => "By_FloatObjGt",
                CmpOp::Ge => "By_FloatObjGe",
            };
            let suffix = if reflected { "Rev" } else { "" };
            let expr = format!("{call}{suffix}({}, {})", value_expr(lhs), value_expr(rhs));
            assign_checked(module, function, *dest, &expr, error_target)
        }
        Op::FloatObjectBinary { dest, op, lhs, rhs } => {
            // whichever side is the object is the one tested; the other stays in
            // its register and is boxed only if the test fails
            let reflected = function.value_type(lhs) == Some(RType::OBJECT);
            let call = match (op, reflected) {
                (BinOp::Add, false) => "By_FloatObjAdd",
                (BinOp::Sub, false) => "By_FloatObjSub",
                (BinOp::Mul, false) => "By_FloatObjMul",
                (BinOp::TrueDiv, false) => "By_FloatObjDiv",
                (BinOp::Add, true) => "By_ObjFloatAdd",
                (BinOp::Sub, true) => "By_ObjFloatSub",
                (BinOp::Mul, true) => "By_ObjFloatMul",
                (BinOp::TrueDiv, true) => "By_ObjFloatDiv",
                // the lowering only ever emits the four the runtime guards
                _ => return String::new(),
            };
            let expr = format!("{call}({}, {})", value_expr(lhs), value_expr(rhs));
            assign_checked(module, function, *dest, &expr, error_target)
        }
        Op::FloatBinary { dest, op, lhs, rhs } => {
            let (lhs, rhs) = (value_expr(lhs), value_expr(rhs));
            let expr = match op {
                BinOp::Add => format!("({lhs} + {rhs})"),
                BinOp::Sub => format!("({lhs} - {rhs})"),
                BinOp::Mul => format!("({lhs} * {rhs})"),
                BinOp::FloorDiv => format!("By_FloatFloorDiv({lhs}, {rhs})"),
                BinOp::Mod => format!("By_FloatMod({lhs}, {rhs})"),
                BinOp::TrueDiv => format!("By_FloatTrueDiv({lhs}, {rhs})"),
                BinOp::Pow => format!("By_FloatPow({lhs}, {rhs})"),
                // the bitwise operators have no float form; the frontend routes
                // them through the object protocol instead
                other => format!("(void)0 /* unreachable: float {} */", other.symbol()),
            };
            if op.can_fail() {
                assign_checked(module, function, *dest, &expr, error_target)
            } else {
                assign_owned(module, function, *dest, &expr)
            }
        }
        Op::ObjectBinary {
            dest,
            op,
            lhs,
            rhs,
            mutation,
        } => {
            let call = match (op, mutation) {
                (BinOp::Add, Mutation::Fresh) => "By_ObjAdd",
                (BinOp::Sub, Mutation::Fresh) => "By_ObjSub",
                (BinOp::Mul, Mutation::Fresh) => "By_ObjMul",
                (BinOp::FloorDiv, Mutation::Fresh) => "By_ObjFloorDiv",
                (BinOp::Mod, Mutation::Fresh) => "By_ObjMod",
                (BinOp::TrueDiv, Mutation::Fresh) => "By_ObjTrueDiv",
                (BinOp::Pow, Mutation::Fresh) => "By_ObjPow",
                (BinOp::BitAnd, Mutation::Fresh) => "By_ObjAnd",
                (BinOp::BitOr, Mutation::Fresh) => "By_ObjOr",
                (BinOp::BitXor, Mutation::Fresh) => "By_ObjXor",
                (BinOp::Shl, Mutation::Fresh) => "By_ObjShl",
                (BinOp::Shr, Mutation::Fresh) => "By_ObjShr",
                (BinOp::Add, Mutation::InPlace) => "By_ObjIAdd",
                (BinOp::Sub, Mutation::InPlace) => "By_ObjISub",
                (BinOp::Mul, Mutation::InPlace) => "By_ObjIMul",
                (BinOp::FloorDiv, Mutation::InPlace) => "By_ObjIFloorDiv",
                (BinOp::Mod, Mutation::InPlace) => "By_ObjIMod",
                (BinOp::TrueDiv, Mutation::InPlace) => "By_ObjITrueDiv",
                (BinOp::Pow, Mutation::InPlace) => "By_ObjIPow",
                (BinOp::BitAnd, Mutation::InPlace) => "By_ObjIAnd",
                (BinOp::BitOr, Mutation::InPlace) => "By_ObjIOr",
                (BinOp::BitXor, Mutation::InPlace) => "By_ObjIXor",
                (BinOp::Shl, Mutation::InPlace) => "By_ObjIShl",
                (BinOp::Shr, Mutation::InPlace) => "By_ObjIShr",
            };
            let expr = format!("{call}({}, {})", value_expr(lhs), value_expr(rhs));
            assign_checked(module, function, *dest, &expr, error_target)
        }
        Op::ObjectCompare { dest, op, lhs, rhs } => {
            let expr = format!(
                "By_ObjCompare({}, {}, {})",
                value_expr(lhs),
                value_expr(rhs),
                rich_compare_op(*op)
            );
            assign_checked(module, function, *dest, &expr, error_target)
        }
        Op::StrCompare { dest, op, lhs, rhs } => {
            let expr = format!(
                "By_StrCompare({}, {}, {})",
                value_expr(lhs),
                value_expr(rhs),
                rich_compare_op(*op)
            );
            assign_checked(module, function, *dest, &expr, error_target)
        }
        Op::Truthy { dest, src } => {
            let expr = format!("By_Truthy({})", value_expr(src));
            assign_checked(module, function, *dest, &expr, error_target)
        }
        Op::IntCompare { dest, op, lhs, rhs } => {
            if let Some(RType::Primitive(Primitive::Fixed(_))) = operand_type(function, lhs) {
                let (l, r) = (value_expr(lhs), value_expr(rhs));
                let symbol = match op {
                    CmpOp::Eq => "==",
                    CmpOp::Ne => "!=",
                    CmpOp::Lt => "<",
                    CmpOp::Le => "<=",
                    CmpOp::Gt => ">",
                    CmpOp::Ge => ">=",
                };
                // a tagged right-hand side is *not* a machine integer, and comparing the
                // two representations directly would read a pointer as a number
                if let Some(RType::Primitive(Primitive::Fixed(_))) = operand_type(function, rhs) {
                    return assign_owned(
                        module,
                        function,
                        *dest,
                        &format!("(char)({l} {symbol} {r})"),
                    );
                }
                let slow = match op {
                    CmpOp::Eq => "By_I64EqSlow",
                    CmpOp::Ne => "By_I64NeSlow",
                    CmpOp::Lt => "By_I64LtSlow",
                    CmpOp::Le => "By_I64LeSlow",
                    CmpOp::Gt => "By_I64GtSlow",
                    CmpOp::Ge => "By_I64GeSlow",
                };
                // the short case is spelled out here rather than hidden behind one call
                // that handles both: only the boxing half can fail, so writing them apart
                // is what keeps the error test off the straight line of a counting loop
                let short = assign_owned(
                    module,
                    function,
                    *dest,
                    &format!("(char)({l} {symbol} (int64_t) By_ShortValue({r}))"),
                );
                let boxed = assign_checked(
                    module,
                    function,
                    *dest,
                    &format!("{slow}({l}, {r})"),
                    error_target,
                );
                return format!(
                    "    if (BY_LIKELY(By_IsShort({r}))) {{\n{short}    }} else {{\n{boxed}    }}\n"
                );
            }
            let call = match op {
                CmpOp::Eq => "By_IntEq",
                CmpOp::Ne => "By_IntNe",
                CmpOp::Lt => "By_IntLt",
                CmpOp::Le => "By_IntLe",
                CmpOp::Gt => "By_IntGt",
                CmpOp::Ge => "By_IntGe",
            };
            let expr = format!("{call}({}, {})", value_expr(lhs), value_expr(rhs));
            assign_checked(module, function, *dest, &expr, error_target)
        }
        Op::FloatCompare { dest, op, lhs, rhs } => {
            let expr = format!(
                "(char)({} {} {})",
                value_expr(lhs),
                op.symbol(),
                value_expr(rhs)
            );
            assign_owned(module, function, *dest, &expr)
        }
        Op::Unary { dest, op, operand } => {
            let expr = match op {
                UnaryOp::Neg => match function.value_type(operand) {
                    Some(RType::Primitive(Primitive::Float)) => {
                        format!("(-{})", value_expr(operand))
                    }
                    Some(RType::Primitive(Primitive::Object)) => {
                        format!("By_ObjNeg({})", value_expr(operand))
                    }
                    _ => format!("By_IntNeg({})", value_expr(operand)),
                },
                UnaryOp::Not => format!("(char)(!{})", value_expr(operand)),
                UnaryOp::Invert => match function.value_type(operand) {
                    Some(RType::Primitive(Primitive::Object)) => {
                        format!("By_ObjInvert({})", value_expr(operand))
                    }
                    _ => format!("By_IntInvert({})", value_expr(operand)),
                },
            };
            if matches!(op, UnaryOp::Neg | UnaryOp::Invert)
                && matches!(
                    function.value_type(operand),
                    Some(RType::Primitive(Primitive::Int | Primitive::Object))
                )
            {
                assign_checked(module, function, *dest, &expr, error_target)
            } else {
                assign_owned(module, function, *dest, &expr)
            }
        }
        Op::CallNative {
            dest,
            owner,
            callee,
            args,
        } => {
            let target = native_callee(module, owner.as_deref(), callee);
            // an argument may be a *subclass* of what the callee declares — the
            // layouts line up, so the pointer is valid, but C wants to be told
            let args = args
                .iter()
                .enumerate()
                .map(|(index, arg)| {
                    match target
                        .and_then(|target| target.params().get(index))
                        .map(|decl| &decl.ty)
                    {
                        Some(ty @ RType::Instance { .. }) => {
                            format!("({}){}", ctype(module, ty), value_expr(arg))
                        }
                        _ => value_expr(arg),
                    }
                })
                .collect::<Vec<_>>()
                .join(", ");
            let symbol = match target {
                Some(target) => target.native_symbol(module.name.dotted()),
                None => format!("by_{}_{}", mangle(module.name.dotted()), mangle(callee)),
            };
            let call = format!("{symbol}({args})");
            let fallible = target.is_none_or(|target| target.convention.can_fail());
            match dest {
                Some(dest) if fallible => {
                    assign_checked(module, function, *dest, &call, error_target)
                }
                Some(dest) => assign_owned(module, function, *dest, &call),
                None => format!("    (void){call};\n"),
            }
        }
        Op::IntToFloat { dest, src } => {
            let expr = format!("By_TaggedToDouble({})", value_expr(src));
            assign_checked(module, function, *dest, &expr, error_target)
        }
        Op::Box { dest, src } => {
            let Some(src_ty) = function.value_type(src) else {
                return String::new();
            };
            let call = match src_ty {
                RType::Primitive(Primitive::Int) => format!("By_BoxInt({})", value_expr(src)),
                // to the *tagged* representation rather than to an object: an `int` is
                // what a machine integer widens to, and that is what `RType::INT` is
                RType::Primitive(Primitive::Fixed(_)) => {
                    format!("By_IntFromI64({})", value_expr(src))
                }
                RType::Primitive(Primitive::Float) => format!("By_BoxFloat({})", value_expr(src)),
                RType::Primitive(Primitive::Bool | Primitive::Bit) => {
                    format!("By_BoxBool({})", value_expr(src))
                }
                RType::Primitive(Primitive::None) => "By_BoxNone()".to_string(),
                // already an object, only with a narrower static class — which for an
                // instance is a struct pointer, and reaches `PyObject *` by a cast
                _ => format!("By_NewRef((PyObject *)({}))", value_expr(src)),
            };
            assign_checked(module, function, *dest, &call, error_target)
        }
        Op::Unbox { dest, src, to } => {
            if function.register(*dest).is_some_and(|decl| decl.borrowed)
                && let Some(check) = check_expr(module, to, &value_expr(src))
            {
                // the narrowing still has to happen — a borrow says nothing about
                // what type the value is — but the reference it would have taken is
                // waste: the borrow pass proved the source goes on holding the value
                // across every use, so there is nothing here that was ever owned
                return format!(
                    "    {{ {ctype} by_t = {check};\n      \
                     if (BY_UNLIKELY(by_t == NULL)) goto {label};\n      \
                     {target} = by_t; }}\n",
                    ctype = function
                        .register(*dest)
                        .map_or_else(String::new, |decl| ctype(module, &decl.ty)),
                    label = error_label(error_target),
                    target = local(*dest),
                );
            }
            let call = match to {
                RType::Primitive(Primitive::Int) => format!("By_UnboxInt({})", value_expr(src)),
                RType::Primitive(Primitive::Float) => format!("By_UnboxFloat({})", value_expr(src)),
                RType::Primitive(Primitive::Bool) => format!("By_UnboxBool({})", value_expr(src)),
                RType::Primitive(Primitive::None) => format!("By_UnboxNone({})", value_expr(src)),
                RType::Primitive(Primitive::Str) => format!("By_UnboxStr({})", value_expr(src)),
                RType::Primitive(Primitive::List) => {
                    format!("By_UnboxList({})", value_expr(src))
                }
                RType::Instance { .. } => unbox_checked(module, to, &value_expr(src)),
                // the frontend narrows to nothing else
                other => format!("({}) /* unreachable: unbox to {other} */", value_expr(src)),
            };
            assign_checked(module, function, *dest, &call, error_target)
        }
        Op::TupleBuild { dest, items } => {
            let Some(decl) = function.register(*dest) else {
                return String::new();
            };
            let fields = items
                .iter()
                .enumerate()
                .map(|(index, item)| format!(".f{index} = {}", value_expr(item)))
                .collect::<Vec<_>>()
                .join(", ");
            let expr = if items.is_empty() {
                format!("({}){{ 0 }}", ctype(module, &decl.ty))
            } else {
                format!("({}){{ {fields} }}", ctype(module, &decl.ty))
            };
            let mut out = assign_owned(module, function, *dest, &expr);
            if let Some(retain) = inc_ref(&decl.ty, &local(*dest)) {
                let _ = writeln!(out, "    {retain}");
            }
            out
        }
        Op::Extend {
            dest,
            container,
            source,
            mapping,
        } => {
            let expr = format!(
                "By_Extend({}, {}, {})",
                value_expr(container),
                value_expr(source),
                i32::from(*mapping)
            );
            assign_checked(module, function, *dest, &expr, error_target)
        }
        Op::CallUnpacked {
            dest,
            callee,
            args,
            kwargs,
        } => {
            let expr = format!(
                "PyObject_Call({}, {}, {})",
                value_expr(callee),
                value_expr(args),
                match kwargs {
                    Some(kwargs) => value_expr(kwargs),
                    None => "NULL".to_string(),
                }
            );
            assign_checked(module, function, *dest, &expr, error_target)
        }
        Op::ArrayNew { dest, items } => {
            let Some(decl) = function.register(*dest) else {
                return String::new();
            };
            let RType::Array(element) = &decl.ty else {
                return String::new();
            };
            let (ctype, width) = (ctype(module, &decl.ty), ctype(module, element));
            let mut out = String::new();
            let _ = writeln!(
                out,
                "    {{ ByArrayHeader *by_a = By_ArrayNew({}, sizeof({width}));",
                items.len()
            );
            let _ = writeln!(
                out,
                "      if (by_a == NULL) goto {};",
                error_label(error_target)
            );
            let _ = writeln!(out, "      by_a->len = {};", items.len());
            for (index, item) in items.iter().enumerate() {
                let _ = writeln!(
                    out,
                    "      (({width} *)By_ArrayItems(by_a))[{index}] = {};",
                    value_expr(item)
                );
            }
            let target = local(*dest);
            if let Some(release) = dec_ref(&decl.ty, &target) {
                let _ = writeln!(out, "      {release}");
            }
            let _ = writeln!(out, "      {target} = ({ctype})by_a; }}");
            out
        }
        Op::ArrayGet { dest, array, index } => {
            let Some(width) = function.register(*dest).map(|decl| ctype(module, &decl.ty)) else {
                return String::new();
            };
            let mut out = String::new();
            let _ = writeln!(
                out,
                "    {{ Py_ssize_t by_i = By_ArrayIndex((ByArrayHeader *){}, {});",
                value_expr(array),
                value_expr(index)
            );
            let _ = writeln!(
                out,
                "      if (by_i < 0) goto {};",
                error_label(error_target)
            );
            let _ = writeln!(
                out,
                "      {} = (({width} *)By_ArrayItems((ByArrayHeader *){}))[by_i]; }}",
                local(*dest),
                value_expr(array)
            );
            out
        }
        Op::ArraySet {
            dest,
            array,
            index,
            value,
        } => {
            let width = element_ctype(module, function, array);
            let mut out = String::new();
            let _ = writeln!(
                out,
                "    {{ Py_ssize_t by_i = By_ArrayIndex((ByArrayHeader *){}, {});",
                value_expr(array),
                value_expr(index)
            );
            let _ = writeln!(out, "      {} = by_i < 0 ? 2 : 0;", local(*dest));
            let _ = writeln!(
                out,
                "      if (by_i < 0) goto {};",
                error_label(error_target)
            );
            let _ = writeln!(
                out,
                "      (({width} *)By_ArrayItems((ByArrayHeader *){}))[by_i] = {}; }}",
                value_expr(array),
                value_expr(value)
            );
            out
        }
        Op::ArrayLen { dest, array } => {
            let raw = format!("((ByArrayHeader *){})->len", value_expr(array));
            let expr = match function.register(*dest).map(|decl| &decl.ty) {
                Some(RType::Primitive(Primitive::Fixed(_))) => raw,
                _ => format!("By_ShortFrom({raw})"),
            };
            format!("    {} = {expr};\n", local(*dest))
        }
        // no bounds check: the index is the lowering's own counter
        Op::DeleteItem {
            dest,
            container,
            index,
        } => {
            let expr = format!(
                "By_DeleteItem({}, {})",
                value_expr(container),
                value_expr(index)
            );
            assign_checked(module, function, *dest, &expr, error_target)
        }
        Op::DeleteAttr {
            dest,
            receiver,
            name: field,
        } => {
            let expr = format!(
                "By_DeleteAttr({}, {})",
                value_expr(receiver),
                c_string(field)
            );
            assign_checked(module, function, *dest, &expr, error_target)
        }
        Op::ArrayRead { dest, array, index } => format!(
            "    {} = (({} *)By_ArrayItems((ByArrayHeader *){}))[{}];\n",
            local(*dest),
            element_ctype(module, function, array),
            value_expr(array),
            // a proven source counter arrives tagged; the lowering's own is already
            // the machine integer this wants
            if function.value_type(index) == Some(RType::INT) {
                format!("By_ShortValue({})", value_expr(index))
            } else {
                value_expr(index)
            }
        ),
        Op::ArrayPush { dest, array, value } => {
            let width = element_ctype(module, function, array);
            let mut out = String::new();
            let _ = writeln!(
                out,
                "    {{ ByArrayHeader *by_a = By_ArrayGrow((ByArrayHeader *){}, sizeof({width}));",
                value_expr(array)
            );
            let _ = writeln!(
                out,
                "      if (by_a == NULL) goto {};",
                error_label(error_target)
            );
            let _ = writeln!(out, "      {} = 0;", local(*dest));
            let _ = writeln!(
                out,
                "      (({width} *)By_ArrayItems(by_a))[by_a->len++] = {};",
                value_expr(value)
            );
            // a grow may have moved the buffer, so the register has to follow it
            let _ = writeln!(
                out,
                "      {} = ({})by_a; }}",
                value_expr(array),
                array_ctype(module, function, array)
            );
            out
        }
        Op::ToTuple { dest, src } => {
            let expr = format!("PySequence_Tuple({})", value_expr(src));
            assign_checked(module, function, *dest, &expr, error_target)
        }
        Op::Unpack { dest, src, starred } => {
            let Some(decl) = function.register(*dest) else {
                return String::new();
            };
            let RType::Tuple(items) = &decl.ty else {
                return String::new();
            };
            // the runtime fills an array rather than the struct itself: aliasing a
            // struct as an array of its members is not something C promises
            let slots = items.len();
            let fields = (0..slots)
                .map(|index| format!(".f{index} = by_u[{index}]"))
                .collect::<Vec<_>>()
                .join(", ");
            let mut out = String::new();
            let _ = writeln!(out, "    {{ PyObject *by_u[{slots}];");
            let _ = writeln!(
                out,
                "      if (By_Unpack({}, by_u, {slots}, {}) < 0) goto {};",
                value_expr(src),
                starred.map_or("-1".to_string(), |index| index.to_string()),
                error_label(error_target)
            );
            let target = local(*dest);
            if let Some(release) = dec_ref(&decl.ty, &target) {
                let _ = writeln!(out, "      {release}");
            }
            let _ = writeln!(
                out,
                "      {target} = ({}){{ {fields} }}; }}",
                ctype(module, &decl.ty)
            );
            out
        }
        Op::TupleGet { dest, src, index } => {
            let Some(decl) = function.register(*dest) else {
                return String::new();
            };
            let expr = format!("{}.f{index}", value_expr(src));
            let mut out = String::new();
            // as for a copy: the borrow pass proved the tuple goes on holding the
            // element across every read of it, so there is nothing to retain and
            // nothing here that was ever owned to give back
            if decl.borrowed {
                let _ = writeln!(out, "    {} = {expr};", local(*dest));
                return out;
            }
            if let Some(retain) = inc_ref(&decl.ty, &expr) {
                let _ = writeln!(out, "    {retain}");
            }
            out.push_str(&assign_owned(module, function, *dest, &expr));
            out
        }
        Op::NewInstance {
            dest,
            class,
            fields,
        } => {
            let Some(owner) = module
                .classes
                .iter()
                .find(|candidate| candidate.name == *class)
            else {
                return String::new();
            };
            let struct_name = owner.struct_name(module.name.dotted());
            let type_name = owner.type_name(module.name.dotted());
            // `tp_alloc` answers with the *object*, which is only the field storage for
            // a class that owns its layout. one appending to a base keeps its fields
            // past that base's instance, so the two addresses differ and `fields_of` is
            // what knows by how much — writing through the object pointer would land on
            // the base's own data
            let mut out = format!(
                "    {{ PyTypeObject *by_type = (PyTypeObject *){type_name}_OBJ;\n\
                 \x20     PyObject *by_obj = by_type->tp_alloc(by_type, 0);\n"
            );
            // `tp_alloc` zeroes the block, so a field the loop below misses is NULL
            // rather than garbage — but every field is written
            let _ = writeln!(
                out,
                "      if (by_obj == NULL) goto {};",
                error_label(error_target)
            );
            let _ = writeln!(
                out,
                "      {{ {struct_name} *by_new = {};",
                fields_of(module, owner, "by_obj")
            );
            for (field, value) in owner.fields.iter().zip(fields) {
                // `tp_alloc` zeroes the block, so `None` leaves the field NULL —
                // which is what an unset cell is
                let Some(value) = value else { continue };
                // a field a base declared sits in that base's region rather than this
                // class's, so it is reached through the base's type — `by_new` is only
                // the storage of what this class adds
                let declared = field_owner(module, owner, &field.name);
                let target = if declared.name == owner.name {
                    "by_new".to_string()
                } else {
                    fields_of(module, declared, "by_obj")
                };
                if let Some(retain) = inc_ref(&field.ty, &value_expr(value)) {
                    let _ = writeln!(out, "      {retain}");
                }
                let _ = writeln!(
                    out,
                    "      {target}->{} = {};",
                    field.member(),
                    value_expr(value)
                );
                // the zero `tp_alloc` left says "never written", so a field filled here
                // has to say otherwise
                if field.optional {
                    let _ = writeln!(out, "      {target}->{} = 1;", field.presence());
                }
            }
            out.push_str("      }\n");
            if let Some(release) = dec_ref(&RType::OBJECT, &local(*dest)) {
                let _ = writeln!(out, "      {release}");
            }
            let destination = function
                .register(*dest)
                .map_or_else(|| "PyObject *".to_string(), |decl| ctype(module, &decl.ty));
            let _ = writeln!(out, "      {} = ({destination})by_obj; }}", local(*dest));
            out
        }
        Op::Enter { dest, manager } => {
            let call = format!("By_Enter({})", value_expr(manager));
            assign_checked(module, function, *dest, &call, error_target)
        }
        Op::ExitContext {
            dest,
            manager,
            exception,
        } => {
            // -1 means `__exit__` itself raised, which is the error path. 0 and 1 are
            // "re-raise" and "suppressed", and both are ordinary control flow
            let mut out = format!(
                "    {{ int by_r = By_ExitContext({}, {});\n",
                value_expr(manager),
                value_expr(exception)
            );
            let _ = writeln!(
                out,
                "      if (by_r < 0) goto {};",
                error_label(error_target)
            );
            let _ = writeln!(out, "      {} = (char)by_r; }}", local(*dest));
            out
        }
        Op::DelegateIter {
            dest,
            src,
            awaitable,
        } => {
            let call = if *awaitable {
                format!("By_AwaitIter({})", value_expr(src))
            } else {
                format!("By_GetIter({})", value_expr(src))
            };
            assign_checked(module, function, *dest, &call, error_target)
        }
        Op::DelegateStep { dest, inner, sent } => {
            let mut out = String::from("    { int by_done = 0;\n");
            let _ = writeln!(
                out,
                "      PyObject *by_t = By_DelegateStep({}, {}, &by_done);",
                value_expr(inner),
                value_expr(sent)
            );
            let _ = writeln!(
                out,
                "      if (BY_UNLIKELY(by_t == NULL)) goto {};",
                error_label(error_target)
            );
            let _ = writeln!(out, "      Py_XDECREF({}.f0);", local(*dest));
            let _ = writeln!(out, "      {}.f0 = by_t;", local(*dest));
            let _ = writeln!(out, "      {}.f1 = (char)by_done; }}", local(*dest));
            out
        }
        Op::RaiseWith { error, value } => format!(
            "    By_RaiseWith({}, {});\n    goto {};\n",
            error.c_name(),
            value_expr(value),
            error_label(error_target)
        ),
        // a finish is the one exit that leaves without an exception: the value is
        // written into the state object and the frame hands back nothing, so that
        // `am_send` can report a return for the price of a pointer read instead of
        // building a `StopIteration` for its caller to unpack again. whoever owes
        // python an exception builds it from there, in `By_TakeReturn`.
        //
        // it goes to the *function's* exit rather than to `error_target`: the frame's
        // cleanups have already run, and a finish that entered an enclosing `except`
        // would be a `return` the body caught
        Op::FinishFrame { value } => {
            debug_assert!(
                resumes(module, function),
                "a finish belongs to a resumable frame, and `{}` is not one",
                function.name
            );
            debug_assert!(
                error_target.is_none(),
                "a finish in `{}` still stands under a handler, which would catch it",
                function.name
            );
            let receiver = local(RegisterId(0));
            format!(
                "    {{ PyObject *by_t = By_NewRef({});\n\
                 \x20     Py_XDECREF({receiver}->by_returned);\n\
                 \x20     {receiver}->by_returned = by_t; }}\n\
                 \x20   goto {};\n",
                value_expr(value),
                error_label(None)
            )
        }
        Op::GetCell {
            dest,
            receiver,
            field,
            free,
            ..
        } => {
            let call = format!(
                "By_ReadCell({}->{}, {}, {})",
                value_expr(receiver),
                mangle_member(field),
                c_string(field),
                i32::from(*free)
            );
            assign_checked(module, function, *dest, &call, error_target)
        }
        Op::MakeClosure {
            dest,
            class,
            method,
            env,
        } => {
            let Some(owner) = module
                .classes
                .iter()
                .find(|candidate| candidate.name == *class)
            else {
                return String::new();
            };
            // the index has to be into the *emitted* table, which is the synthetic
            // entries followed by `table_methods` — not into `methods`, which still
            // holds the `__new__` and property halves the table leaves out
            let Some(index) = owner
                .table_methods()
                .position(|candidate| candidate.name == *method)
                .map(|position| {
                    position
                        + synthetic_table_entries(owner, &owner.type_name(module.name.dotted()))
                            .len()
                })
            else {
                return String::new();
            };
            let table = format!("{}_methods", owner.type_name(module.name.dotted()));
            let call = format!(
                "By_MakeClosure(&{table}[{index}], (PyObject *)({}))",
                value_expr(env)
            );
            assign_checked(module, function, *dest, &call, error_target)
        }
        Op::LoadGlobal { dest, name } => {
            let mut out = String::new();
            let slot = format!("by_g_{}", mangle(name));
            let _ = writeln!(out, "    {{ static PyObject *{slot} = NULL;");
            let _ = writeln!(
                out,
                "      if ({slot} == NULL) {slot} = By_InternedStr({});",
                c_string_sized(name)
            );
            let _ = writeln!(
                out,
                "      if ({slot} == NULL) goto {};",
                error_label(error_target)
            );
            let site = format!("by_gs_{}", mangle(name));
            let _ = writeln!(
                out,
                "      static ByGlobalSite {site} = BY_GLOBAL_SITE_INIT;"
            );
            let _ = writeln!(
                out,
                "      PyObject *by_t = By_LookupGlobalSite(&{site}, by_module_dict, {slot});"
            );
            out.push_str(&commit_checked(function, *dest, error_target));
            out
        }
        // `globals()`, which is the very dict the two halves above reach. calling the
        // builtin instead would answer about the *calling* frame, and a compiled
        // function pushes none — so it would hand back the caller's namespace, in
        // another module. `by_module_dict` is borrowed, and the register owns what it
        // holds, so the reference is taken here
        Op::ModuleDict { dest } => assign_owned(
            module,
            function,
            *dest,
            "(PyObject *)Py_NewRef(by_module_dict)",
        ),
        // the write half of `LoadGlobal`, reaching the same dict through the same
        // interned key — a register write would leave the module's binding alone
        Op::StoreGlobal { dest, name, value } => global_namespace_op(
            module,
            function,
            *dest,
            name,
            &|slot| {
                format!(
                    "By_StoreGlobal(by_module_dict, {slot}, {})",
                    value_expr(value)
                )
            },
            error_target,
        ),
        Op::DeleteGlobal { dest, name } => global_namespace_op(
            module,
            function,
            *dest,
            name,
            &|slot| format!("By_DeleteGlobal(by_module_dict, {slot})"),
            error_target,
        ),
        // `del x`: refuse if the name is already unbound, then release what it held
        // and put the slot back to the value an unwritten register starts at. that
        // value is release-safe for every representation — which is what lets the
        // cleanup on the way out go on releasing this register unconditionally
        Op::DeleteLocal { dest } => {
            let Some(decl) = function.register(*dest) else {
                return String::new();
            };
            let mut out = guard_unassigned_registers(function, &[*dest], error_target);
            // a borrowed register holds a reference the frame never took, so there is
            // nothing here to give back
            if !decl.borrowed
                && let Some(release) = dec_ref(&decl.ty, &local(*dest))
            {
                let _ = writeln!(out, "    {release}");
            }
            let _ = writeln!(
                out,
                "    {} = {};",
                local(*dest),
                undefined(module, &decl.ty)
            );
            let _ = writeln!(
                out,
                "    {} = 0;",
                by_ir::function::RegisterDecl::presence(*dest)
            );
            out
        }
        Op::LoadClass { dest, class } => {
            let Some(owner) = module
                .classes
                .iter()
                .find(|candidate| candidate.name == *class)
            else {
                return String::new();
            };
            let type_name = owner.type_name(module.name.dotted());
            assign_owned(
                module,
                function,
                *dest,
                &format!("By_NewRef({type_name}_OBJ)"),
            )
        }
        Op::ImportModule {
            dest,
            name,
            fromlist,
            level,
        } => {
            let names = fromlist
                .iter()
                .map(|name| c_string(name))
                .collect::<Vec<_>>()
                .join(", ");
            let mut out = String::new();
            let _ = writeln!(
                out,
                "    {{ static const char *const by_from[] = {{ {} }};",
                if fromlist.is_empty() {
                    "NULL".to_string()
                } else {
                    names
                }
            );
            let call = format!(
                "By_ImportModule({}, by_module_dict, by_from, {}, {level})",
                c_string(name),
                fromlist.len()
            );
            out.push_str(&assign_checked(
                module,
                function,
                *dest,
                &call,
                error_target,
            ));
            out.push_str("    }\n");
            out
        }
        Op::ImportFrom {
            dest,
            module: imported,
            name,
        } => {
            let call = format!(
                "By_ImportFrom({}, {})",
                value_expr(imported),
                c_string(name)
            );
            assign_checked(module, function, *dest, &call, error_target)
        }
        Op::CallValue { dest, callee, args } => {
            let argv = args.iter().map(value_expr).collect::<Vec<_>>().join(", ");
            let mut out = String::new();
            let _ = writeln!(
                out,
                "    {{ PyObject *by_argv[] = {{ {} }};",
                if args.is_empty() {
                    "NULL".to_string()
                } else {
                    argv
                }
            );
            let _ = writeln!(
                out,
                "      PyObject *by_t = By_CallPython({}, by_argv, {});",
                value_expr(callee),
                args.len()
            );
            out.push_str(&commit_checked(function, *dest, error_target));
            out
        }
        Op::CallPython { dest, callee, args } => {
            let mut out = String::new();
            // asked afresh whenever a namespace has been written to since the last
            // answer, which is not the same as early binding: early binding is a
            // tier-3 assumption gated on `api.lock`, where this is a memo with a
            // validity test, so a module that rebinds the name is obeyed at once
            //
            // the slot below holds the interned *name*, which is what makes it
            // sound to keep unconditionally: it is what the lookup is keyed on
            let slot = format!("by_g_{}", mangle(callee));
            let _ = writeln!(
                out,
                "    {{ static PyObject *{slot} = NULL; PyObject *by_fn = NULL;"
            );
            let _ = writeln!(
                out,
                "      if ({slot} == NULL) {slot} = By_InternedStr({});",
                c_string_sized(callee)
            );
            let _ = writeln!(
                out,
                "      if ({slot} == NULL) goto {};",
                error_label(error_target)
            );
            let site = format!("by_gs_{}", mangle(callee));
            let _ = writeln!(
                out,
                "      static ByGlobalSite {site} = BY_GLOBAL_SITE_INIT;"
            );
            let _ = writeln!(
                out,
                "      by_fn = By_LookupGlobalSite(&{site}, by_module_dict, {slot});"
            );
            let _ = writeln!(
                out,
                "      if (by_fn == NULL) goto {};",
                error_label(error_target)
            );
            let argv = args.iter().map(value_expr).collect::<Vec<_>>().join(", ");
            let _ = writeln!(
                out,
                "      PyObject *by_argv[] = {{ {} }};",
                if args.is_empty() {
                    "NULL".to_string()
                } else {
                    argv
                }
            );
            let _ = writeln!(
                out,
                "      PyObject *by_t = By_CallPython(by_fn, by_argv, {});",
                args.len()
            );
            let _ = writeln!(out, "      Py_DECREF(by_fn);");
            out.push_str(&commit_checked(function, *dest, error_target));
            out
        }
        Op::CallMethod {
            dest,
            receiver,
            name,
            args,
        } => {
            let mut out = String::new();
            let slot = format!("by_m_{}", mangle(name));
            let _ = writeln!(out, "    {{ static PyObject *{slot} = NULL;");
            let _ = writeln!(
                out,
                "      if ({slot} == NULL) {slot} = By_InternedStr({});",
                c_string_sized(name)
            );
            let _ = writeln!(
                out,
                "      if ({slot} == NULL) goto {};",
                error_label(error_target)
            );
            // slot 0 is left for the receiver, which By_CallMethod fills in
            let argv = std::iter::once("NULL".to_string())
                .chain(args.iter().map(value_expr))
                .collect::<Vec<_>>()
                .join(", ");
            let _ = writeln!(out, "      PyObject *by_argv[] = {{ {argv} }};");
            // `append` on an exact list skips the attribute lookup the protocol
            // would repeat every time. the *runtime* guard is what makes that
            // correct — a list is an `object` here, like every other container, so
            // there is nothing statically to key on, and a receiver that is not an
            // exact list pays one type check and takes the ordinary path
            //
            // every other name goes through a site of its own, which remembers what
            // the name resolved to for the receiver's type and re-derives it only
            // when the type or its version tag moves
            match name.as_str() {
                "append" => {
                    let _ = writeln!(
                        out,
                        "      PyObject *by_t = By_ListAppend({}, {slot}, by_argv, {});",
                        value_expr(receiver),
                        args.len()
                    );
                }
                _ => {
                    let site = format!("by_site_{}", mangle(name));
                    let _ = writeln!(
                        out,
                        "      static ByMethodSite {site} = BY_METHOD_SITE_INIT;"
                    );
                    let _ = writeln!(
                        out,
                        "      PyObject *by_t = By_CallMethodSite(&{site}, {}, {slot}, by_argv, {});",
                        value_expr(receiver),
                        args.len()
                    );
                }
            }
            out.push_str(&commit_checked(function, *dest, error_target));
            out
        }
        Op::GetField {
            dest,
            receiver,
            class,
            field,
        } => {
            // a field read at a compile-time offset *into the storage*: no hash lookup
            // and no descriptor, though for a class appending to a base the storage has
            // to be found first
            let fields = receiver_fields(module, class, field, receiver);
            let mut expr = format!("{fields}->{}", mangle_member(field));
            // an attribute `__init__` assigns on only some paths may not be there, and
            // python answers a read of one with `AttributeError` rather than a value
            let mut out = String::new();
            // unless the class body bound a value of its own under the name, which is what
            // python answers with until the instance has one. it is the same question the
            // descriptor over the field asks, and it has to be asked here too: a method
            // reads the layout directly, so an `AttributeError` here is exactly the answer
            // the class-level value exists to replace — `asyncio`'s `_LoopBoundMixin` reads
            // `self._loop` before anything has written one
            if let Some(decl) = field_decl(module, class, field)
                && decl.optional
            {
                match named_field_default(module, class, field) {
                    Some(cell) => {
                        expr = format!("({fields}->{} ? {expr} : {cell})", decl.presence());
                    }
                    None => {
                        let _ = writeln!(
                            out,
                            "    if (BY_UNLIKELY(!{}->{})) {{ PyErr_Format(PyExc_AttributeError, \
                             \"'%s' object has no attribute '%s'\", By_TypeName((PyObject *){}), {}); \
                             goto {}; }}",
                            fields,
                            decl.presence(),
                            value_expr(receiver),
                            c_string(field),
                            error_label(error_target)
                        );
                    }
                }
            }
            let borrowed = function.register(*dest).is_some_and(|decl| decl.borrowed);
            if borrowed {
                // no retain, and nothing to release: the borrow pass proved the
                // receiver keeps the value alive across every use
                let _ = writeln!(out, "    {} = {expr};", local(*dest));
                return out;
            }
            if let Some(decl) = function.register(*dest)
                && let Some(retain) = inc_ref(&decl.ty, &expr)
            {
                let _ = writeln!(out, "    {retain}");
            }
            out.push_str(&assign_owned(module, function, *dest, &expr));
            out
        }
        Op::SetField {
            receiver,
            class,
            field,
            value,
        } => {
            let fields = receiver_fields(module, class, field, receiver);
            let target = format!("{fields}->{}", mangle_member(field));
            let mut out = String::new();
            let ty = function.value_type(value).unwrap_or(RType::OBJECT);
            if let Some(retain) = inc_ref(&ty, &value_expr(value)) {
                let _ = writeln!(out, "    {retain}");
            }
            // the release reads the old value, so an optional field that was never
            // written must not be released: the byte says whether there is one
            if let Some(release) = dec_ref(&ty, &target) {
                match field_decl(module, class, field) {
                    Some(decl) if decl.optional => {
                        let _ = writeln!(
                            out,
                            "    if ({}->{}) {{ {release} }}",
                            fields,
                            decl.presence()
                        );
                    }
                    _ => {
                        let _ = writeln!(out, "    {release}");
                    }
                }
            }
            let _ = writeln!(out, "    {target} = {};", value_expr(value));
            if let Some(decl) = field_decl(module, class, field)
                && decl.optional
            {
                let _ = writeln!(out, "    {}->{} = 1;", fields, decl.presence());
            }
            if let Some(owner) = class_named(module, class)
                && let Some(decl) = field_decl(module, class, field)
            {
                out.push_str(&publish_field(module, owner, &fields, decl, &target));
            }
            out
        }
        Op::GetAttr {
            dest,
            receiver,
            name,
        } => {
            let mut out = String::new();
            let slot = format!("by_a_{}", mangle(name));
            let _ = writeln!(out, "    {{ static PyObject *{slot} = NULL;");
            let _ = writeln!(
                out,
                "      if ({slot} == NULL) {slot} = By_InternedStr({});",
                c_string_sized(name)
            );
            let _ = writeln!(
                out,
                "      if ({slot} == NULL) goto {};",
                error_label(error_target)
            );
            let _ = writeln!(
                out,
                "      PyObject *by_t = By_GetAttr({}, {slot});",
                value_expr(receiver)
            );
            out.push_str(&commit_checked(function, *dest, error_target));
            out
        }
        Op::SetAttr {
            dest,
            receiver,
            name,
            value,
        } => {
            let mut out = String::new();
            let slot = format!("by_a_{}", mangle(name));
            let _ = writeln!(out, "    {{ static PyObject *{slot} = NULL;");
            let _ = writeln!(
                out,
                "      if ({slot} == NULL) {slot} = By_InternedStr({});",
                c_string_sized(name)
            );
            let _ = writeln!(
                out,
                "      if ({slot} == NULL) goto {};",
                error_label(error_target)
            );
            let expr = format!(
                "By_SetAttr({}, {slot}, {})",
                value_expr(receiver),
                value_expr(value)
            );
            out.push_str(&assign_checked(
                module,
                function,
                *dest,
                &expr,
                error_target,
            ));
            out.push_str("    }\n");
            out
        }
        Op::BuildList { dest, items } => emit_container(
            function,
            *dest,
            items,
            "By_BuildList",
            items.len(),
            error_target,
        ),
        Op::BuildSet { dest, items } | Op::BuildTuple { dest, items } => {
            let builder = if matches!(op, Op::BuildSet { .. }) {
                "By_BuildSet"
            } else {
                "By_BuildTuple"
            };
            emit_container(function, *dest, items, builder, items.len(), error_target)
        }
        Op::BuildDict { dest, pairs } => emit_container(
            function,
            *dest,
            pairs,
            "By_BuildDict",
            pairs.len() / 2,
            error_target,
        ),
        Op::GetItem {
            dest,
            container,
            index,
        } => {
            let call = match function.value_type(index) {
                Some(RType::INT) => "By_GetItemTagged",
                // an unboxed counter names the element by the number already in
                // its register, so it does not have to become a tagged `int`
                // first — see `By_GetItemI64`
                Some(RType::Primitive(Primitive::Fixed(_))) => "By_GetItemI64",
                _ => "By_GetItem",
            };
            let expr = format!("{call}({}, {})", value_expr(container), value_expr(index));
            assign_checked(module, function, *dest, &expr, error_target)
        }
        Op::DictFind {
            dest,
            container,
            key,
        } => {
            // a null result is an absent key *or* failure, so the check has to
            // consult the exception state rather than the value alone
            let expr = format!(
                "By_DictFind({}, {})",
                value_expr(container),
                value_expr(key)
            );
            let mut out = assign_owned(module, function, *dest, &expr);
            let _ = writeln!(
                out,
                "    if ({} == NULL && PyErr_Occurred()) goto {};",
                local(*dest),
                error_label(error_target)
            );
            out
        }
        Op::StrGetItem {
            dest,
            container,
            index,
        } => {
            let expr = format!(
                "By_StrItemTagged({}, {})",
                value_expr(container),
                value_expr(index)
            );
            assign_checked(module, function, *dest, &expr, error_target)
        }
        Op::StrItemCompare {
            dest,
            op,
            container,
            index,
            character,
        } => {
            let expr = format!(
                "{}({}, {}, {}, {})",
                match function.value_type(index) {
                    Some(RType::INT) => "By_StrItemCompareChar",
                    _ => "By_StrItemCompareCharI64",
                },
                value_expr(container),
                value_expr(index),
                *character as u32,
                rich_compare_op(*op)
            );
            assign_checked(module, function, *dest, &expr, error_target)
        }
        Op::SetItem {
            dest,
            container,
            index,
            value,
        } => {
            let expr = format!(
                "{}({}, {}, {})",
                match function.value_type(index) {
                    Some(RType::INT) => "By_SetItemTagged",
                    _ => "By_SetItem",
                },
                value_expr(container),
                value_expr(index),
                value_expr(value)
            );
            assign_checked(module, function, *dest, &expr, error_target)
        }
        Op::Format {
            dest,
            value,
            spec,
            conversion,
        } => {
            let spec = spec.as_ref().map_or("NULL".to_string(), value_expr);
            let expr = format!(
                "By_Format({}, {spec}, {})",
                value_expr(value),
                conversion.c_name()
            );
            assign_checked(module, function, *dest, &expr, error_target)
        }
        Op::FetchException { dest } => {
            // a null result means nothing was pending, which is not an error
            assign_owned(module, function, *dest, "By_FetchException()")
        }
        Op::PushHandled { dest, value } => {
            let expr = format!("By_PushHandled({})", value_expr(value));
            assign_owned(module, function, *dest, &expr)
        }
        Op::PopHandled { value } => format!("    By_PopHandled({});\n", value_expr(value)),
        Op::RaiseObject { exception, cause } => format!(
            "    By_RaiseObject({}, {});\n    goto {};\n",
            value_expr(exception),
            match cause {
                Some(cause) => value_expr(cause),
                None => "NULL".to_string(),
            },
            error_label(error_target)
        ),
        Op::ExceptionMatches { dest, value, class } => {
            let expr = format!(
                "By_ExceptionMatches({}, {})",
                value_expr(value),
                value_expr(class)
            );
            assign_owned(module, function, *dest, &expr)
        }
        // a re-raise leaves the region it is *in* and enters the one around it. a
        // handler block is sealed after its own target is restored, so its
        // `error_target` is already the enclosing handler — and `by_error` when there
        // is none. hardcoding the function exit skipped every outer `except`
        Op::Reraise { value } => format!(
            "    By_Reraise({});\n    goto {};\n",
            value_expr(value),
            error_label(error_target)
        ),
        Op::GetIter { dest, src, cursor } => {
            let mut out = match cursor {
                Some(_) => {
                    let expr = format!("By_CursorIter({})", value_expr(src));
                    assign_checked(module, function, *dest, &expr, error_target)
                }
                None => {
                    let expr = format!("By_GetIter({})", value_expr(src));
                    assign_checked(module, function, *dest, &expr, error_target)
                }
            };
            // the cursor starts a loop at the top of whatever it is walking. it is
            // set *here* rather than where the register is declared because one
            // register serves every trip through an enclosing loop
            if let Some(cursor) = cursor {
                let _ = writeln!(out, "    {} = 0;", local(*cursor));
            }
            out
        }
        Op::IterNext { dest, iter, cursor } => {
            // a null result is exhaustion *or* failure, so the check has to
            // consult the exception state rather than the value alone
            let expr = match cursor {
                Some(cursor) => format!("By_CursorStep({}, &{})", value_expr(iter), local(*cursor)),
                None => format!("By_IterNext({})", value_expr(iter)),
            };
            let mut out = assign_owned(module, function, *dest, &expr);
            let _ = writeln!(
                out,
                "    if ({} == NULL && PyErr_Occurred()) goto {};",
                local(*dest),
                error_label(error_target)
            );
            out
        }
        Op::IsNull { dest, src } => {
            let expr = format!("(char)({} == NULL)", value_expr(src));
            assign_owned(module, function, *dest, &expr)
        }
        Op::Len { dest, src } => {
            let expr = format!("By_Len({})", value_expr(src));
            assign_checked(module, function, *dest, &expr, error_target)
        }
        // the file and the line are the ones `warn` would have read off this
        // function's own frame, and they come from the same line table the `#line`
        // directives above do — so a warning names the source somebody wrote, exactly
        // as a `.pyc` carries the path it was compiled from. a module built without a
        // table has nowhere to name; python renders an empty file name as an empty
        // prefix rather than inventing one, and `by_build` gives every module a table
        // before this runs
        Op::Warn {
            dest,
            message,
            category,
            stacklevel,
            offset,
        } => {
            let (path, line) = module.lines.as_ref().map_or((String::new(), 0), |lines| {
                (lines.path.clone(), lines.line(*offset))
            });
            let expr = format!(
                "By_Warn({}, {}, by_module_dict, {}, {line}, {stacklevel})",
                value_expr(message),
                category.as_ref().map_or("NULL".to_string(), value_expr),
                c_string(&path),
            );
            assign_checked(module, function, *dest, &expr, error_target)
        }
        Op::StrOfInt { dest, value } => {
            let mut out = resolve_str(error_target);
            let _ = writeln!(
                out,
                "      PyObject *by_t = By_StrOfInt(by_fn, {});",
                value_expr(value)
            );
            let _ = writeln!(out, "      Py_DECREF(by_fn);");
            out.push_str(&commit_checked(function, *dest, error_target));
            out
        }
        Op::StrConcatInt { dest, lhs, value } => {
            let mut out = resolve_str(error_target);
            let _ = writeln!(
                out,
                "      PyObject *by_t = By_StrConcatInt({}, by_fn, {});",
                value_expr(lhs),
                value_expr(value)
            );
            let _ = writeln!(out, "      Py_DECREF(by_fn);");
            out.push_str(&commit_checked(function, *dest, error_target));
            out
        }
        Op::StrConcat {
            dest,
            lhs,
            rhs,
            consumes_lhs,
        } => {
            let Some(Value::Register(source)) = consumes_lhs.then_some(lhs) else {
                let expr = format!("By_StrConcat({}, {})", value_expr(lhs), value_expr(rhs));
                return assign_checked(module, function, *dest, &expr, error_target);
            };
            // the register is emptied *before* the call, because `By_StrAppend` takes
            // the reference over whether it succeeds or not — a register still naming
            // it would be released a second time on the way out.
            //
            // the right operand is read first so that emptying the left cannot take
            // it away: the two name one register when a string is appended to itself,
            // and the helper's own answer for that is what has to be reached
            let held = local(*source);
            format!(
                "    {{ PyObject * by_rhs = {};\n      \
                 PyObject * by_lhs = {held}; {held} = NULL;\n      \
                 PyObject * by_t = By_StrAppend(by_lhs, by_rhs);\n{}",
                value_expr(rhs),
                commit_checked(function, *dest, error_target),
            )
        }
        Op::RaiseStandard { error, message } => {
            // no message means *no argument*, not an empty one: a bare
            // `StopIteration` carries `None`, and `StopIteration('')` would make a
            // coroutine's result the empty string
            let raise = if message.is_empty() {
                format!("PyErr_SetNone({})", error.c_name())
            } else {
                format!("PyErr_SetString({}, {})", error.c_name(), c_string(message))
            };
            format!("    {raise};\n    goto {};\n", error_label(error_target))
        }
    }
}

/// a container built from owned references it steals
///
/// each element is retained before the call, because the builder takes ownership
/// of what it is handed and the operand registers keep theirs
fn emit_container(
    function: &Function,
    dest: RegisterId,
    items: &[Value],
    builder: &str,
    count: usize,
    error_target: Option<BlockId>,
) -> String {
    let mut out = String::new();
    let argv = if items.is_empty() {
        "NULL".to_string()
    } else {
        items
            .iter()
            .map(|item| object_expr(function, item))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let _ = writeln!(out, "    {{ PyObject *by_items[] = {{ {argv} }};");
    for item in items {
        if let Some(retain) = inc_ref(&RType::OBJECT, &value_expr(item)) {
            let _ = writeln!(out, "      {retain}");
        }
    }
    let _ = writeln!(out, "      PyObject *by_t = {builder}(by_items, {count});");
    out.push_str(&commit_checked(function, dest, error_target));
    out
}

/// the label a failing operation jumps to: a handler block, or the function's
/// own error exit
fn error_label(error_target: Option<BlockId>) -> String {
    match error_target {
        Some(block) => format!("b{}", block.0),
        None => "by_error".to_string(),
    }
}

fn emit_terminator(
    module: &ModuleIr,
    function: &Function,
    terminator: &Terminator,
    live: Option<&[RegisterId]>,
) -> String {
    match terminator {
        Terminator::Goto(target) => format!("    goto b{};\n", target.0),
        Terminator::Branch {
            cond,
            then_block,
            else_block,
        } => format!(
            "    if ({}) goto b{}; else goto b{};\n",
            value_expr(cond),
            then_block.0,
            else_block.0
        ),
        Terminator::Return(value) => {
            let mut out = String::new();
            let ty = function.ret.clone();
            let expr = value_expr(value);
            // hand the caller an owned reference, then release everything this
            // frame holds. where the value *is* a register this frame owns, the
            // reference it already holds is the one the caller gets, so neither the
            // retain nor that register's release is written
            let moved = returned_by_move(function, value, live);
            let _ = writeln!(out, "    {{ {} by_ret = {expr};", ctype(module, &ty));
            if let Some(retain) = inc_ref(&ty, "by_ret")
                && moved.is_none()
            {
                let _ = writeln!(out, "      {retain}");
            }
            out.push_str(&emit_cleanup(function, "      ", live, moved));
            out.push_str("      return by_ret; }\n");
            out
        }
        Terminator::NarrowShort {
            dest,
            src,
            fits,
            otherwise,
        } => {
            let src = value_expr(src);
            format!(
                "    if (BY_LIKELY(By_IsShort({src}))) {{ {} = (int64_t) By_ShortValue({src}); goto b{}; }} else goto b{};\n",
                local(*dest),
                fits.0,
                otherwise.0
            )
        }
        Terminator::Unreachable => {
            let mut out = emit_cleanup(function, "    ", live, None);
            let _ = writeln!(out, "    return {};", undefined(module, &function.ret));
            out
        }
    }
}

/// the conditions under which a boundary hands the whole call to the interpreted
/// definition, phrased against the `by_bound` slots the binding filled
///
/// a deferring parameter is tested before anything is unboxed, so the outcome does
/// not depend on which parameter is looked at first. the tests are pure: they set no
/// error, because a call that fails one is legal python.
///
/// `receiver` says whether the first parameter is a `self` that arrives outside the
/// binding, which is what shifts every slot index
fn defer_tests(function: &Function, receiver: bool) -> Vec<String> {
    let slot = |index: &usize| format!("by_bound[{}]", index - usize::from(receiver));
    function
        .deferring
        .iter()
        .map(|index| {
            let slot = slot(index);
            format!("({slot} != NULL && !By_IsExactFloat({slot}))")
        })
        // an omitted parameter whose default is not an immediate: the interpreted
        // definition holds the one object every such call has to share — unless the
        // receiver holds it, in which case this boundary can fill it itself
        .chain(
            function
                .computed_defaults
                .iter()
                .filter(|_| function.defaults_held_by == by_ir::function::DefaultsHeldBy::Twin)
                .map(|index| format!("{} == NULL", slot(index))),
        )
        .collect()
}

/// the python-facing wrapper: unbox each argument, call the native entry, box the
/// result. the argument checks here are the `parameters` soundness position
fn emit_wrapper(module: &ModuleIr, function: &Function, is_method: bool) -> String {
    let mut out = format!(
        "static PyObject *{}(PyObject *self, PyObject *const *args, Py_ssize_t nargs, PyObject *kwnames) {{\n",
        function.wrapper_symbol(module.name.dotted())
    );
    // a method's receiver arrives in `self`, so it does not count as an argument
    if !is_method {
        out.push_str("    (void)self;\n");
    }
    let bound = Bound::of(function, is_method);
    let explicit = bound.params.len();
    // one binding pass over positionals and keywords together, so a caller may use
    // either. `by_bound[i]` is NULL where the caller did not supply that parameter,
    // and the per-parameter code below fills those from the defaults
    out.push_str(&bound.declare());
    let _ = writeln!(
        out,
        "    if (By_BindArgs(args, nargs, kwnames, by_names, {explicit}, by_required, {}, {}, by_bound, {}, {}, {}, {}) < 0) return NULL;",
        bound.posonly,
        bound.kwonly,
        i32::from(bound.vararg),
        i32::from(bound.kwarg),
        // python names a method by its class in an arity error, and a bare
        // function by itself
        c_string(&match &function.owner {
            Some(owner) => format!("{owner}.{}", function.name),
            None => function.name.clone(),
        }),
        i32::from(is_method)
    );

    let tests = defer_tests(function, is_method);
    if !tests.is_empty() {
        let _ = writeln!(out, "    if ({}) goto by_wrap_defer;", tests.join(" || "));
    }

    let first_packed = explicit + usize::from(is_method);
    // every argument local is declared *before* the first jump that can skip one:
    // an unbox that fails jumps to the release path, which releases them all, and a
    // local whose declaration was jumped over would be released while indeterminate
    for (index, decl) in function.params().iter().enumerate() {
        let _ = writeln!(
            out,
            "    {} a{index} = {};",
            ctype(module, &decl.ty),
            decl.ty.undefined()
        );
    }
    for (index, decl) in function.params().iter().enumerate() {
        let name = format!("a{index}");
        // the packed parameters are *built* from what is left over, not bound
        if index >= first_packed {
            let is_vararg = function.vararg && index == first_packed;
            let call = if is_vararg {
                // the surplus starts where the run a caller may fill positionally
                // ends, which keyword-only parameters move back
                format!("By_PackArgs(args, nargs, {})", explicit - bound.kwonly)
            } else {
                format!(
                    "By_PackKwargs(args, nargs, kwnames, by_names, {explicit}, {})",
                    bound.posonly
                )
            };
            let _ = writeln!(out, "    {name} = {call};");
            let _ = writeln!(out, "    if ({name} == NULL) goto by_wrap_error;");
            continue;
        }
        // the receiver is `self`; every later parameter comes from the binding, which
        // is where a keyword argument landed
        let slot = if is_method {
            if index == 0 {
                "self".to_string()
            } else {
                format!("by_bound[{}]", index - 1)
            }
        } else {
            format!("by_bound[{index}]")
        };
        // a parameter past `nargs` takes its default, which is an immediate — so the
        // fill is a plain initializer rather than a call into the runtime
        let default = function
            .defaults
            .get(index)
            .and_then(Option::as_ref)
            .filter(|_| !(is_method && index == 0));
        let unbox = unbox_checked(module, &decl.ty, &slot);
        // a default the *receiver* holds, which is how a nested function keeps one: the
        // frame that made the closure evaluated it where the `def` stood and parked it
        // in the environment, and `a0` is that environment
        let from_receiver = (function.defaults_held_by
            == by_ir::function::DefaultsHeldBy::Receiver
            && function.computed_defaults.contains(&index))
        .then(|| function.receiver_default_field(index))
        .flatten();
        match (default, from_receiver) {
            (_, Some(field)) => {
                let position = index - usize::from(is_method);
                let held = unbox_checked(module, &decl.ty, "by_default");
                let _ = writeln!(out, "    if (by_bound[{position}] != NULL) {{");
                let _ = writeln!(out, "        {name} = {unbox};");
                let _ = writeln!(out, "    }} else {{");
                // the read raises rather than handing NULL on, in python's own wording.
                // it cannot fire: the frame writes the field before the closure that
                // reaches this boundary exists at all
                let _ = writeln!(
                    out,
                    "        PyObject *by_default = By_ReadCell(a0->{}, {}, 0);",
                    mangle_member(&field),
                    c_string(decl.name.as_deref().unwrap_or(""))
                );
                let _ = writeln!(out, "        if (by_default == NULL) goto by_wrap_error;");
                let _ = writeln!(out, "        {name} = {held};");
                let _ = writeln!(out, "        Py_DECREF(by_default);");
                let _ = writeln!(out, "    }}");
            }
            (Some(default), None) => {
                let position = index - usize::from(is_method);
                let _ = writeln!(out, "    if (by_bound[{position}] != NULL) {{");
                let _ = writeln!(out, "        {name} = {unbox};");
                let _ = writeln!(out, "    }} else {{");
                let _ = writeln!(out, "        {name} = {};", default_expr(&decl.ty, default));
                let _ = writeln!(out, "    }}");
            }
            // a parameter with no default is guaranteed present: `By_BindArgs` reports
            // every missing one, in cpython's own wording
            (None, None) => {
                let _ = writeln!(out, "    {name} = {unbox};");
            }
        }
        let _ = writeln!(
            out,
            "    if ({}) goto by_wrap_error;",
            error_check(&decl.ty, &name)
        );
    }

    let args = (0..function.param_count)
        .map(|index| format!("a{index}"))
        .collect::<Vec<_>>()
        .join(", ");
    let _ = writeln!(
        out,
        "    {} by_result = {}({args});",
        ctype(module, &function.ret),
        function.native_symbol(module.name.dotted())
    );
    // a resume hands back nothing both when the frame returned and when it raised, and
    // the second is the only one a python caller can be given. anything reaching the
    // step through this wrapper rather than through the iterator protocol still gets
    // the `StopIteration`, because a wrapper that returned NULL with no exception set
    // would be a `SystemError` at best
    if resumes(module, function) {
        out.push_str("    if (by_result == NULL) (void)By_TakeReturn(&a0->by_returned);\n");
    }
    if function.convention.can_fail() {
        let _ = writeln!(
            out,
            "    if ({}) goto by_wrap_error;",
            error_check(&function.ret, "by_result")
        );
    }

    // arguments were owned by the wrapper; the native entry took its own
    for (index, decl) in function.params().iter().enumerate() {
        if let Some(release) = dec_ref(&decl.ty, &format!("a{index}")) {
            let _ = writeln!(out, "    {release}");
        }
    }

    // the native entry handed us an owned reference; taking another would leak
    let boxed = box_owned(&function.ret, "by_result");
    let _ = writeln!(out, "    return {boxed};");

    out.push_str("by_wrap_error: ;\n");
    for (index, decl) in function.params().iter().enumerate() {
        if let Some(release) = dec_ref(&decl.ty, &format!("a{index}")) {
            let _ = writeln!(out, "    {release}");
        }
    }
    out.push_str("    return NULL;\n");
    // the jump here happens before any argument local is filled, so there is
    // nothing to release — and no error is set, because nothing went wrong
    if function.defers() {
        // the twin is taken off the interpreted class, and for a *static* method that
        // is the plain function the `staticmethod` wraps — so the call is the one a
        // module-level function makes, and `self` holds nothing to put in front of it
        let hands_over_self = function.owner.is_some() && function.binding != Binding::Static;
        let _ = writeln!(out, "by_wrap_defer: ;");
        let _ = writeln!(
            out,
            "    return {}({}, {}, {}args, nargs, kwnames);",
            if hands_over_self {
                "By_CallInterpretedMethod"
            } else {
                "By_CallInterpreted"
            },
            function.interpreted_symbol(module.name.dotted()),
            c_string(&function.name),
            if hands_over_self { "self, " } else { "" }
        );
    }
    out.push_str("}\n");
    out
}

/// a C string literal for `text`, split into adjacent literals
///
/// the maximum length of a *single* string literal is implementation-defined
/// (MSVC stops at 16380 bytes), but adjacent literals concatenate without limit
fn c_string_chunked(text: &str) -> String {
    if text.is_empty() {
        return "\"\"".to_string();
    }
    let mut out = String::new();
    let mut chunk = String::new();
    for ch in text.chars() {
        chunk.push(ch);
        if chunk.len() >= 1024 {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&c_string(&chunk));
            chunk.clear();
        }
    }
    if !chunk.is_empty() {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&c_string(&chunk));
    }
    out
}

/// a C string literal for arbitrary bytes, split into adjacent literals
///
/// as [`c_string_chunked`], but the input is not text: a marshalled code object is
/// mostly bytes no character stands for, and every one of them is escaped. the length
/// is carried separately because these bytes contain NULs
fn c_bytes_chunked(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return "\"\"".to_string();
    }
    bytes
        .chunks(1024)
        .map(c_byte_string)
        .collect::<Vec<_>>()
        .join("\n")
}

/// the statements that resolve a dotted name out of the module namespace into `into`
///
/// the interpreted fallback has already run, so an imported name is in the module dict;
/// a builtin like `Exception` is not, and falls through to builtins. one lookup covers
/// both, and names the one it cannot find. a dotted name like `os.PathLike` then walks
/// attributes, each step replacing the reference it came from — so a failure anywhere
/// leaves `into` NULL and `on_failure` is what runs
fn resolve_dotted(path: &str, into: &str, on_failure: &str) -> String {
    let mut segments = path.split('.');
    let root = segments.next().unwrap_or(path);
    let mut out = format!(
        "\x20     {into} = By_LookupGlobalString(dict, {});\n",
        c_string(root)
    );
    for attr in segments {
        let _ = writeln!(
            out,
            "\x20     if ({into} != NULL) {{ PyObject *by_next = \
             PyObject_GetAttrString({into}, {}); \
             Py_DECREF({into}); {into} = by_next; }}",
            c_string(attr)
        );
    }
    let _ = writeln!(out, "\x20     if ({into} == NULL) {{ {on_failure} }}");
    out
}

/// the class header's keywords as a fresh dict, or `NULL` where it has none
///
/// each value is evaluated the way the class body would have evaluated it: a name out
/// of the module namespace, or a literal where it stands
fn emit_class_keywords(class: &ClassIr, type_name: &str) -> String {
    if class.keywords.is_empty() {
        return String::new();
    }
    let mut body = String::new();
    for keyword in &class.keywords {
        let value = match &keyword.value {
            KeywordValue::Path(path) => {
                let _ = write!(
                    body,
                    "{}",
                    resolve_dotted(path, "by_value", "Py_DECREF(by_kwds); return NULL;")
                );
                "by_value"
            }
            KeywordValue::Bool(value) => {
                let _ = writeln!(
                    body,
                    "\x20     by_value = By_NewRef({});",
                    if *value { "Py_True" } else { "Py_False" }
                );
                "by_value"
            }
            KeywordValue::None => {
                let _ = writeln!(body, "\x20     by_value = By_NewRef(Py_None);");
                "by_value"
            }
            KeywordValue::Int(value) => {
                let _ = writeln!(
                    body,
                    "\x20     by_value = PyLong_FromLongLong({value}LL);\n\
                     \x20     if (by_value == NULL) {{ Py_DECREF(by_kwds); return NULL; }}"
                );
                "by_value"
            }
            KeywordValue::Str(text) => {
                let _ = writeln!(
                    body,
                    "\x20     by_value = PyUnicode_FromStringAndSize({});\n\
                     \x20     if (by_value == NULL) {{ Py_DECREF(by_kwds); return NULL; }}",
                    c_string_sized(text)
                );
                "by_value"
            }
        };
        let _ = writeln!(
            body,
            "\x20     by_failed = PyDict_SetItemString(by_kwds, {}, {value}) < 0;\n\
             \x20     Py_DECREF({value});\n\
             \x20     if (by_failed) {{ Py_DECREF(by_kwds); return NULL; }}",
            c_string(&keyword.name)
        );
    }
    format!(
        "static PyObject *{type_name}_keywords(PyObject *dict) {{\n\
         \x20     PyObject *by_value;\n\
         \x20     int by_failed;\n\
         \x20     PyObject *by_kwds = PyDict_New();\n\
         \x20     if (by_kwds == NULL) return NULL;\n\
         {body}\
         \x20     return by_kwds;\n\
         }}\n\n"
    )
}

/// the `By_BuildClass` call that gives a class on bases out of this module its type
///
/// the runtime decides between the three constructions, because only the running
/// interpreter knows what the base names resolved to. what this settles is which of
/// them are open to *this* class at all: the spec where a keyword does not rule it out,
/// and the metaclass where the class has no fields of its own to place
///
/// a class appending storage is not built here at all: the interpreted definition is one
/// of the three constructions, and for such a class it is not an answer — so it is built
/// where a refusal can still decline the whole module
fn external_construction(
    module: &ModuleIr,
    class: &ClassIr,
    type_name: &str,
    pack: &str,
    slot: Option<usize>,
) -> String {
    let spec = if !class.keywords.is_empty() || stands_on_an_emitted_base(module, class) {
        // a keyword has nowhere to go in a spec, and a base of ours beside one from
        // outside is a shape a spec cannot work out
        "NULL".to_string()
    } else {
        format!("&{type_name}_spec")
    };
    let keywords = if class.keywords.is_empty() {
        "NULL"
    } else {
        "by_kwds"
    };
    // what a metaclass construction writes into the namespace off the interpreted body,
    // and reads back off the class to see whether it agreed. a class no interpreted
    // `class` statement wrote has no captured body to take a value off, and there is
    // nothing to carry
    let (carried, required) = carried_off_the_body(class);
    let (declare, constants) = match slot.filter(|_| !carried.is_empty()) {
        None => (String::new(), "NULL".to_string()),
        Some(slot) => {
            let names = carried
                .iter()
                .map(|name| c_string(name))
                .collect::<Vec<_>>()
                .join(", ");
            (
                format!(
                    "\x20     static const char *const by_constants[] = {{{names}}};\n\
                     \x20     By_ClassConstants by_carried = {{by_body[{slot}], by_constants, {}, {required}, &by_twins}};\n",
                    carried.len()
                ),
                "&by_carried".to_string(),
            )
        }
    };
    let build = format!(
        "By_BuildClass(dict, {}, {pack}, {keywords}, {type_name}_methods, {spec}, {}, {constants})",
        c_string(&class.name),
        i32::from(metaclass_construction(class))
    );
    if class.keywords.is_empty() {
        return format!("{declare}\x20     {type_name} = {build};\n");
    }
    format!(
        "{declare}\
         \x20     {{ PyObject *by_kwds = {type_name}_keywords(dict);\n\
         \x20       if (by_kwds == NULL) return -1;\n\
         \x20       {type_name} = {build};\n\
         \x20       Py_DECREF(by_kwds); }}\n"
    )
}

/// every decorator this module's init applies, applied where the twin left off
///
/// the source the twin runs has these taken out of it — see
/// [`ModuleIr::decorated_at_init`](by_ir::function::ModuleIr::decorated_at_init) — so an
/// init that gives up before it has installed anything of its own still has to run them,
/// or the module is left holding definitions nothing ever decorated. it applies them to
/// the namespace entry, which on that path is still the twin's own definition — which is
/// exactly where python would have applied them
fn twin_decorators(module: &ModuleIr) -> String {
    let mut out = String::new();
    for decoration in module.decorated_at_init() {
        for decorator in decoration.decorators.iter().rev() {
            let _ = writeln!(
                out,
                "    if (By_ApplyDecorator(dict, {}, {}) < 0) return -1;",
                c_string(decoration.name),
                c_string(&decorator.dotted())
            );
        }
    }
    out
}

fn emit_module_init(module: &ModuleIr) -> String {
    let mut out = String::new();

    // the interpreted definition of the whole module, run at import so that
    // module-level code executes and declined functions exist
    let _ = writeln!(
        out,
        "static const char by_fallback_source[] =\n{};\n",
        c_string_chunked(module.fallback_source.as_deref().unwrap_or(""))
    );
    // and the same program as a code object, so that an import reads it rather than
    // parsing the text over again. the text stays: a code object is only good for the
    // interpreter that wrote it — `By_Fallback` says which — and a build with no
    // interpreter to ask has none at all
    match &module.fallback_code {
        Some(code) => {
            let _ = writeln!(
                out,
                "static const char by_fallback_code[] =\n{};\n\n\
                 static const By_Fallback by_fallback = {{\n\
                 \x20   by_fallback_source, by_fallback_code, {}, {}L, {}}};\n",
                c_bytes_chunked(&code.marshalled),
                code.marshalled.len(),
                code.magic,
                code.optimize
            );
        }
        None => {
            let _ = writeln!(
                out,
                "static const By_Fallback by_fallback = {{by_fallback_source, NULL, 0, 0L, 0}};\n"
            );
        }
    }

    // a function the twin publishes a forwarder for is *not* in `by_methods`: its
    // native object never goes into the module namespace under its own name, and is
    // reached only through the `function` the forwarder installs there. see
    // `by_irbuild::shims` for why a module cannot publish the native object itself
    let forwarded: Vec<&str> = module
        .shims
        .as_ref()
        .map(|shims| shims.functions.iter().map(String::as_str).collect())
        .unwrap_or_default();
    let entry = |function: &Function| {
        format!(
            "    {{\"{}\", (PyCFunction)(void(*)(void)){}, METH_FASTCALL | METH_KEYWORDS, {}}},\n",
            function.name,
            function.wrapper_symbol(module.name.dotted()),
            method_doc(function)
        )
    };
    out.push_str("static PyMethodDef by_methods[] = {\n");
    for function in &module.functions {
        if function.exported && !forwarded.contains(&function.name.as_str()) {
            out.push_str(&entry(function));
        }
    }
    out.push_str("    {NULL, NULL, 0, NULL}\n};\n\n");
    // and the forwarded ones. both this table and the installer walk the module's
    // own function order, which is what keeps slot `n` here the native the `n`th
    // forwarder calls
    let mut forwarded_entries = 0;
    if !forwarded.is_empty() {
        out.push_str("static PyMethodDef by_forwarded[] = {\n");
        for function in &module.functions {
            if function.exported && forwarded.contains(&function.name.as_str()) {
                out.push_str(&entry(function));
                forwarded_entries += 1;
            }
        }
        out.push_str("    {NULL, NULL, 0, NULL}\n};\n\n");
    }
    debug_assert_eq!(
        forwarded_entries,
        forwarded.len(),
        "every forwarder the twin defines needs the native it calls"
    );

    // the `PyModuleDef`'s `m_name`, which is not where a module's `__name__` comes
    // from: this is a multi-phase init, so python builds the module from the
    // *spec*'s name and never reads this one. it says the last component because
    // that is what the init symbol beside it is named after
    let last = module.name.last_component();
    // `m_methods` is NULL and the natives are installed from the exec slot
    // instead, so they land *after* the interpreted definitions rather than
    // being overwritten by them
    // decorators run last: the native function has to be in the namespace before
    // a decorator can be applied to it.
    //
    // `exported` is what says there is a namespace entry to apply one to at all — an
    // unboxed edition is a second function under a mangled name nothing binds, and
    // reaching for it here would fail the import with a `NameError`. it is also the
    // condition `ModuleIr::decorated_at_init` states, and the twin's source has these
    // decorators taken out of it on the strength of that: applying one here that the
    // twin no longer applies, or the reverse, is what makes a decorator run twice or
    // not at all
    let mut decorators = String::new();
    for function in module.functions.iter().filter(|function| function.exported) {
        for decorator in function.decorators.iter().rev() {
            let _ = writeln!(
                decorators,
                "    if (By_ApplyDecorator(dict, {}, {}) < 0) return -1;",
                c_string(&function.name),
                c_string(&decorator.dotted())
            );
        }
    }

    // a class that keeps its fields past a base's instance is built here, before anything
    // of this module's own is installed, because it is the one class no other construction
    // answers for: the compiled functions read its fields at an offset only the type
    // `By_SpecClass` builds has, and the interpreted definition's instances stop where the
    // base's do. so a refusal is a whole-module one — the interpreted definition already
    // built the module, and it is left standing rather than made into a half-native
    // mixture
    let mut conditions = Vec::new();
    // below 3.12 there is no way to say where appended storage goes at all, so no such
    // class has a construction and the module has none either
    if module
        .classes
        .iter()
        .any(|class| built_ahead(module, class))
    {
        conditions.push("!BY_HAS_TYPE_DATA");
    }
    // whether the fallback source has to be run with its class bodies captured. the
    // capture costs a dict copy per class the body writes, so a module with nothing to
    // take out of one runs its body the plain way. a decorated method is taken out of a
    // body too — the body is where the decorator's single application landed
    let captures_bodies = module.classes.iter().any(|class| {
        class.exported
            && (!class.constants.is_empty()
                || class
                    .methods
                    .iter()
                    .any(|method| !method.decorators.is_empty()))
    });
    let release_bodies = if captures_bodies {
        "    Py_XDECREF(by_bodies);\n"
    } else {
        ""
    };
    // an exit that leaves the module interpreted never installs a native, so the
    // installer the twin defined has nothing to do — and it must not be left standing
    // in the namespace, where it would be a name python's own module never has
    let drop_installer = match &module.shims {
        Some(shims) => format!("    By_DropName(dict, {});\n", c_string(&shims.installer)),
        None => String::new(),
    };
    let mut layout_guard = String::new();
    if !conditions.is_empty() {
        // the twin's source no longer carries the decorators init applies, so leaving
        // the module interpreted means applying them here — to the twin's own
        // definitions, which is where python would have run them
        let _ = write!(
            layout_guard,
            "    if ({}) {{\n{}{drop_installer}{release_bodies}    return 0;\n    }}\n",
            conditions.join(" || "),
            twin_decorators(module)
        );
    }
    for class in &module.classes {
        if built_ahead(module, class) {
            let type_name = class.type_name(module.name.dotted());
            // one of these standing on another is built on the *finished* type below it
            // rather than on the interpreted definition — which is the only base such a
            // class can chain a deallocation to. the order is the module's, so the one
            // below is already built
            let construction = match appended_over_an_emitted_base(module, class) {
                Some(base) => format!(
                    "By_SpecSubclass(dict, {}, &{type_name}_spec, {}, {}_OBJ)",
                    c_string(&class.name),
                    c_string(&base.name),
                    base.type_name(module.name.dotted())
                ),
                None => format!(
                    "By_SpecClass(dict, {}, &{type_name}_spec)",
                    c_string(&class.name)
                ),
            };
            // a class nothing else compiled reaches into is left where it stands: the
            // NULL is carried forward, every step that would have installed it is
            // skipped, and its interpreted definition keeps the name. see
            // `declines_on_its_own`
            if declines_on_its_own(module, class) {
                let _ = writeln!(layout_guard, "    {type_name} = {construction};");
                continue;
            }
            // giving up here leaves every interpreted definition standing, and their
            // decorators have been taken out of the source that built them — so this
            // exit has to run them for the same reason the guard above does. a module
            // with nothing to run keeps the plain one-line refusal
            let unwind = format!(
                "{}{drop_installer}{release_bodies}",
                twin_decorators(module)
            );
            let refusal = if unwind.is_empty() {
                "return 0;".to_string()
            } else {
                format!("{{\n{unwind}    return 0;\n    }}")
            };
            let _ = writeln!(
                layout_guard,
                "    {type_name} = {construction};\n\
                 \x20   if ({type_name} == NULL) {refusal}"
            );
        }
    }

    // the module body goes on mutating a class after its `class` statement — through a
    // helper, through a local, through any name at all — and every one of those writes
    // lands on the interpreted definition, because that is what is under the name until
    // the compiled type replaces it. so the definitions are held onto here, while they
    // still are, and what the body gave them is carried across once every type exists:
    // `urllib.parse` sets `ParseResult._encoded_counterpart` to `ParseResultBytes`, a
    // class declared further down the module, so nothing narrower than all of them at
    // once can be remapped
    let twins: Vec<&ClassIr> = module
        .classes
        .iter()
        .filter(|class| class.exported)
        .collect();
    let mut twin_init = String::new();
    let mut adopt_init = String::new();
    // and the alias remap after that, once every decorator has settled what stands under
    // each class's own name — see `By_RemapTwinAliases` for why it waits that long
    let mut twin_remap = String::new();
    if !twins.is_empty() {
        let count = twins.len();
        // the types are held alongside the twins from here rather than gathered at the
        // adoption, because a class constant is remapped against them as its class is
        // built. a slot is NULL until then, and `By_TwinReplacement` reads that as a
        // refusal — so a constant naming a class built later is left off rather than
        // copied across as the twin
        // the layouts run alongside, and a NULL entry is a class whose instances stay
        // where the body built them — see `By_MovedInstance` for what a non-NULL one buys
        let layouts = twins
            .iter()
            .map(|class| match instance_layout_symbol(module, class) {
                Some(symbol) => symbol,
                None => "NULL".to_string(),
            })
            .collect::<Vec<_>>()
            .join(", ");
        let _ = writeln!(
            twin_init,
            "    PyObject *by_twin[{count}];\n\
             \x20   PyObject *by_type[{count}] = {{NULL}};\n\
             \x20   static const By_Field *const by_layout[{count}] = {{{layouts}}};\n\
             \x20   By_Twins by_twins = {{by_twin, by_type, by_layout, {count}, PyDict_New()}};\n\
             \x20   if (by_twins.moved == NULL) return -1;"
        );
        // and the bodies those `class` statements wrote, which is where a class-level
        // constant's value comes from — the twin has been through its own decorators by
        // now, and `By_RunModuleBody` says what that costs. borrowed from `by_bodies`,
        // which is held for the whole of this function
        // a decorated method takes its value from here too, so the bodies are needed
        // whenever either asks for one
        if twins.iter().any(|class| {
            !class.constants.is_empty()
                || class
                    .methods
                    .iter()
                    .any(|method| !method.decorators.is_empty())
        }) {
            let _ = writeln!(twin_init, "    PyObject *by_body[{count}];");
            for (slot, class) in twins.iter().enumerate() {
                let _ = writeln!(
                    twin_init,
                    "    by_body[{slot}] = By_ClassBody(by_bodies, {});",
                    c_string(&class.name)
                );
            }
        }
        for (slot, class) in twins.iter().enumerate() {
            let _ = writeln!(
                twin_init,
                "    by_twin[{slot}] = By_ClassTwin(dict, {});",
                c_string(&class.name)
            );
        }
        let _ = writeln!(
            adopt_init,
            "    if (By_AdoptTwinAttributes(&by_twins) < 0) return -1;"
        );
        // and now that every type holds everything its body gave it, the methods those
        // values captured. this waits for the adoption because a table a factory installed
        // after the `class` statement is one of the values it has to reach
        let _ = writeln!(
            adopt_init,
            "    if (By_RemapTwinMethods(&by_twins) < 0) return -1;"
        );
        let names = twins
            .iter()
            .map(|class| c_string(&class.name))
            .collect::<Vec<_>>()
            .join(", ");
        // a retained interpreted definition evaluated its defaults and closed over its
        // cells while the fallback source ran, so one naming a class of this module
        // captured the *twin* — and the name it was read from now answers the type that
        // replaced it. a body comparing the two by identity is then wrong, which is every
        // sentinel-by-identity api at once. done here because it is the last point where
        // both arrays still stand
        for function in module.all_functions().filter(|function| function.defers()) {
            let _ = writeln!(
                twin_remap,
                "    By_SettleTwins({}, &by_twins);",
                function.interpreted_symbol(module.name.dotted())
            );
        }
        let _ = writeln!(
            twin_remap,
            "    {{ static const char *const by_name[] = {{{names}}};\n\
             \x20     int by_remapped = By_RemapTwinAliases(dict, &by_twins, by_name);\n\
             \x20     for (Py_ssize_t by_at = 0; by_at < {count}; by_at++) Py_XDECREF(by_twin[by_at]);\n\
             \x20     Py_CLEAR(by_twins.moved);\n\
             \x20     if (by_remapped < 0) return -1; }}"
        );
    }

    let mut class_init = String::new();
    // a class decorator is arbitrary python handed the class, and what it reads is the
    // class *body* — its annotations, its class-level defaults, what its `__dict__`
    // holds. none of that is on the emitted type until the twin's attributes have been
    // adopted, so every decorator waits for that and runs in a pass of its own.
    //
    // what this costs is a class resolving an in-module base *through the namespace* —
    // the mixed `class C(Base, Mixin)` shape — which now stands on the emitted type
    // rather than on what `Base`'s decorator returned. that is already what a class on
    // an in-module base alone does, which builds on `Base`'s type object directly
    let mut class_decorate = String::new();
    // which slot of the twin arrays a class occupies. it is counted rather than looked up
    // by name, because `twins` is this same walk filtered on `exported` and two classes in
    // one module can be given the same name
    let mut exported_so_far = 0;
    for class in &module.classes {
        let type_name = class.type_name(module.name.dotted());
        let slot = class.exported.then(|| {
            let slot = exported_so_far;
            exported_so_far += 1;
            slot
        });
        let ready = if built_ahead(module, class) {
            // already built, by the one construction open to it
            String::new()
        } else if !heap_type(module, class) {
            format!("    if (PyType_Ready(&{type_name}) < 0) return -1;")
        } else if let Some(base) =
            class
                .base
                .as_ref()
                .and_then(ClassBase::in_module)
                .and_then(|base| {
                    module
                        .classes
                        .iter()
                        .find(|candidate| candidate.name == base)
                })
        {
            // the base is built first — a class is declared before anything that
            // extends it, and the module keeps that order. what the base *became* is
            // still a runtime answer: one that fell back to its interpreted definition
            // is somebody else's class, and a spec cannot always be built on one
            format!(
                "    {{\n\
                 {}\
                 \x20     if ({type_name} == NULL) return -1; }}",
                external_construction(
                    module,
                    class,
                    &type_name,
                    &format!(
                        "PyTuple_Pack(1, {}_OBJ)",
                        base.type_name(module.name.dotted())
                    ),
                    slot,
                )
            )
        } else if let Some(externals) = class.base.as_ref().and_then(ClassBase::external) {
            let count = externals.len();
            // a keyword-only class header — `class C(metaclass=M):` — has no bases at
            // all, and an empty array is not a declaration C accepts
            let (declare, resolve, pack, releases) = if count == 0 {
                (
                    String::new(),
                    String::new(),
                    "PyTuple_New(0)".to_string(),
                    String::new(),
                )
            } else {
                let mut resolve = String::new();
                for (slot, path) in externals.iter().enumerate() {
                    let _ = write!(
                        resolve,
                        "{}",
                        resolve_dotted(path, &format!("by_base[{slot}]"), "return -1;")
                    );
                }
                let packed = (0..count)
                    .map(|slot| format!("by_base[{slot}]"))
                    .collect::<Vec<_>>()
                    .join(", ");
                (
                    format!("\x20     PyObject *by_base[{count}];\n"),
                    resolve,
                    format!("PyTuple_Pack({count}, {packed})"),
                    (0..count)
                        .map(|slot| format!("Py_DECREF(by_base[{slot}]);"))
                        .collect::<Vec<_>>()
                        .join(" "),
                )
            };
            // the tuple goes to `By_BuildClass`, which owns it from there — including
            // when packing it failed and it is NULL
            format!(
                "    {{\n\
                 {declare}\
                 {resolve}\
                 {}\
                 \x20     {releases}\n\
                 \x20     if ({type_name} == NULL) return -1; }}",
                external_construction(module, class, &type_name, &pack, slot)
            )
        } else {
            format!(
                "    {type_name} = PyType_FromSpec(&{type_name}_spec);\n\
                 \x20   if ({type_name} == NULL) return -1;"
            )
        };
        // everything that only makes sense once this class's type exists. a class the
        // layout guard was allowed to leave unbuilt has none, so the whole run is put
        // behind a test of it rather than emitted straight into `class_init`
        let mut installed = String::new();
        if !ready.is_empty() {
            let _ = writeln!(installed, "{ready}");
        }
        // the wrappers python published for the *rest* of a shared slot's group come off
        // first, so everything below — a decorator above all — is handed a class whose
        // surface is the body the `class` statement wrote
        let unpublished = published_beyond_the_body(class);
        if !unpublished.is_empty() {
            let listed = unpublished
                .iter()
                .map(|name| c_string(name))
                .collect::<Vec<_>>()
                .join(", ");
            let _ = writeln!(
                installed,
                "    {{ static const char *const by_unpublished[] = {{{listed}, NULL}};\n\
                 \x20     if (By_UnpublishSlotNames({type_name}_OBJ, by_unpublished) < 0) return -1; }}"
            );
        }
        // before anything else asks the type to build an instance: the assignment is what
        // gives the class its `tp_new`, and until it has run the type still allocates the
        // way `object` does
        if publishes_new(class).is_some() {
            let _ = writeln!(
                installed,
                "    if (By_PublishNew({type_name}_OBJ, &{type_name}_new_def) < 0) return -1;"
            );
        }
        // a `@property` is published by building the object and putting it in the type's
        // dict, which is what a `class` statement does — see `By_PublishProperty`. it goes
        // here rather than after the adoption because the adoption carries a name the type
        // does not already hold, and the name a property is under is exactly one of those:
        // left later, the twin's own `property` would be carried first and this would
        // replace it, so the compiled halves would be reached through a second object
        for property in &class.properties {
            let symbol = property_symbol(&type_name, &property.name);
            let defs = property_halves(class, property)
                .map(|(half, body)| match body {
                    Some(_) => format!("&{symbol}_{half}_def"),
                    None => "NULL".to_string(),
                })
                .collect::<Vec<_>>()
                .join(", ");
            let _ = writeln!(
                installed,
                "    if (By_PublishProperty({type_name}_OBJ, {}, {defs}) < 0) return -1;",
                c_string(&property.name)
            );
        }
        // the type exists from here, so it is what stands for this class's twin in every
        // remap below. a class built in the layout guard was built before the array even
        // existed, which is why this is not written where the construction is
        if let Some(slot) = slot {
            let _ = writeln!(installed, "    by_type[{slot}] = {type_name}_OBJ;");
        }
        // the awaitable `__anext__` hands back is a type of its own, and an unreadied
        // type has no `tp_free` — `PyObject_New` on one segfaults rather than failing
        if class
            .resume
            .as_ref()
            .is_some_and(|resume| resume.surface == Surface::AsyncGenerator)
        {
            let _ = writeln!(
                installed,
                "    if (PyType_Ready(&{type_name}_asend_type) < 0) return -1;"
            );
        }
        // `asyncio.iscoroutine` tests `isinstance(x, collections.abc.Coroutine)`, so
        // a type that answers `__await__` still has to say it is one
        if class
            .resume
            .as_ref()
            .is_some_and(|resume| resume.surface == Surface::Coroutine)
        {
            let _ = writeln!(
                installed,
                "    if (By_RegisterCoroutine({type_name}_OBJ) < 0) return -1;"
            );
        }
        // a class-level constant comes from the interpreted definition, which
        // evaluated it at class-definition time. copying keeps the *same* object,
        // which is what evaluating once means
        for constant in &class.constants {
            // a class no interpreted `class` statement wrote has no body to take one off
            let Some(slot) = slot else { continue };
            let _ = writeln!(
                installed,
                "    if (By_CopyClassConstant(by_body[{slot}], (PyTypeObject *){type_name}_OBJ, {}, &by_twins) < 0) return -1;",
                c_string(constant)
            );
        }
        // and a field's class-level value goes the same way: the copy has put it in the
        // type's dict, and this takes it into the cell and puts the one descriptor that
        // answers both a class read and an instance read over the top of it
        for field in &class.fields {
            let Some((cell, holds)) = field_default(module, class, field) else {
                continue;
            };
            let setter = if class.writable() {
                format!("{type_name}_set_{}", field.name)
            } else {
                "NULL".to_string()
            };
            let _ = writeln!(
                installed,
                "    if (By_HoldFieldDefault((PyTypeObject *){type_name}_OBJ, {}, &{cell}, {}, {type_name}_get_{}, {setter}, {type_name}_has_{}) < 0) return -1;",
                c_string(&field.name),
                i32::from(holds),
                field.name,
                field.name
            );
        }
        // a dunder the body assigned is now in the type's dict under its own name, and
        // the slot emitted for it reads the value back out of a cell rather than looking
        // it up on every call. that is what keeps `repr(x)` and `x.__repr__()` on the one
        // object, and it has to come *after* the copy that put the object there
        for alias in &class.slot_aliases {
            if alias.unsupported {
                continue;
            }
            let _ = writeln!(
                installed,
                "    if (By_HoldSlotAlias((PyTypeObject *){type_name}_OBJ, {}, &{}) < 0) return -1;",
                c_string(&alias.name),
                alias_cell(&type_name, &alias.name)
            );
        }
        // a decorated method is decorated *after* the type exists, which is the only
        // place a spec leaves for it. the whole list goes in one call, so the runtime
        // folds them onto a single writable stand-in for the method rather than
        // re-reading the type between two of them
        for method in &class.methods {
            if method.decorators.is_empty() {
                continue;
            }
            let names = method
                .decorators
                .iter()
                .map(|decorator| c_string(&decorator.dotted()))
                .collect::<Vec<_>>()
                .join(", ");
            // a class with no interpreted `class` statement has no body to take the
            // decorator's answer from — and never ran the decorators either, so
            // applying them there is the only application rather than a second one
            let body = slot.map_or_else(|| "NULL".to_string(), |slot| format!("by_body[{slot}]"));
            let _ = writeln!(
                installed,
                "    {{ static const char *const by_decorators[] = {{{names}}};\n\
                 \x20     if (By_DecoratedMethod({body}, (PyTypeObject *){type_name}_OBJ, dict, {}, {}, by_decorators, {}, &by_twins) < 0) return -1; }}",
                c_string(&class.name),
                c_string(&method.name),
                method.decorators.len()
            );
        }
        // a closure environment is a real type with a real layout, and nothing
        // should be able to name it
        if class.exported {
            let _ = writeln!(
                installed,
                "    if (PyDict_SetItemString(dict, \"{}\", {type_name}_OBJ) < 0) return -1;",
                class.name
            );
            // and its own decorators after the adoption, because a decorator *reads* the
            // class it is handed and every one of those reads has to see the body the
            // `class` statement wrote. they go after the namespace entry too, because a
            // decorator replaces it — which is where every construction looks
            for decorator in class.decorators.iter().rev() {
                let _ = writeln!(
                    class_decorate,
                    "    if (By_ApplyDecorator(dict, {}, {}) < 0) return -1;",
                    c_string(&class.name),
                    c_string(&decorator.dotted())
                );
            }
        }
        // a class the layout guard was allowed to leave unbuilt has a NULL here, and
        // everything above would have written through it. the namespace entry it would
        // have replaced is left holding the interpreted definition, which is what the
        // class means from now on — so that definition is what stands for its own twin
        // in every remap below, rather than the nothing a never-filled slot would say.
        // its own decorators are applied either way, because `class_decorate` applies
        // them to the namespace entry rather than to the type
        if declines_on_its_own(module, class) {
            let stands = slot.map_or_else(String::new, |slot| {
                format!("    else {{ by_type[{slot}] = by_twin[{slot}]; }}\n")
            });
            let _ = write!(
                class_init,
                "    if ({type_name} != NULL) {{\n{installed}    }}\n{stands}"
            );
        } else {
            class_init.push_str(&installed);
        }
    }

    let mut literal_init = String::new();
    for (index, literal) in collect_string_literals(module).iter().enumerate() {
        let _ = writeln!(
            literal_init,
            "    by_str{index} = By_InternedStr({});\n    if (by_str{index} == NULL) return -1;",
            c_string_sized(literal)
        );
    }
    for (index, literal) in collect_bytes_literals(module).iter().enumerate() {
        let _ = writeln!(
            literal_init,
            "    by_bytes{index} = PyBytes_FromStringAndSize({}, {});\n    if (by_bytes{index} == NULL) return -1;",
            c_byte_string(literal),
            literal.len()
        );
    }

    // taken while the interpreted definition is still the one under this name
    let mut interpreted_init = String::new();
    for function in module.all_functions().filter(|function| function.defers()) {
        let handle = function.interpreted_symbol(module.name.dotted());
        match &function.owner {
            // a method's twin is an attribute of the interpreted class, which is what
            // sits under the class's name until the compiled type replaces it
            Some(owner) => {
                let _ = writeln!(
                    interpreted_init,
                    "    {{ PyObject *by_cls = PyDict_GetItemString(dict, {});\n\
                     \x20     if (by_cls != NULL) {{\n\
                     \x20         {handle} = PyObject_GetAttrString(by_cls, {});\n\
                     \x20         if ({handle} == NULL) PyErr_Clear();\n\
                     \x20     }} }}",
                    c_string(owner),
                    c_string(&function.name)
                );
            }
            None => {
                let _ = writeln!(
                    interpreted_init,
                    "    {handle} = PyDict_GetItemString(dict, {});\n\x20   Py_XINCREF({handle});",
                    c_string(&function.name)
                );
            }
        }
    }

    // the body is run either way; what a class-level constant needs is what each `class`
    // statement wrote *before* its own decorators, which only the capturing run keeps
    let run_body = if captures_bodies {
        "\x20   PyObject *by_bodies = NULL;\n\
         \x20   if (by_fallback_source[0] != '\\0') {\n\
         \x20       by_bodies = By_RunModuleBody(&by_fallback, dict);\n\
         \x20       if (by_bodies == NULL) return -1;\n\
         \x20   }\n"
            .to_string()
    } else {
        "\x20   if (by_fallback_source[0] != '\\0') {\n\
         \x20       PyObject *result = By_ExecModuleBody(&by_fallback, dict);\n\
         \x20       if (result == NULL) return -1;\n\
         \x20       Py_DECREF(result);\n\
         \x20   }\n"
            .to_string()
    };
    // last, after every hand this module lays on its own classes: a decorator that
    // replaced a method, a twin whose attributes were adopted. arming here is what makes
    // the licence answer for what the class *ends up* holding rather than what it was
    // built with
    let arm_dispatch: String = dispatch_licences(module)
        .iter()
        .filter_map(|(class, method)| {
            let owner = class_named(module, class)?;
            let body = module.all_functions().find(|function| {
                function.owner.as_deref() == Some(class) && function.name == *method
            })?;
            Some(format!(
                "    By_ArmMethod(&{}, {}_OBJ, {}, (PyCFunction){});\n",
                dispatch_licence(module, class, method),
                owner.type_name(module.name.dotted()),
                c_string(method),
                body.wrapper_symbol(module.name.dotted()),
            ))
        })
        .collect();
    // every method table this module owns, so the docstrings baked into them can be
    // taken back out where the *running* interpreter is one that compiles without
    // docstrings — see `By_StripDocsAtOO`
    let mut strip_docs = String::from("    By_StripDocsAtOO(by_methods);\n");
    if module.shims.is_some() {
        strip_docs.push_str("    By_StripDocsAtOO(by_forwarded);\n");
    }
    for class in &module.classes {
        let _ = writeln!(
            strip_docs,
            "    By_StripDocsAtOO({}_methods);",
            class.type_name(module.name.dotted())
        );
    }
    // the forwarders go in before the natives, and before the decorators: a decorator
    // in python is handed the `function` the `def` made, and the forwarder is what
    // stands in for that here
    let publish_forwarders = match &module.shims {
        Some(shims) => format!(
            "    if (By_PublishForwarders(module, dict, by_forwarded, {}, {}) < 0) return -1;\n",
            shims.functions.len(),
            c_string(&shims.installer)
        ),
        None => String::new(),
    };
    let _ = write!(
        out,
        "static int by_exec(PyObject *module) {{\n\
         \x20   PyObject *dict = PyModule_GetDict(module);\n\
         \x20   if (dict == NULL) return -1;\n\
         \x20   by_module_dict = dict;\n\
         \x20   /* before a type is built off one of these tables, or a function\n\
         \x20    * installed from one: what `__doc__` may say depends on the level\n\
         \x20    * this interpreter is running at, not the one the build saw */\n\
         {strip_docs}\
         \x20   /* before the body below can read a global, and before it can write\n\
         \x20    * `__builtins__` itself: what this module resolves builtins through\n\
         \x20    * is settled by the import, the way an interpreted module's is */\n\
         \x20   if (By_BindBuiltins(dict) < 0) return -1;\n\
         \x20   /* before anything below can bind a name in it: a memo of a global\n\
         \x20    * is only allowed to exist while this namespace is being watched,\n\
         \x20    * and a second execution of this module invalidates every memo the\n\
         \x20    * first one left behind */\n\
         \x20   By_WatchModule(dict);\n\
         {literal_init}\
         {run_body}\
         {layout_guard}\
         {interpreted_init}\
         {twin_init}\
         {class_init}\
         {adopt_init}\
         {class_decorate}\
         {twin_remap}\
         {release_bodies}\
         {publish_forwarders}\
         \x20   if (PyModule_AddFunctions(module, by_methods) < 0) return -1;\n\
         {decorators}\
         {arm_dispatch}\
         \x20   return 0;\n\
         }}\n\n\
         static PyModuleDef_Slot by_slots[] = {{\n\
         \x20   {{Py_mod_exec, (void *)by_exec}},\n\
         #if PY_VERSION_HEX >= 0x030D0000\n\
         \x20   /* compiled functions hold no shared mutable state: every register\n\
         \x20    * is a frame local and refcounting goes through cpython's own\n\
         \x20    * macros. without this slot a free-threaded interpreter re-enables\n\
         \x20    * the GIL for the whole process the moment this module is imported */\n\
         \x20   {{Py_mod_gil, Py_MOD_GIL_NOT_USED}},\n\
         #endif\n\
         \x20   {{0, NULL}}\n\
         }};\n\n\
         static struct PyModuleDef by_module = {{\n\
         \x20   PyModuleDef_HEAD_INIT, \"{last}\", NULL, 0, NULL, by_slots, NULL, NULL, NULL\n\
         }};\n\n\
         PyMODINIT_FUNC {}(void) {{\n\
         \x20   /* before `by_module` itself is handed over: a module definition is a\n\
         \x20    * struct this build laid out, so a mismatched interpreter must be turned\n\
         \x20    * away without reading one */\n\
         \x20   if (!By_InterpreterMatches()) return NULL;\n\
         \x20   return PyModuleDef_Init(&by_module);\n\
         }}\n",
        module.init_symbol()
    );
    out
}

fn mangle(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use by_ir::builder::FunctionBuilder;
    use by_ir::function::{CallConvention, Declined};
    use by_ir::rtype::IntWidth;
    use by_ir::verify::verify;

    fn module_with(function: Function) -> ModuleIr {
        ModuleIr {
            name: by_ir::ModuleName::new("app"),
            functions: vec![function],
            declined: Vec::new(),
            classes: Vec::new(),
            gradual: Vec::new(),
            promoted: Vec::new(),
            lines: None,
            fallback_source: None,
            fallback_code: None,
            shims: None,
        }
    }

    fn add() -> Function {
        let mut builder = FunctionBuilder::new("add", RType::INT);
        let a = builder.param("a", RType::INT);
        let b = builder.param("b", RType::INT);
        let sum = builder.temp(RType::INT);
        builder.push(Op::IntBinary {
            dest: sum,
            op: BinOp::Add,
            lhs: Value::Register(a),
            rhs: Value::Register(b),
        });
        builder.terminate(Terminator::Return(Value::Register(sum)));
        builder.finish()
    }

    /// a module-level function that builds an instance of `class`, which is the plainest
    /// way for compiled code to read that class's appended storage as its own struct
    ///
    /// a class the module reaches into like this is one whose refusal has to be the whole
    /// module's — see `declines_on_its_own`
    fn constructs(class: &str) -> Function {
        let mut builder = FunctionBuilder::new("make", RType::OBJECT);
        let tag = builder.param("tag", RType::OBJECT);
        let made = builder.temp(RType::OBJECT);
        builder.push(Op::NewInstance {
            dest: made,
            class: class.to_string(),
            fields: vec![Some(Value::Register(tag))],
        });
        builder.terminate(Terminator::Return(Value::Register(made)));
        builder.finish()
    }

    /// a method that reads a field off an instance of `class` — the direct struct read
    /// that only the emitted type's layout has an offset for
    fn reads_a_field_of(class: &str) -> Function {
        let mut builder = FunctionBuilder::new("look", RType::OBJECT);
        let held = builder.param(
            "held",
            RType::Instance {
                class: class.to_string(),
                exact: true,
            },
        );
        let tag = builder.temp(RType::OBJECT);
        builder.push(Op::GetField {
            dest: tag,
            receiver: Value::Register(held),
            class: class.to_string(),
            field: "tag".to_string(),
        });
        builder.terminate(Terminator::Return(Value::Register(tag)));
        builder.finish()
    }

    /// a method that writes `spare` on an instance of `class` through the dynamic form —
    /// the receiver boxed to an object first, which is what the frontend emits where it
    /// has no field to write instead
    fn writes_an_unheld_attribute(class: &str) -> Function {
        let mut builder = FunctionBuilder::new("fill", RType::NONE);
        let held = builder.param(
            "held",
            RType::Instance {
                class: class.to_string(),
                exact: true,
            },
        );
        let boxed = builder.temp(RType::OBJECT);
        builder.push(Op::Box {
            dest: boxed,
            src: Value::Register(held),
        });
        let status = builder.temp(RType::BIT);
        builder.push(Op::SetAttr {
            dest: status,
            receiver: Value::Register(boxed),
            name: "spare".to_string(),
            value: Value::Int(1),
        });
        builder.terminate(Terminator::Return(Value::None));
        builder.finish()
    }

    #[test]
    fn a_native_signature_uses_the_mangled_symbol() {
        let module = module_with(add());
        let c = emit_module(&module);
        assert!(c.contains("static ByTagged by_app_add(ByTagged r0, ByTagged r1)"));
    }

    #[test]
    fn a_fallible_function_gets_an_error_label_and_an_error_check() {
        let c = emit_module(&module_with(add()));
        assert!(c.contains("if (BY_UNLIKELY(by_t == BY_INT_ERROR)) goto by_error;"));
        assert!(c.contains("by_error: ;"));
    }

    /// python leaves a name bound to what it was when the operation that would have
    /// rebound it raises, and an `except` handler in the same function reads it. a
    /// destination released and stored before the test is a `NULL` that handler goes
    /// on to hand to cpython
    #[test]
    fn a_failing_operation_does_not_write_its_destination() {
        let mut builder = FunctionBuilder::new("lookup", RType::OBJECT);
        let table = builder.param("table", RType::OBJECT);
        let key = builder.param("key", RType::OBJECT);
        let held = builder.local("held", RType::OBJECT);
        builder.push(Op::GetItem {
            dest: held,
            container: Value::Register(table),
            index: Value::Register(key),
        });
        builder.terminate(Terminator::Return(Value::Register(held)));
        let c = emit_module(&module_with(builder.finish()));
        let check = c
            .find("if (BY_UNLIKELY(by_t == NULL)) goto by_error;")
            .expect("the failure is tested");
        let release = c
            .find("Py_XDECREF(r2);")
            .expect("the old value is released");
        assert!(
            check < release,
            "the error edge must be taken before the destination is touched: {c}"
        );
    }

    /// the same rule at the two shapes that emit their own write rather than going
    /// through `assign_checked`. neither destination is a name the source can read,
    /// so the difference is not observable — which is exactly why they have to be
    /// held to the rule rather than left to be noticed
    #[test]
    fn an_operation_that_writes_its_own_destination_tests_first() {
        let mut builder = FunctionBuilder::new("step", RType::BIT);
        let inner = builder.param("inner", RType::OBJECT);
        let buffer = builder.param("buffer", RType::Array(Box::new(RType::BIT)));
        let stepped = builder.temp(RType::Tuple(Box::new([RType::OBJECT, RType::BIT])));
        let pushed = builder.temp(RType::BIT);
        builder.push(Op::DelegateStep {
            dest: stepped,
            inner: Value::Register(inner),
            sent: Value::Register(inner),
        });
        builder.push(Op::ArrayPush {
            dest: pushed,
            array: Value::Register(buffer),
            value: Value::Bool(true),
        });
        builder.terminate(Terminator::Return(Value::Register(pushed)));
        let c = emit_module(&module_with(builder.finish()));
        let before = |check: &str, write: &str| {
            let check = c.find(check).unwrap_or_else(|| panic!("no `{check}`: {c}"));
            let write = c.find(write).unwrap_or_else(|| panic!("no `{write}`: {c}"));
            assert!(check < write, "`{check}` must precede `{write}`: {c}");
        };
        before(
            "if (BY_UNLIKELY(by_t == NULL)) goto by_error;",
            "r2.f0 = by_t;",
        );
        before("if (by_a == NULL) goto by_error;", "r3 = 0;");
    }

    /// an unboxed counter compared against a bound that is still tagged. the
    /// shortness test cannot be hoisted — the bound is an ordinary `int` — but only
    /// its failing side boxes, and boxing is the only part that can raise
    #[test]
    fn only_the_boxing_side_of_an_unboxed_comparison_carries_the_error_test() {
        let mut builder = FunctionBuilder::new("count", RType::BIT);
        let counter = builder.param("counter", RType::fixed(IntWidth::I64));
        let bound = builder.param("bound", RType::INT);
        let less = builder.temp(RType::BIT);
        builder.push(Op::IntCompare {
            dest: less,
            op: CmpOp::Lt,
            lhs: Value::Register(counter),
            rhs: Value::Register(bound),
        });
        builder.terminate(Terminator::Return(Value::Register(less)));
        let c = emit_module(&module_with(builder.finish()));

        assert!(c.contains("if (BY_LIKELY(By_IsShort(r1))) {"));
        assert!(c.contains("r2 = (char)(r0 < (int64_t) By_ShortValue(r1));"));
        assert!(c.contains("char by_t = By_I64LtSlow(r0, r1);"));

        // the point of writing the two sides apart: a loop that stays short branches
        // on the comparison itself and never on the error sentinel
        let (short, boxed) = c.split_once("} else {").expect("both sides are emitted");
        assert!(!short.contains("== 2"));
        assert!(boxed.contains("if (BY_UNLIKELY(by_t == 2)) goto by_error;"));
    }

    /// a class whose fields sit past a base's instance is the one class no other type
    /// can stand in for: the compiled reads and writes are offsets into an instance only
    /// the spec-built type allocates, so a construction that may answer with the
    /// interpreted definition instead is a write past the end of the object. it is built
    /// where a refusal can still leave the whole module interpreted, and `By_BuildClass`
    /// — which has that fallback — must never be asked about it
    #[test]
    fn a_class_appending_storage_is_never_built_through_the_fallback() {
        // a module that builds one of these is a module whose refusal is a whole-module
        // one, which is what makes the ordering below the thing under test
        let mut module = module_with(constructs("Wrapped"));
        module.classes.push(ClassIr {
            name: "Wrapped".to_string(),
            immutable: false,
            exported: true,
            base: Some(ClassBase::External(vec!["Exception".to_string()])),
            inherited_init: false,
            generic: false,
            declares_slots: false,
            constants: Vec::new(),
            properties: Vec::new(),
            slot_aliases: Vec::new(),
            fields: vec![by_ir::function::FieldDecl {
                name: "tag".to_string(),
                ty: RType::OBJECT,
                default: None,
                optional: false,
                defaulted_by: None,
            }],
            decorators: Vec::new(),
            methods: Vec::new(),
            resume: None,
            keywords: Vec::new(),
        });
        let c = emit_module(&module);
        assert!(
            c.contains(
                "By_app_Wrapped_Type = By_SpecClass(dict, \"Wrapped\", &By_app_Wrapped_Type_spec);"
            ),
            "the spec construction is the guard's: {c}"
        );
        assert!(
            !c.contains("By_BuildClass"),
            "no fallback construction: {c}"
        );
        // and a refusal is the whole module's, taken before anything of this module's
        // own is installed
        let refusal = c
            .find("if (By_app_Wrapped_Type == NULL) return 0;")
            .expect("a refusal leaves the module interpreted");
        let install = c
            .find("PyModule_AddFunctions")
            .expect("the natives are installed");
        assert!(refusal < install, "the refusal comes first: {c}");
    }

    /// a class appending storage: `tp_alloc` hands back the *object*, and its fields are
    /// somewhere past it. writing them through the object pointer would land on the
    /// base's own data — 8 bytes into a `complex`, say — so the construction has to
    /// reach the storage the same way every other field access does
    #[test]
    fn constructing_a_class_that_appends_storage_writes_past_the_base() {
        let mut builder = FunctionBuilder::new("make", RType::OBJECT);
        let tag = builder.param("tag", RType::OBJECT);
        let made = builder.temp(RType::OBJECT);
        builder.push(Op::NewInstance {
            dest: made,
            class: "Wrapped".to_string(),
            fields: vec![Some(Value::Register(tag))],
        });
        builder.terminate(Terminator::Return(Value::Register(made)));
        let mut module = module_with(builder.finish());
        module.classes.push(appending_class());
        let c = emit_module(&module);

        assert!(
            c.contains(
                "By_app_Wrapped *by_new = ((By_app_Wrapped *)By_TypeData(by_obj, By_app_Wrapped_Type_OBJ));"
            ),
            "the storage is found, not assumed: {c}"
        );
        assert!(
            !c.contains("(By_app_Wrapped *)by_type->tp_alloc"),
            "the allocation is an object, not a field struct: {c}"
        );
        // and the value handed on is still the object — the field storage is not one
        assert!(c.contains("r1 = (PyObject *)by_obj;"), "{c}");
    }

    /// the same construction for a class that owns its layout: the object *is* the
    /// storage there, and finding it must not cost a lookup
    #[test]
    fn constructing_a_class_that_owns_its_layout_casts_the_object() {
        let mut builder = FunctionBuilder::new("make", RType::OBJECT);
        let tag = builder.param("tag", RType::OBJECT);
        let made = builder.temp(RType::OBJECT);
        builder.push(Op::NewInstance {
            dest: made,
            class: "Owned".to_string(),
            fields: vec![Some(Value::Register(tag))],
        });
        builder.terminate(Terminator::Return(Value::Register(made)));
        let mut module = module_with(builder.finish());
        let mut owned = appending_class();
        owned.name = "Owned".to_string();
        owned.base = None;
        module.classes.push(owned);
        let c = emit_module(&module);

        assert!(
            c.contains("By_app_Owned *by_new = (By_app_Owned *)by_obj;"),
            "no lookup for a class that owns its layout: {c}"
        );
        assert!(!c.contains("By_TypeData(by_obj"), "{c}");
    }

    /// two classes appending storage, one over the other. the inner one is built on the
    /// *finished* type of the outer rather than on the interpreted definition, because
    /// the interpreted definition's `tp_dealloc` is `subtype_dealloc` — which picks the
    /// deallocator to chain to out of `Py_TYPE(self)`, finds this class's own and calls
    /// it back until the stack runs out
    #[test]
    fn appended_storage_over_appended_storage_stands_on_the_emitted_base() {
        let mut module = module_with(constructs("Deeper"));
        module.classes.push(appending_class());
        module.classes.push(deeper_class());
        let c = emit_module(&module);

        assert!(
            c.contains(
                "By_app_Deeper_Type = By_SpecSubclass(dict, \"Deeper\", &By_app_Deeper_Type_spec, \"Wrapped\", By_app_Wrapped_Type_OBJ);"
            ),
            "the subclass stands on the emitted base: {c}"
        );
        // the base is still the one construction that has to answer from reality, and
        // it is built first
        let base = c
            .find("By_app_Wrapped_Type = By_SpecClass(dict, \"Wrapped\"")
            .expect("the base is built from the twin's own bases");
        let derived = c
            .find("By_app_Deeper_Type = By_SpecSubclass(")
            .expect("the subclass is built after it");
        assert!(base < derived, "the base is built first: {c}");
        // and a refusal is still the whole module's
        assert!(
            c.contains("if (By_app_Deeper_Type == NULL) return 0;"),
            "{c}"
        );
    }

    /// the same pair, with nothing constructing either. `Wrapped` is still the whole
    /// module's to refuse — `Deeper` is built on its type object, and a NULL there is not
    /// something a bases tuple can be packed from — where `Deeper`, which nothing stands
    /// on and nothing builds, is left out on its own
    #[test]
    fn a_class_another_stands_on_refuses_the_whole_module() {
        let mut module = module_with(add());
        module.classes.push(appending_class());
        module.classes.push(deeper_class());
        let c = emit_module(&module);

        assert!(
            c.contains("if (By_app_Wrapped_Type == NULL) return 0;"),
            "the base a class stands on refuses for the module: {c}"
        );
        assert!(
            !c.contains("if (By_app_Deeper_Type == NULL) return 0;"),
            "nothing stands on the subclass, so it refuses alone: {c}"
        );
    }

    /// a class appending storage that no compiled code reaches into is left out on its
    /// own: the construction is still the guard's, but a NULL is carried forward instead
    /// of ending the init, and every step that would have installed the type is put
    /// behind a test of it
    #[test]
    fn a_class_nothing_reaches_into_declines_on_its_own() {
        let mut module = module_with(add());
        module.classes.push(appending_class());
        let c = emit_module(&module);

        assert!(
            c.contains(
                "By_app_Wrapped_Type = By_SpecClass(dict, \"Wrapped\", &By_app_Wrapped_Type_spec);"
            ),
            "the construction is unchanged: {c}"
        );
        assert!(
            !c.contains("return 0;\n    By_app_Wrapped_Type")
                && !c.contains("Wrapped_Type == NULL) return 0"),
            "nothing reaches into it, so the module is not refused: {c}"
        );
        assert!(
            c.contains("if (By_app_Wrapped_Type != NULL) {"),
            "the install waits on the type: {c}"
        );
        assert!(
            c.contains("if (PyDict_SetItemString(dict, \"Wrapped\", By_app_Wrapped_Type_OBJ) < 0) return -1;"),
            "and installs it where there is one: {c}"
        );
    }

    /// and where there is not, the interpreted definition keeps the name — so it is what
    /// stands for its own twin in the remaps below, rather than the nothing a slot left
    /// unfilled would say. without this an attribute of some *other* compiled class that
    /// named this one would be dropped rather than carried
    #[test]
    fn a_class_left_unbuilt_stands_for_its_own_twin() {
        let mut module = module_with(add());
        module.classes.push(appending_class());
        let c = emit_module(&module);

        assert!(
            c.contains("else { by_type[0] = by_twin[0]; }"),
            "the twin stands in for itself: {c}"
        );
    }

    /// a module-level function that builds one is exactly the reader the whole-module
    /// refusal exists for: it reads the instance as its own struct, at an offset the
    /// interpreted definition's instance does not reach
    #[test]
    fn a_class_a_function_builds_refuses_the_whole_module() {
        let mut module = module_with(constructs("Wrapped"));
        module.classes.push(appending_class());
        let c = emit_module(&module);

        assert!(
            c.contains("if (By_app_Wrapped_Type == NULL) return 0;"),
            "a module function reaching into it refuses for the module: {c}"
        );
    }

    /// a module holding a `__slots__` class that is written an attribute its layout does
    /// not hold keeps the whole-module refusal
    ///
    /// such a class has no instance dict, so the write lands nowhere — its instances are
    /// ones only the interpreted definition can build. the frontend no longer hands over
    /// IR like this: an unpacking target is a field like any other now, and a write of a
    /// name the layout chain has nowhere for declines. the IR is built by hand here for
    /// that reason, because this is the guard for a lowering added *later* that finds a
    /// way past both
    #[test]
    fn a_slotted_class_written_an_attribute_it_does_not_hold_keeps_the_module_refusing() {
        let mut module = module_with(add());
        module.classes.push(appending_class());
        let mut wakeup = appending_class();
        wakeup.name = "Wakeup".to_string();
        wakeup.base = None;
        wakeup.declares_slots = true;
        wakeup.methods.push(writes_an_unheld_attribute("Wakeup"));
        module.classes.push(wakeup);
        let c = emit_module(&module);

        assert!(
            c.contains("if (By_app_Wrapped_Type == NULL) return 0;"),
            "a class the module cannot answer for keeps the whole-module refusal: {c}"
        );
    }

    /// and the same class with the attribute in its layout does not, because then the
    /// write is an offset the emitted type really has
    #[test]
    fn a_class_that_holds_what_it_is_written_lets_the_module_narrow() {
        let mut module = module_with(add());
        module.classes.push(appending_class());
        let mut wakeup = appending_class();
        wakeup.name = "Wakeup".to_string();
        wakeup.base = None;
        wakeup.declares_slots = true;
        wakeup.fields.push(by_ir::function::FieldDecl {
            name: "spare".to_string(),
            ty: RType::OBJECT,
            default: None,
            optional: false,
            defaulted_by: None,
        });
        wakeup.methods.push(writes_an_unheld_attribute("Wakeup"));
        module.classes.push(wakeup);
        let c = emit_module(&module);

        assert!(
            !c.contains("if (By_app_Wrapped_Type == NULL) return 0;"),
            "a class that holds the attribute does not hold the module: {c}"
        );
    }

    /// a class keeping a dict keeps it in a word of its own, and tells its type where
    ///
    /// the offset is what lets the shadow test at a call site be a load rather than a
    /// call into libpython, so it is written out at both ends: as a `__dictoffset__`
    /// member, which is the only way a type built from a spec can carry one, and as an
    /// `offsetof` at every site that asks
    #[test]
    fn a_class_keeping_a_dict_names_the_word_it_keeps_it_in() {
        let mut module = module_with(add());
        let mut open = appending_class();
        open.name = "Open".to_string();
        open.base = None;
        module.classes.push(open);
        let c = emit_module(&module);

        assert!(
            c.contains("    PyObject *by_dict;"),
            "the dict is a word in the struct: {c}"
        );
        assert!(
            c.contains(
                "{\"__dictoffset__\", BY_DICT_OFFSET_MEMBER,\n\
                 \x20    offsetof(By_app_Open, by_dict), BY_DICT_OFFSET_FLAGS}"
            ),
            "the type is told where the word is: {c}"
        );
        assert!(
            c.contains("{Py_tp_members, (void *)By_app_Open_Type_members}"),
            "the members table reaches the type: {c}"
        );
    }

    /// a class declaring `__slots__` still reserves the word where something under it
    /// keeps a dict
    ///
    /// the two disagree about whether a dict exists and cannot disagree about where the
    /// fields start: a subclass's struct is a clone of its base's, and a direct call on a
    /// base-typed receiver reads the base's offsets out of a subclass instance
    #[test]
    fn a_slotted_base_reserves_the_word_its_subclass_keeps_a_dict_in() {
        let mut module = module_with(add());
        let mut tight = appending_class();
        tight.name = "Tight".to_string();
        tight.base = None;
        tight.declares_slots = true;
        module.classes.push(tight);
        let mut loose = appending_class();
        loose.name = "Loose".to_string();
        loose.base = Some(ClassBase::InModule("Tight".to_string()));
        module.classes.push(loose);
        let c = emit_module(&module);

        assert!(
            c.contains(
                "struct By_app_Tight {\n\
                 \x20   PyObject_HEAD\n\
                 \x20   PyObject *by_dict;"
            ),
            "the base reserves the word: {c}"
        );
        assert!(
            c.contains(
                "struct By_app_Loose {\n\
                 \x20   PyObject_HEAD\n\
                 \x20   PyObject *by_dict;"
            ),
            "the subclass keeps it at the same offset: {c}"
        );
        // only the subclass can reach one, so only the subclass names the offset
        assert!(
            !c.contains("offsetof(By_app_Tight, by_dict)"),
            "the slotted base publishes no dict: {c}"
        );
        assert!(
            c.contains("offsetof(By_app_Loose, by_dict)"),
            "the subclass publishes one: {c}"
        );
    }

    /// a class with no dict anywhere in its chain reserves nothing
    #[test]
    fn a_chain_that_declares_slots_throughout_keeps_the_bare_layout() {
        let mut module = module_with(add());
        let mut tight = appending_class();
        tight.name = "Tight".to_string();
        tight.base = None;
        tight.declares_slots = true;
        module.classes.push(tight);
        let mut tighter = appending_class();
        tighter.name = "Tighter".to_string();
        tighter.base = Some(ClassBase::InModule("Tight".to_string()));
        tighter.declares_slots = true;
        module.classes.push(tighter);
        let c = emit_module(&module);

        assert!(
            !c.contains("PyObject *by_dict;"),
            "nothing in the chain keeps a dict, so no word is reserved: {c}"
        );
    }

    /// a class *without* `__slots__` narrows too, and for the other reason: it keeps a
    /// dict, which is where python puts a name the layout never mentioned. it is the same
    /// place the interpreted twin would have put it
    #[test]
    fn a_class_keeping_a_dict_answers_for_an_attribute_its_layout_does_not_hold() {
        let mut module = module_with(add());
        module.classes.push(appending_class());
        let mut wakeup = appending_class();
        wakeup.name = "Wakeup".to_string();
        wakeup.base = None;
        wakeup.methods.push(writes_an_unheld_attribute("Wakeup"));
        module.classes.push(wakeup);
        let c = emit_module(&module);

        assert!(
            !c.contains("if (By_app_Wrapped_Type == NULL) return 0;"),
            "a class with a dict answers for the write itself: {c}"
        );
    }

    /// a function *typed* on the class, whose body names it in no operation at all
    ///
    /// an instance-typed register is what licenses a direct call and a pinned instance
    /// size, and it is the *type* the later stages read to decide either — so it counts as
    /// reaching into the class whether or not the body has been lowered to an operation
    /// that says so yet. for a class appending storage the emitted C is a bare
    /// `PyObject *` either way, which is exactly why this has to be asked of the type
    /// rather than read back off the C
    #[test]
    fn a_function_typed_on_the_class_holds_the_whole_module() {
        let mut builder = FunctionBuilder::new("pass_along", RType::OBJECT);
        let held = builder.param(
            "held",
            RType::Instance {
                class: "Wrapped".to_string(),
                exact: true,
            },
        );
        builder.terminate(Terminator::Return(Value::Register(held)));
        let mut module = module_with(builder.finish());
        module.classes.push(appending_class());
        let c = emit_module(&module);

        assert!(
            c.contains("if (By_app_Wrapped_Type == NULL) return 0;"),
            "a register typed on it reaches into it: {c}"
        );
    }

    /// and the answer a function hands back is a register's type one frame along: a caller
    /// taking it into an instance-typed register of its own reads it as that class
    #[test]
    fn a_function_that_answers_with_the_class_holds_the_whole_module() {
        let mut builder = FunctionBuilder::new(
            "hand_back",
            RType::Instance {
                class: "Wrapped".to_string(),
                exact: true,
            },
        );
        builder.terminate(Terminator::Unreachable);
        let mut module = module_with(builder.finish());
        module.classes.push(appending_class());
        let c = emit_module(&module);

        assert!(
            c.contains("if (By_app_Wrapped_Type == NULL) return 0;"),
            "the type it answers with reaches into it: {c}"
        );
    }

    /// a method of some *other* class that still gets a type is the same reader one step
    /// along, and refuses the same way
    #[test]
    fn a_class_a_method_of_another_class_reads_refuses_the_whole_module() {
        let mut module = module_with(add());
        module.classes.push(appending_class());
        let mut reader = appending_class();
        reader.name = "Reader".to_string();
        reader.methods.push(reads_a_field_of("Wrapped"));
        module.classes.push(reader);
        let c = emit_module(&module);

        assert!(
            c.contains("if (By_app_Wrapped_Type == NULL) return 0;"),
            "another class's method reaching into it refuses for the module: {c}"
        );
    }

    /// the state object a generator method suspends into is a class of its own, and it
    /// captured the `self` it was made from — so it names the class exactly as any other
    /// reader would. but nothing in the namespace is bound to it and nothing but the
    /// class's own methods ever builds one, so where that class has no type this is never
    /// constructed and its reads never happen. counting it would leave the narrower
    /// refusal firing on nothing, because every spec class with a generator method or a
    /// nested function has one
    #[test]
    fn a_helper_class_only_the_class_itself_builds_goes_unbuilt_with_it() {
        let mut module = module_with(add());
        module.classes.push(appending_class());
        let mut state = appending_class();
        state.name = "Wrapped_step_gen".to_string();
        state.exported = false;
        state.methods.push(reads_a_field_of("Wrapped"));
        module.classes.push(state);
        let c = emit_module(&module);

        assert!(
            !c.contains("if (By_app_Wrapped_Type == NULL) return 0;"),
            "a helper nothing else builds does not hold the module: {c}"
        );
        assert!(
            c.contains("if (By_app_Wrapped_Type != NULL) {"),
            "the class is still left out on its own: {c}"
        );
    }

    /// the same helper, put in the namespace. an exported class is reachable by name from
    /// anywhere at all, so its methods may run whatever became of the class they read —
    /// and the refusal goes back to being the module's
    #[test]
    fn an_exported_helper_holds_the_whole_module() {
        let mut module = module_with(add());
        module.classes.push(appending_class());
        let mut state = appending_class();
        state.name = "Wrapped_step_gen".to_string();
        state.exported = true;
        state.methods.push(reads_a_field_of("Wrapped"));
        module.classes.push(state);
        let c = emit_module(&module);

        assert!(
            c.contains("if (By_app_Wrapped_Type == NULL) return 0;"),
            "an exported reader holds the module: {c}"
        );
    }

    /// and a helper some *still-running* code builds is reachable after all, so it is not
    /// gathered up with the class and its reads count
    #[test]
    fn a_helper_a_module_function_builds_holds_the_whole_module() {
        let mut module = module_with(constructs("Wrapped_step_gen"));
        module.classes.push(appending_class());
        let mut state = appending_class();
        state.name = "Wrapped_step_gen".to_string();
        state.exported = false;
        state.methods.push(reads_a_field_of("Wrapped"));
        module.classes.push(state);
        let c = emit_module(&module);

        assert!(
            c.contains("if (By_app_Wrapped_Type == NULL) return 0;"),
            "a helper the module still builds holds it: {c}"
        );
    }

    /// an instance counts as one reference to its type however deep the chain of
    /// appended storage is. each traverse chains to the one below, so reporting the link
    /// unconditionally would report it once per rung — and a collector told an instance
    /// holds two references where it holds one has been told the type is garbage
    #[test]
    fn the_link_to_the_type_is_reported_by_exactly_one_traverse() {
        let mut module = module_with(add());
        module.classes.push(appending_class());
        let c = emit_module(&module);
        assert!(
            c.contains(
                "    if (!(by_base->tp_flags & Py_TPFLAGS_HEAPTYPE)) Py_VISIT(Py_TYPE(self));"
            ),
            "the rung whose base carries it already does not: {c}"
        );
        assert_eq!(
            c.matches("Py_VISIT(Py_TYPE(self))").count(),
            1,
            "one report per traverse: {c}"
        );
    }

    /// and the same rung drops it. an instance holds one reference to its type; a chain
    /// of appended storage whose every rung drops one loses the type after five
    /// instances and the process goes with it
    #[test]
    fn the_reference_to_the_type_is_dropped_by_exactly_one_deallocator() {
        let mut module = module_with(add());
        module.classes.push(appending_class());
        let c = emit_module(&module);
        assert!(
            c.contains(
                "    if (!(by_base->tp_flags & Py_TPFLAGS_HEAPTYPE)\n\
                 \x20       && (by_type->tp_flags & Py_TPFLAGS_HEAPTYPE)) Py_DECREF(by_type);"
            ),
            "the rung whose base drops it already does not: {c}"
        );
        assert_eq!(c.matches("Py_DECREF(by_type)").count(), 1, "{c}");
        // and the base is read before the deallocation, because after it the object is
        // gone and the type pointer with it
        let read = c.find("by_base = ").expect("the base is read");
        let free = c
            .find("by_base->tp_dealloc(self);")
            .expect("and chained to");
        assert!(read < free, "{c}");
    }

    /// the subclass's own region holds only what it adds: the base keeps its fields in a
    /// region of its own, reached through the type that declared them, and a copy here
    /// would give the pair two of each — the base's methods writing one and the
    /// subclass's the other
    #[test]
    fn an_appended_subclass_declares_only_the_fields_it_adds() {
        let mut module = module_with(add());
        module.classes.push(appending_class());
        module.classes.push(deeper_class());
        let c = emit_module(&module);

        let declaration = c
            .split("struct By_app_Deeper {")
            .nth(1)
            .and_then(|rest| rest.split_once("};"))
            .map(|(body, _)| body.to_string())
            .expect("the subclass declares a struct");
        assert!(declaration.contains("by_f_depth;"), "{declaration}");
        assert!(!declaration.contains("by_f_tag;"), "{declaration}");
        // and a read of the inherited field goes to the base's region rather than this
        // one's, which is what keeps the single copy single
        assert!(
            c.contains(
                "static PyObject *By_app_Deeper_Type_get_tag(PyObject *selfobj, void *closure) {\n\
                 \x20                (void)closure;\n\
                 \x20                By_app_Wrapped *self = ((By_app_Wrapped *)By_TypeData(selfobj, By_app_Wrapped_Type_OBJ));"
            ),
            "{c}"
        );
    }

    /// a generated constructor still fills every field the class declares, so it binds
    /// one pointer per rung: the inherited field is written into the base's region and
    /// the added one into this class's, which is where each is read back from
    #[test]
    fn an_appended_subclass_constructor_writes_each_field_into_its_own_rung() {
        let mut module = module_with(add());
        module.classes.push(appending_class());
        module.classes.push(deeper_class());
        let c = emit_module(&module);

        let init = c
            .split("static int By_app_Deeper_Type_init(")
            .nth(1)
            .and_then(|rest| rest.split_once("\n}\n"))
            .map(|(body, _)| body.to_string())
            .expect("the subclass generates a constructor");
        assert!(
            init.contains(
                "By_app_Deeper *self = ((By_app_Deeper *)By_TypeData(selfobj, By_app_Deeper_Type_OBJ));"
            ),
            "{init}"
        );
        assert!(
            init.contains(
                "By_app_Wrapped *by_up1 = ((By_app_Wrapped *)By_TypeData(selfobj, By_app_Wrapped_Type_OBJ));"
            ),
            "{init}"
        );
        assert!(init.contains("by_up1->by_f_tag = by_v;"), "{init}");
        assert!(init.contains("self->by_f_depth = by_v;"), "{init}");
        // and both are still bound from the call, in the order the class declares them
        assert!(
            init.contains("static const char *const by_names[] = { \"tag\", \"depth\" };"),
            "{init}"
        );
    }

    /// a field list that is not its base's followed by something more is some other
    /// layout entirely, and there is no region to append. such a class keeps the
    /// construction that answers from reality — which refuses a heap base
    #[test]
    fn a_subclass_whose_fields_do_not_extend_its_bases_is_not_appended_over_it() {
        let mut module = module_with(add());
        module.classes.push(appending_class());
        let mut inner = appending_class();
        inner.name = "Deeper".to_string();
        inner.base = Some(ClassBase::InModule("Wrapped".to_string()));
        inner.fields[0].name = "depth".to_string();
        module.classes.push(inner);
        let c = emit_module(&module);

        assert!(!c.contains("By_SpecSubclass"), "{c}");
        assert!(
            c.contains(
                "By_app_Deeper_Type = By_SpecClass(dict, \"Deeper\", &By_app_Deeper_Type_spec);"
            ),
            "{c}"
        );
    }

    /// a base that lays nothing out of its own is still the rung `Deeper`'s deallocator
    /// chains to, and a `class` statement's type there is `subtype_dealloc` and the
    /// recursion that comes with it. so it is built from a spec too, and given the three
    /// slots with nothing in them — a spec asking for none of them would be handed
    /// `subtype_dealloc` just the same
    #[test]
    fn a_base_that_holds_nothing_is_still_built_from_a_spec_to_stand_on() {
        let mut module = module_with(add());
        let mut base = appending_class();
        base.fields.clear();
        module.classes.push(base);
        let mut inner = appending_class();
        inner.name = "Deeper".to_string();
        inner.base = Some(ClassBase::InModule("Wrapped".to_string()));
        inner.fields[0].name = "depth".to_string();
        module.classes.push(inner);
        let c = emit_module(&module);

        assert!(
            c.contains(
                "By_app_Wrapped_Type = By_SpecClass(dict, \"Wrapped\", &By_app_Wrapped_Type_spec);"
            ),
            "the base is built from a spec of its own: {c}"
        );
        assert!(
            c.contains(
                "By_app_Deeper_Type = By_SpecSubclass(dict, \"Deeper\", &By_app_Deeper_Type_spec, \"Wrapped\", By_app_Wrapped_Type_OBJ);"
            ),
            "and the subclass stands on the finished type: {c}"
        );
        assert!(
            c.contains("{Py_tp_dealloc, (void *)By_app_Wrapped_Type_dealloc},"),
            "the base carries the deallocator that breaks the chain: {c}"
        );
        // and it binds no storage, because it declared no region for one
        assert!(
            !c.contains("By_app_Wrapped *by_f"),
            "nothing of the base's own is reached: {c}"
        );
    }

    /// the same base, with nothing appending storage past it. it is then an ordinary
    /// class with no storage of its own: no slots to free an instance with, and the
    /// construction that can fall back to the interpreted definition
    #[test]
    fn a_base_nothing_appends_past_keeps_the_construction_it_had() {
        let mut module = module_with(add());
        let mut base = appending_class();
        base.fields.clear();
        module.classes.push(base);
        let c = emit_module(&module);

        assert!(!c.contains("By_SpecClass"), "{c}");
        assert!(c.contains("By_BuildClass"), "{c}");
        assert!(
            !c.contains("{Py_tp_dealloc, (void *)By_app_Wrapped_Type_dealloc},"),
            "no slot takes the base's freeing over: {c}"
        );
    }

    /// a decorated method rides into the namespace with the constants, and ahead of them
    ///
    /// the order is what `By_ClassConstants::required` counts against, so it is asserted
    /// rather than left to whichever way the two lists happened to be joined. this is a
    /// shape assertion because the behaviour it protects only shows up when the body has
    /// *lost* the name, which the module the differential harness builds never does
    #[test]
    fn a_decorated_method_is_carried_into_the_namespace_ahead_of_the_constants() {
        let mut module = module_with(add());
        let mut class = appending_class();
        class.fields.clear();
        class.constants = vec!["TAG".to_string()];
        class.keywords = vec![by_ir::function::ClassKeyword {
            name: "metaclass".to_string(),
            value: by_ir::function::KeywordValue::Path("ABCMeta".to_string()),
        }];
        let mut method = add();
        method.name = "area".to_string();
        method.decorators = vec![by_ir::function::Decorator::name("abstractmethod")];
        class.methods.push(method);
        module.classes.push(class);
        let c = emit_module(&module);

        assert!(
            c.contains("static const char *const by_constants[] = {\"area\", \"TAG\"};"),
            "the method comes first: {c}"
        );
        assert!(
            c.contains(
                "By_ClassConstants by_carried = {by_body[0], by_constants, 2, 1, &by_twins};"
            ),
            "one of the two has to be found: {c}"
        );
        // and the class is still built by calling the metaclass, which is the whole point
        // of putting the value where the metaclass can read it
        assert!(
            c.contains(", by_kwds, By_app_Wrapped_Type_methods, NULL, 1, &by_carried);"),
            "the metaclass is what builds it, and it is handed the carry: {c}"
        );
    }

    fn appending_class() -> ClassIr {
        ClassIr {
            name: "Wrapped".to_string(),
            immutable: false,
            exported: true,
            base: Some(ClassBase::External(vec!["Exception".to_string()])),
            inherited_init: false,
            generic: false,
            declares_slots: false,
            constants: Vec::new(),
            properties: Vec::new(),
            slot_aliases: Vec::new(),
            fields: vec![by_ir::function::FieldDecl {
                name: "tag".to_string(),
                ty: RType::OBJECT,
                default: None,
                optional: false,
                defaulted_by: None,
            }],
            decorators: Vec::new(),
            methods: Vec::new(),
            resume: None,
            keywords: Vec::new(),
        }
    }

    /// a class appending storage over [`appending_class`], laid out the way the frontend
    /// lays a subclass out: the declared fields begin with the base's, and what it adds
    /// of its own is the run past them
    fn deeper_class() -> ClassIr {
        let mut inner = appending_class();
        inner.name = "Deeper".to_string();
        inner.base = Some(ClassBase::InModule("Wrapped".to_string()));
        inner.fields.push(by_ir::function::FieldDecl {
            name: "depth".to_string(),
            ty: RType::OBJECT,
            default: None,
            optional: false,
            defaulted_by: None,
        });
        inner
    }

    #[test]
    fn an_infallible_function_emits_no_error_path_at_all() {
        // this is error-path elision: the `raises Never` contract, in the C
        let mut function = add();
        function.convention = CallConvention::NativeInfallible;
        // the add itself is still fallible, so drop it for this case
        function.blocks[0].ops.clear();
        function.blocks[0].terminator = Terminator::Return(Value::Register(RegisterId(0)));
        let c = emit_module(&module_with(function));
        assert!(!c.contains("by_error:"));
    }

    #[test]
    fn a_call_to_an_infallible_callee_emits_no_check() {
        let mut callee = FunctionBuilder::new("pure", RType::INT);
        callee.convention(CallConvention::NativeInfallible);
        callee.terminate(Terminator::Return(Value::Int(1)));

        let mut caller = FunctionBuilder::new("use", RType::INT);
        let out = caller.temp(RType::INT);
        caller.push(Op::CallNative {
            owner: None,
            dest: Some(out),
            callee: "pure".to_string(),
            args: Vec::new(),
        });
        caller.terminate(Terminator::Return(Value::Register(out)));

        let module = ModuleIr {
            name: by_ir::ModuleName::new("app"),
            functions: vec![callee.finish(), caller.finish()],
            declined: Vec::new(),
            classes: Vec::new(),
            gradual: Vec::new(),
            promoted: Vec::new(),
            lines: None,
            fallback_source: None,
            fallback_code: None,
            shims: None,
        };
        let c = emit_module(&module);
        // the forward declaration precedes the body, so take the last split
        let use_body = c
            .rsplit("static ByTagged by_app_use")
            .next()
            .expect("the caller is emitted");
        assert!(use_body.contains("by_app_pure()"), "{use_body}");
        assert!(!use_body.contains("if (r0 == BY_INT_ERROR) goto by_error;"));
    }

    #[test]
    fn every_exit_releases_what_the_frame_owns_and_nothing_else() {
        let c = emit_module(&module_with(add()));
        // the frame does not release the parameters, which the caller still owns.
        // releasing them here *and* in the wrapper was a double-decref that only
        // survived because small ints are unrefcounted
        let body = c
            .split("static ByTagged by_app_add(ByTagged r0, ByTagged r1) {")
            .nth(1)
            .and_then(|rest| rest.split("static PyObject").next())
            .expect("the body is emitted");
        assert!(!body.contains("By_DecRefTagged(r0);"), "{body}");
        assert!(!body.contains("By_DecRefTagged(r1);"), "{body}");
    }

    #[test]
    fn a_returned_register_hands_its_own_reference_to_the_caller() {
        let c = emit_module(&module_with(add()));
        let returned = c
            .split("{ ByTagged by_ret =")
            .nth(1)
            .and_then(|rest| rest.split("return by_ret;").next())
            .expect("the return is emitted");
        // the sum is the frame's own, and the caller is given the reference the
        // frame already holds rather than a second one. the *error* path still
        // releases it, because nothing is being handed out there
        assert!(!returned.contains("By_IncRefTagged"), "{returned}");
        assert!(!returned.contains("By_DecRefTagged(r2);"), "{returned}");
        assert!(c.contains("by_error: ;\n    By_DecRefTagged(r2);"), "{c}");
    }

    #[test]
    fn a_returned_parameter_is_still_retained() {
        let mut builder = FunctionBuilder::new("first", RType::STR);
        let a = builder.param("a", RType::STR);
        builder.terminate(Terminator::Return(Value::Register(a)));
        let c = emit_module(&module_with(builder.finish()));
        let body = c
            .split("static PyObject * by_app_first(PyObject * r0) {")
            .nth(1)
            .and_then(|rest| rest.split("static PyObject *byw").next())
            .expect("the body is emitted");
        // the frame never owned it, so there is no reference of its own to hand on
        assert!(body.contains("Py_XINCREF(by_ret);"), "{body}");
    }

    #[test]
    fn a_returned_register_the_analysis_rules_out_is_still_retained() {
        let mut builder = FunctionBuilder::new("f", RType::STR);
        let s = builder.temp(RType::STR);
        builder.assign(s, Value::Str("x".to_string()));
        builder.terminate(Terminator::Return(Value::Register(s)));
        let mut module = module_with(builder.finish());
        // claim nothing is owned at the exit: there is no release to cancel against
        module.functions[0].blocks[0].owned_at_exit = Some(Vec::new());
        let c = emit_module(&module);
        assert!(c.contains("Py_XINCREF(by_ret);"), "{c}");
    }

    #[test]
    fn a_reassigned_parameter_is_retained_on_entry_and_released_like_a_local() {
        // the first write would otherwise release a reference the caller holds
        let mut builder = FunctionBuilder::new("halve", RType::INT);
        let n = builder.param("n", RType::INT);
        builder.push(Op::IntBinary {
            dest: n,
            op: BinOp::FloorDiv,
            lhs: Value::Register(n),
            rhs: Value::Int(2),
        });
        builder.terminate(Terminator::Return(Value::Register(n)));
        let c = emit_module(&module_with(builder.finish()));
        let body = c
            .split("static ByTagged by_app_halve(ByTagged r0) {")
            .nth(1)
            .and_then(|rest| rest.split("static PyObject").next())
            .expect("the body is emitted");
        assert!(
            body.trim_start().starts_with("By_IncRefTagged(r0);"),
            "the retain comes first: {body}"
        );
        assert!(body.contains("By_DecRefTagged(r0);"), "{body}");
    }

    #[test]
    fn a_write_computes_before_it_releases() {
        let c = emit_module(&module_with(add()));
        assert!(
            c.contains(
                "{ ByTagged by_t = By_IntAdd(r0, r1);\n\
                 \x20     if (BY_UNLIKELY(by_t == BY_INT_ERROR)) goto by_error;\n\
                 \x20     By_DecRefTagged(r2); r2 = by_t; }"
            ),
            "{c}"
        );
    }

    #[test]
    fn writing_into_a_register_that_is_also_an_operand_is_safe() {
        // `acc = acc + n` must not release `acc` before the addition reads it
        let mut builder = FunctionBuilder::new("accumulate", RType::INT);
        let n = builder.param("n", RType::INT);
        let acc = builder.local("acc", RType::INT);
        builder.assign(acc, Value::Int(0));
        builder.push(Op::IntBinary {
            dest: acc,
            op: BinOp::Add,
            lhs: Value::Register(acc),
            rhs: Value::Register(n),
        });
        builder.terminate(Terminator::Return(Value::Register(acc)));
        let c = emit_module(&module_with(builder.finish()));
        let compute = c
            .find("By_IntAdd(r1, r0)")
            .expect("the addition is emitted");
        let release = c[compute..]
            .find("By_DecRefTagged(r1);")
            .expect("the old value is released");
        assert!(
            release > 0,
            "the release must follow the read, not precede it"
        );
    }

    #[test]
    fn copying_a_register_retains_before_it_releases() {
        // `a = a` must not release the value it is about to store
        let mut builder = FunctionBuilder::new("copy", RType::INT);
        let a = builder.param("a", RType::INT);
        let b = builder.local("b", RType::INT);
        builder.assign(b, Value::Register(a));
        builder.terminate(Terminator::Return(Value::Register(b)));
        let c = emit_module(&module_with(builder.finish()));
        let retain = c
            .find("By_IncRefTagged(r1_t);")
            .expect("retains the source");
        let release = c
            .find("By_DecRefTagged(r1);\n      r1 = r1_t")
            .expect("releases the old");
        assert!(retain < release, "the retain must come first");
    }

    #[test]
    fn a_float_error_is_confirmed_against_the_exception_state() {
        // BY_FLOAT_ERROR is a legal double, so the sentinel alone proves nothing
        let mut builder = FunctionBuilder::new("div", RType::FLOAT);
        let a = builder.param("a", RType::FLOAT);
        let b = builder.param("b", RType::FLOAT);
        let out = builder.temp(RType::FLOAT);
        builder.push(Op::FloatBinary {
            dest: out,
            op: BinOp::TrueDiv,
            lhs: Value::Register(a),
            rhs: Value::Register(b),
        });
        builder.terminate(Terminator::Return(Value::Register(out)));
        let c = emit_module(&module_with(builder.finish()));
        assert!(c.contains(
            "if (BY_UNLIKELY(by_t == BY_FLOAT_ERROR && PyErr_Occurred())) goto by_error;"
        ));
    }

    #[test]
    fn a_float_add_needs_no_error_check() {
        let mut builder = FunctionBuilder::new("sum", RType::FLOAT);
        let a = builder.param("a", RType::FLOAT);
        let b = builder.param("b", RType::FLOAT);
        let out = builder.temp(RType::FLOAT);
        builder.push(Op::FloatBinary {
            dest: out,
            op: BinOp::Add,
            lhs: Value::Register(a),
            rhs: Value::Register(b),
        });
        builder.terminate(Terminator::Return(Value::Register(out)));
        let c = emit_module(&module_with(builder.finish()));
        assert!(c.contains("r2 = (r0 + r1);"));
        assert!(!c.contains("if (r2 == BY_FLOAT_ERROR"));
    }

    #[test]
    fn the_wrapper_checks_arity_and_unboxes_each_argument() {
        let c = emit_module(&module_with(add()));
        // positionals and keywords bind in one pass, and a missing one is reported by
        // name rather than by count
        assert!(
            c.contains("By_BindArgs(args, nargs, kwnames, by_names, 2"),
            "{c}"
        );
        assert!(
            c.contains("static const unsigned char by_required[] = { 1, 1 };"),
            "{c}"
        );
        assert!(c.contains("ByTagged a0 = BY_INT_ERROR;"), "{c}");
        assert!(c.contains("ByTagged a1 = BY_INT_ERROR;"), "{c}");
        assert!(c.contains("a0 = By_UnboxInt(by_bound[0]);"), "{c}");
        assert!(c.contains("a1 = By_UnboxInt(by_bound[1]);"), "{c}");
        assert!(c.contains("return By_BoxInt(by_result);"));
    }

    #[test]
    fn a_non_exported_function_gets_no_wrapper_or_table_entry() {
        let mut function = add();
        function.exported = false;
        let c = emit_module(&module_with(function));
        assert!(!c.contains("byw_app_add"));
        assert!(c.contains("static ByTagged by_app_add"));
    }

    #[test]
    fn the_module_init_matches_the_import_name() {
        let module = ModuleIr {
            name: by_ir::ModuleName::new("pkg.app"),
            functions: vec![add()],
            classes: Vec::new(),
            gradual: Vec::new(),
            promoted: Vec::new(),
            declined: vec![Declined {
                range: None,
                name: "gen".to_string(),
                reason: "unsupported".to_string(),
            }],
            lines: None,
            fallback_source: None,
            fallback_code: None,
            shims: None,
        };
        let c = emit_module(&module);
        assert!(c.contains("PyMODINIT_FUNC PyInit_app(void)"));
        assert!(c.contains("\"app\""));
        assert!(c.contains(
            "{\"add\", (PyCFunction)(void(*)(void))byw_pkg_app_add, METH_FASTCALL | METH_KEYWORDS, NULL},"
        ));
    }

    /// the version tag an artefact carries is in its file name, and every 3.x also
    /// accepts a bare `.so` — so a renamed artefact is offered to an interpreter the
    /// build never saw. the emitted init refuses one before it reads `by_module`,
    /// because that struct's layout is the build's own
    #[test]
    fn the_module_init_refuses_a_mismatched_interpreter_first() {
        let c = emit_module(&module_with(add()));
        let init = c
            .split_once("PyMODINIT_FUNC PyInit_app(void)")
            .expect("the module init is emitted")
            .1;
        let guard = init
            .find("if (!By_InterpreterMatches()) return NULL;")
            .expect("the init guards on the running interpreter");
        let hand_over = init
            .find("PyModuleDef_Init(&by_module)")
            .expect("the init hands the definition over");
        assert!(guard < hand_over);
    }

    /// cpython reads a class's `__module__` off the front of its `tp_name` and its
    /// `__name__` off the back, so a class in a package needs the *whole* dotted
    /// name there. emitting only the last component made a class in `tkinter/m.py`
    /// answer `__module__ == "m"`, which `sys.modules` has nothing under
    #[test]
    fn a_type_in_a_package_names_the_dotted_module_it_came_from() {
        let mut module = module_with(add());
        module.name = by_ir::ModuleName::new("pkg.app");
        module.classes.push(ClassIr {
            name: "Point".to_string(),
            immutable: false,
            exported: true,
            base: None,
            inherited_init: false,
            generic: false,
            declares_slots: false,
            constants: Vec::new(),
            properties: Vec::new(),
            slot_aliases: Vec::new(),
            fields: vec![by_ir::function::FieldDecl {
                name: "x".to_string(),
                ty: RType::INT,
                default: None,
                optional: false,
                defaulted_by: None,
            }],
            decorators: Vec::new(),
            methods: Vec::new(),
            resume: None,
            keywords: Vec::new(),
        });
        let c = emit_module(&module);
        assert!(c.contains("\"pkg.app.Point\""), "{c}");
        assert!(!c.contains("\"app.Point\""), "{c}");
    }

    #[test]
    fn a_docstring_that_python_would_clean_is_emitted_both_ways() {
        // the emitted C is a function of the source alone — the same bytes whichever
        // interpreter builds them — so a docstring the versions disagree about has to
        // carry both spellings and let the preprocessor choose
        let mut function = add();
        function.doc = Some("summary\n    body\n".to_string());
        let c = emit_module(&module_with(function));
        assert!(
            c.contains("BY_DOC(\"summary\\n    body\\n\",\n\"summary\\nbody\\n\")"),
            "{c}"
        );
    }

    #[test]
    fn a_docstring_python_leaves_alone_is_emitted_once() {
        // no version disagrees about a single line with no indentation to take out, and
        // spelling it twice would double what the artefact carries for nothing
        let mut function = add();
        function.doc = Some("summary".to_string());
        let c = emit_module(&module_with(function));
        assert!(
            c.contains("METH_FASTCALL | METH_KEYWORDS, \"summary\""),
            "{c}"
        );
        assert!(!c.contains("BY_DOC"), "{c}");
    }

    #[test]
    fn a_definition_with_no_docstring_leaves_the_entry_null() {
        let c = emit_module(&module_with(add()));
        assert!(c.contains("METH_FASTCALL | METH_KEYWORDS, NULL}"), "{c}");
    }

    #[test]
    fn the_interpreted_definitions_run_before_the_natives_are_installed() {
        let mut module = module_with(add());
        module.fallback_source = Some("def add(a, b):\n    return 0\n".to_string());
        let c = emit_module(&module);
        // the natives must be installed from the exec slot, not from m_methods,
        // or the interpreted definitions would overwrite them
        assert!(c.contains("By_ExecModuleBody(&by_fallback, dict)"), "{c}");
        assert!(
            c.contains("PyModule_AddFunctions(module, by_methods)"),
            "{c}"
        );
        assert!(c.contains("{Py_mod_exec, (void *)by_exec}"), "{c}");
        assert!(
            c.contains("PyModuleDef_HEAD_INIT, \"app\", NULL, 0, NULL, by_slots"),
            "m_methods must stay NULL: {c}"
        );
    }

    #[test]
    fn a_module_declares_that_it_does_not_need_the_gil() {
        // on a free-threaded 3.13+ build, importing an extension without this
        // slot re-enables the GIL for the whole process — the single most
        // expensive thing a compiled module could silently do
        let c = emit_module(&module_with(add()));
        assert!(c.contains("{Py_mod_gil, Py_MOD_GIL_NOT_USED}"), "{c}");
        // the slot does not exist before 3.13, so it has to be guarded
        assert!(c.contains("#if PY_VERSION_HEX >= 0x030D0000"), "{c}");
        assert!(c.contains("#endif"), "{c}");
    }

    #[test]
    fn a_module_with_nothing_to_fall_back_to_skips_the_exec() {
        let c = emit_module(&module_with(add()));
        assert!(
            c.contains("static const char by_fallback_source[] =\n\"\";"),
            "{c}"
        );
        assert!(c.contains("if (by_fallback_source[0] != '\\0')"), "{c}");
    }

    #[test]
    fn a_long_fallback_source_is_split_into_adjacent_literals() {
        // a single string literal has an implementation-defined maximum length;
        // adjacent literals concatenate without one
        let mut module = module_with(add());
        module.fallback_source = Some("x = 1\n".repeat(500));
        let c = emit_module(&module);
        let declaration = c
            .split("static PyMethodDef")
            .next()
            .expect("the declaration precedes the method table");
        assert!(
            declaration.matches("\"\n\"").count() > 1,
            "expected several concatenated literals"
        );
    }

    /// read a run of adjacent C string literals back into the bytes they stand for
    ///
    /// the emitted escaping is only worth having if it is exact, and "the module still
    /// imports" cannot say that it is: an artefact whose code object will not read
    /// falls back to its source and behaves identically, just slower. so the literal is
    /// decoded here and compared byte for byte
    fn decode_c_literals(text: &str) -> Vec<u8> {
        let bytes = text.as_bytes();
        let mut out = Vec::new();
        let mut at = 0;
        let mut inside = false;
        while at < bytes.len() {
            let byte = bytes[at];
            at += 1;
            if !inside {
                inside = byte == b'"';
                continue;
            }
            match byte {
                b'"' => inside = false,
                b'\\' => {
                    let escape = bytes[at];
                    at += 1;
                    match escape {
                        b'n' => out.push(b'\n'),
                        b'r' => out.push(b'\r'),
                        b't' => out.push(b'\t'),
                        b'"' => out.push(b'"'),
                        b'\\' => out.push(b'\\'),
                        // octal, always exactly three digits so a digit that follows
                        // stays a digit
                        _ => {
                            let digits = std::str::from_utf8(&bytes[at - 1..at + 2])
                                .expect("an octal escape is ascii");
                            out.push(u8::from_str_radix(digits, 8).expect("three octal digits"));
                            at += 2;
                        }
                    }
                }
                other => out.push(other),
            }
        }
        out
    }

    #[test]
    fn the_compiled_twin_is_emitted_byte_for_byte_beside_its_source() {
        // marshalled bytes are not text: every value from 0 to 255 turns up, NULs and
        // quotes and backslashes among them, and a digit landing right after an escaped
        // byte is what a two-digit octal escape would swallow
        let mut module = module_with(add());
        module.fallback_source = Some("x = 1\n".to_string());
        let marshalled: Vec<u8> = (0..=255u8).chain([b'\\', b'1', 1, b'7', b'"', 0]).collect();
        module.fallback_code = Some(by_ir::function::FallbackCode {
            marshalled: marshalled.clone().into(),
            magic: 168_627_699,
            optimize: 2,
        });
        let c = emit_module(&module);
        let declaration = c
            .split("static const By_Fallback")
            .next()
            .expect("the literal precedes the struct");
        let literal = declaration
            .split("static const char by_fallback_code[] =")
            .nth(1)
            .expect("the code object is emitted");
        assert_eq!(decode_c_literals(literal), marshalled);
        assert!(
            c.contains(&format!(
                "by_fallback_source, by_fallback_code, {}, 168627699L, 2}}",
                marshalled.len()
            )),
            "{c}"
        );
    }

    #[test]
    fn a_module_with_no_compiled_twin_stands_a_null_in_its_place() {
        // `--emit-c-only` with no interpreter to ask is the case: the artefact still
        // carries its source, and the runtime reads a NULL as "compile that instead"
        let mut module = module_with(add());
        module.fallback_source = Some("x = 1\n".to_string());
        let c = emit_module(&module);
        assert!(!c.contains("by_fallback_code[]"), "{c}");
        assert!(
            c.contains(
                "static const By_Fallback by_fallback = {by_fallback_source, NULL, 0, 0L, 0};"
            ),
            "{c}"
        );
    }

    #[test]
    fn a_long_compiled_twin_is_split_into_adjacent_literals() {
        // the same implementation-defined maximum the source literal has to dodge
        let mut module = module_with(add());
        module.fallback_source = Some("x = 1\n".to_string());
        module.fallback_code = Some(by_ir::function::FallbackCode {
            marshalled: vec![b'a'; 5000].into(),
            magic: 1,
            optimize: 0,
        });
        let c = emit_module(&module);
        let literal = c
            .split("static const char by_fallback_code[] =")
            .nth(1)
            .expect("the code object is emitted")
            .split("static const By_Fallback")
            .next()
            .expect("the struct follows it");
        assert_eq!(literal.matches("\"\n\"").count(), 4, "{literal}");
        assert_eq!(decode_c_literals(literal), vec![b'a'; 5000]);
    }

    #[test]
    fn a_string_literal_is_interned_once_rather_than_built_per_use() {
        // building it per use created a reference nobody released — a leak
        // `gc.get_objects` cannot see, because `str` is not GC-tracked
        let mut builder = FunctionBuilder::new("pick", RType::STR);
        let out = builder.temp(RType::STR);
        builder.assign(out, Value::Str("hello".to_string()));
        builder.terminate(Terminator::Return(Value::Register(out)));
        let c = emit_module(&module_with(builder.finish()));
        assert!(c.contains("static PyObject *by_str0 = NULL;"), "{c}");
        assert!(c.contains("by_str0 = By_InternedStr(\"hello\", 5);"), "{c}");
        // and no per-use construction remains
        assert!(!c.contains("PyUnicode_FromString(\"hello\")"), "{c}");
    }

    #[test]
    fn a_literal_carries_its_length_rather_than_ending_at_a_nul() {
        // built from the C string alone, `"a\0b"` was every use's `"a"` — the
        // constructor read no further than the first NUL, silently
        let mut builder = FunctionBuilder::new("pick", RType::STR);
        let out = builder.temp(RType::STR);
        builder.assign(out, Value::Str("a\0b\u{e9}".to_string()));
        builder.terminate(Terminator::Return(Value::Register(out)));
        let c = emit_module(&module_with(builder.finish()));
        // the length is of the utf-8 the escape writes, not of the code points
        assert!(
            c.contains("by_str0 = By_InternedStr(\"a\\000b\\303\\251\", 5);"),
            "{c}"
        );
    }

    #[test]
    fn a_literal_naming_itself_cannot_close_the_comment_it_sits_in() {
        let mut builder = FunctionBuilder::new("pick", RType::STR);
        let out = builder.temp(RType::STR);
        builder.assign(out, Value::Str("a */ b".to_string()));
        builder.terminate(Terminator::Return(Value::Register(out)));
        let c = emit_module(&module_with(builder.finish()));
        assert!(
            c.contains("static PyObject *by_str0 = NULL; // \"a */ b\"\n"),
            "{c}"
        );
    }

    #[test]
    fn the_same_literal_twice_shares_one_static() {
        let mut builder = FunctionBuilder::new("pick", RType::STR);
        let a = builder.temp(RType::STR);
        let b = builder.temp(RType::STR);
        let joined = builder.temp(RType::STR);
        builder.assign(a, Value::Str("x".to_string()));
        builder.assign(b, Value::Str("x".to_string()));
        builder.push(Op::StrConcat {
            dest: joined,
            lhs: Value::Register(a),
            rhs: Value::Register(b),
            consumes_lhs: false,
        });
        builder.terminate(Terminator::Return(Value::Register(joined)));
        let c = emit_module(&module_with(builder.finish()));
        assert_eq!(c.matches("static PyObject *by_str").count(), 1, "{c}");
    }

    #[test]
    fn a_consuming_concatenation_empties_the_register_it_takes_over() {
        // the register must be cleared whether the call succeeds or not: the
        // reference is gone either way, and an exit path still naming it would
        // release a second time
        let mut builder = FunctionBuilder::new("grow", RType::STR);
        let seed = builder.param("s", RType::STR);
        let out = builder.temp(RType::STR);
        let grown = builder.temp(RType::STR);
        builder.assign(out, Value::Str("x".to_string()));
        builder.push(Op::StrConcat {
            dest: grown,
            lhs: Value::Register(out),
            rhs: Value::Register(seed),
            consumes_lhs: true,
        });
        builder.terminate(Terminator::Return(Value::Register(grown)));
        let c = emit_module(&module_with(builder.finish()));
        let register = local(out);
        assert!(
            c.contains(&format!(
                "PyObject * by_lhs = {register}; {register} = NULL;"
            )),
            "{c}"
        );
        assert!(c.contains("By_StrAppend(by_lhs, by_rhs)"), "{c}");
        assert!(!c.contains("By_StrConcat("), "{c}");
    }

    #[test]
    fn a_bytes_literal_is_built_once_and_carries_its_own_length() {
        // a `bytes` constant can hold a NUL, so the length has to be passed rather
        // than taken from the C string
        let mut builder = FunctionBuilder::new("pick", RType::OBJECT);
        let out = builder.temp(RType::OBJECT);
        builder.assign(out, Value::Bytes(Box::from(&b"a\0\xffb"[..])));
        builder.terminate(Terminator::Return(Value::Register(out)));
        let c = emit_module(&module_with(builder.finish()));
        assert!(c.contains("static PyObject *by_bytes0 = NULL;"), "{c}");
        assert!(
            c.contains("by_bytes0 = PyBytes_FromStringAndSize(\"a\\000\\377b\", 4);"),
            "{c}"
        );
    }

    #[test]
    fn a_guard_clause_still_releases_what_was_written_before_it() {
        // the refcount pass narrows the release set; getting the direction wrong
        // (liveness instead of may-have-been-written) leaks here
        let mut builder = FunctionBuilder::new("f", RType::INT);
        let cond = builder.param("c", RType::BIT);
        let scratch = builder.temp(RType::STR);
        let early = builder.new_block();
        let late = builder.new_block();
        builder.assign(scratch, Value::Str("x".to_string()));
        builder.terminate(Terminator::Branch {
            cond: Value::Register(cond),
            then_block: early,
            else_block: late,
        });
        builder.switch_to(early);
        builder.terminate(Terminator::Return(Value::Int(1)));
        builder.switch_to(late);
        builder.terminate(Terminator::Return(Value::Int(0)));

        let mut module = module_with(builder.finish());
        by_ir::verify::verify(&module.functions[0]).expect("the fixture verifies");
        // with no analysis result, every exit releases it — plus the write itself
        let conservative = emit_module(&module);
        assert!(
            conservative.matches("Py_XDECREF(r1);").count() >= 3,
            "{conservative}"
        );

        // and with the analysis, the guard still releases it
        module.functions[0].blocks[1].owned_at_exit = Some(vec![RegisterId(1)]);
        let analysed = emit_module(&module);
        assert!(analysed.contains("Py_XDECREF(r1);"), "{analysed}");
    }

    #[test]
    fn a_register_the_analysis_rules_out_is_not_released() {
        let mut builder = FunctionBuilder::new("f", RType::INT);
        let unused = builder.temp(RType::STR);
        builder.assign(unused, Value::Str("x".to_string()));
        builder.terminate(Terminator::Return(Value::Int(0)));
        let mut module = module_with(builder.finish());
        // claim nothing is owned at the exit
        module.functions[0].blocks[0].owned_at_exit = Some(Vec::new());
        let c = emit_module(&module);
        let return_block = c
            .split("{ ByTagged by_ret =")
            .nth(1)
            .and_then(|rest| rest.split("return by_ret;").next())
            .expect("the return is emitted");
        assert!(
            !return_block.contains("Py_XDECREF(r0);"),
            "the return path should skip it: {return_block}"
        );
    }

    #[test]
    fn a_string_literal_is_escaped_for_c() {
        let mut builder = FunctionBuilder::new("greet", RType::STR);
        let s = builder.temp(RType::STR);
        builder.assign(s, Value::Str("a\"b\n\u{e9}".to_string()));
        builder.terminate(Terminator::Return(Value::Register(s)));
        let c = emit_module(&module_with(builder.finish()));
        assert!(c.contains(r#""a\"b\n\303\251""#));
    }

    #[test]
    fn a_fixed_tuple_gets_one_struct_per_layout() {
        let tuple_ty = RType::Tuple(Box::new([RType::INT, RType::FLOAT]));
        let mut builder = FunctionBuilder::new("pair", tuple_ty.clone());
        let a = builder.param("a", RType::INT);
        let out = builder.temp(tuple_ty);
        builder.push(Op::TupleBuild {
            dest: out,
            items: vec![Value::Register(a), Value::Float(1.5)],
        });
        builder.terminate(Terminator::Return(Value::Register(out)));
        let c = emit_module(&module_with(builder.finish()));
        assert!(c.contains("typedef struct { ByTagged f0; double f1; } ByTuple_int_float;"));
        assert_eq!(c.matches("ByTuple_int_float;").count(), 1);
    }

    #[test]
    fn a_tuple_slot_naming_an_instance_has_the_name_ahead_of_it() {
        // neither group can be defined first — a tuple slot may be an instance and a
        // class field may be a tuple — so the instance structs are *named* ahead of
        // both, and a slot holding one is a pointer to a name that is already there
        let point = RType::Instance {
            class: "Point".to_string(),
            exact: false,
        };
        let tuple_ty = RType::Tuple(Box::new([point.clone(), RType::INT]));
        let mut builder = FunctionBuilder::new("placed", tuple_ty.clone());
        let n = builder.param("n", RType::INT);
        let made = builder.param("made", point);
        let out = builder.temp(tuple_ty);
        builder.push(Op::TupleBuild {
            dest: out,
            items: vec![Value::Register(made), Value::Register(n)],
        });
        builder.terminate(Terminator::Return(Value::Register(out)));
        let mut module = module_with(builder.finish());
        let mut class = appending_class();
        class.name = "Point".to_string();
        class.base = None;
        module.classes.push(class);
        let c = emit_module(&module);

        let named = c
            .find("typedef struct By_app_Point By_app_Point;")
            .expect("the instance struct is named");
        let slot = c
            .find("typedef struct { By_app_Point * f0; ByTagged f1; } ByTuple_iPoint_int;")
            .expect("the slot is a pointer to it");
        assert!(named < slot, "{c}");
        let defined = c
            .find("struct By_app_Point {\n")
            .expect("and defined after both");
        assert!(slot < defined, "{c}");
    }

    #[test]
    fn two_instance_slots_that_differ_only_in_exactness_share_one_struct() {
        // an exact register type licenses a direct method call and an inexact one does
        // not, which is a real difference to the lowering. it is no difference at all
        // to the C, where both are the same pointer to the same struct — so the name
        // the two agree on has to be defined once, however many register types reach it
        let instance = |exact| RType::Instance {
            class: "Point".to_string(),
            exact,
        };
        let pair = |exact| RType::Tuple(Box::new([instance(exact), RType::INT]));
        let returning_a_pair = |name: &str, exact| {
            let mut builder = FunctionBuilder::new(name, pair(exact));
            let n = builder.param("n", RType::INT);
            let point = builder.param("point", instance(exact));
            let out = builder.temp(pair(exact));
            builder.push(Op::TupleBuild {
                dest: out,
                items: vec![Value::Register(point), Value::Register(n)],
            });
            builder.terminate(Terminator::Return(Value::Register(out)));
            builder.finish()
        };
        let mut module = module_with(returning_a_pair("made", true));
        module.functions.push(returning_a_pair("passed", false));
        let mut class = appending_class();
        class.name = "Point".to_string();
        class.base = None;
        module.classes.push(class);
        for function in &module.functions {
            assert_eq!(verify(function), Ok(()), "{}", function.name);
        }
        let c = emit_module(&module);

        assert_eq!(c.matches("} ByTuple_iPoint_int;").count(), 1, "{c}");
    }

    #[test]
    fn emitted_functions_verify_first() {
        // codegen is only correct for verified input, so the fixtures must verify
        assert_eq!(verify(&add()), Ok(()));
    }

    #[test]
    fn a_borrowed_field_read_emits_no_refcount_traffic() {
        let mut builder = FunctionBuilder::new("label_of", RType::STR);
        let outer = builder.param(
            "n",
            RType::Instance {
                class: "Nest".to_string(),
                exact: false,
            },
        );
        let inner = builder.temp(RType::Instance {
            class: "Holder".to_string(),
            exact: false,
        });
        let label = builder.temp(RType::STR);
        builder.push(Op::GetField {
            dest: inner,
            receiver: Value::Register(outer),
            class: "Nest".to_string(),
            field: "inner".to_string(),
        });
        builder.push(Op::GetField {
            dest: label,
            receiver: Value::Register(inner),
            class: "Holder".to_string(),
            field: "label".to_string(),
        });
        builder.terminate(Terminator::Return(Value::Register(label)));
        let mut function = builder.finish();
        function.registers[inner.index()].borrowed = true;

        let text = emit_function(&ModuleIr::new("app"), &function);
        assert!(text.contains("r1 = r0->by_f_inner;"), "{text}");
        // the intermediate is neither retained nor released
        assert!(!text.contains("Py_XINCREF(r0->by_f_inner)"), "{text}");
        assert!(!text.contains("Py_XDECREF(r1)"), "{text}");
        // the value that leaves still is
        assert!(text.contains("Py_XINCREF(r1->by_f_label)"), "{text}");
    }

    #[test]
    fn a_borrowed_copy_is_a_plain_store() {
        // `len(line)` over a `str` parameter: the operand position widens, and the
        // widening is a `PyObject *` moving into another `PyObject *`
        let mut builder = FunctionBuilder::new("size", RType::INT);
        let line = builder.param("line", RType::STR);
        let widened = builder.temp(RType::OBJECT);
        let length = builder.temp(RType::INT);
        builder.assign(widened, Value::Register(line));
        builder.push(Op::Len {
            dest: length,
            src: Value::Register(widened),
        });
        builder.terminate(Terminator::Return(Value::Register(length)));
        let mut function = builder.finish();
        function.registers[widened.index()].borrowed = true;

        let text = emit_function(&ModuleIr::new("app"), &function);
        assert!(text.contains("    r1 = r0;\n"), "{text}");
        assert!(!text.contains("Py_XINCREF"), "{text}");
        // and the frame does not give back what it never took, on either way out
        assert!(!text.contains("Py_XDECREF(r1)"), "{text}");
    }

    #[test]
    fn a_borrowed_tuple_element_is_a_plain_store() {
        // `head, tail = split(s)`: the struct the call answered with owns both
        // elements, so reading one takes nothing and gives nothing back
        let mut builder = FunctionBuilder::new("f", RType::INT);
        let text_in = builder.param("s", RType::STR);
        let pair = builder.temp(RType::Tuple(Box::new([RType::STR, RType::STR])));
        let head = builder.local("head", RType::STR);
        let length = builder.temp(RType::INT);
        builder.push(Op::CallNative {
            owner: None,
            dest: Some(pair),
            callee: "split".to_string(),
            args: vec![Value::Register(text_in)],
        });
        builder.push(Op::TupleGet {
            dest: head,
            src: Value::Register(pair),
            index: 0,
        });
        builder.push(Op::Len {
            dest: length,
            src: Value::Register(head),
        });
        builder.terminate(Terminator::Return(Value::Register(length)));
        let mut function = builder.finish();
        function.registers[head.index()].borrowed = true;

        let text = emit_function(&ModuleIr::new("app"), &function);
        assert!(text.contains("    r2 = r1.f0;\n"), "{text}");
        assert!(!text.contains("Py_XINCREF(r1.f0)"), "{text}");
        assert!(!text.contains("Py_XDECREF(r2)"), "{text}");
        // the tuple itself still owns what it holds, on either way out
        assert!(text.contains("Py_XDECREF(r1.f0)"), "{text}");
    }

    #[test]
    fn a_line_directive_points_at_the_by_source() {
        let mut builder = FunctionBuilder::new("f", RType::INT);
        let a = builder.param("a", RType::INT);
        // as if `def f` were on line 2 and its body on line 3
        builder.at((3, 20));
        builder.block_at((12, 20));
        builder.terminate(Terminator::Return(Value::Register(a)));

        let mut module = ModuleIr::new("app");
        module.functions.push(builder.finish());
        module.lines = Some(by_ir::function::LineTable::new(
            "src/app.by",
            "\ndef f(a):\n    return a\n",
        ));
        let text = emit_module(&module);
        assert!(text.contains("#line 2 \"src/app.by\""), "{text}");
        assert!(text.contains("#line 3 \"src/app.by\""), "{text}");
    }

    #[test]
    fn no_line_table_means_no_directives() {
        let mut builder = FunctionBuilder::new("f", RType::INT);
        let a = builder.param("a", RType::INT);
        builder.at((3, 20));
        builder.terminate(Terminator::Return(Value::Register(a)));
        let mut module = ModuleIr::new("app");
        module.functions.push(builder.finish());
        assert!(!emit_module(&module).contains("#line"));
    }

    #[test]
    fn a_path_with_backslashes_is_escaped() {
        // a windows path is full of backslashes, and each one is a C escape
        let mut builder = FunctionBuilder::new("f", RType::INT);
        let a = builder.param("a", RType::INT);
        builder.at((0, 1));
        builder.terminate(Terminator::Return(Value::Register(a)));
        let mut module = ModuleIr::new("app");
        module.functions.push(builder.finish());
        module.lines = Some(by_ir::function::LineTable::new(
            r"C:\src\app.by",
            "def f(a): return a",
        ));
        let text = emit_module(&module);
        assert!(text.contains(r#"#line 1 "C:\\src\\app.by""#), "{text}");
    }

    #[test]
    fn a_parameter_with_a_default_may_be_omitted() {
        let mut builder = FunctionBuilder::new("offset", RType::INT);
        let a = builder.param("a", RType::INT);
        let b = builder.param("b", RType::INT);
        builder.defaults(vec![None, Some(Value::Int(10))]);
        let out = builder.temp(RType::INT);
        builder.push(Op::IntBinary {
            dest: out,
            op: BinOp::Add,
            lhs: Value::Register(a),
            rhs: Value::Register(b),
        });
        builder.terminate(Terminator::Return(Value::Register(out)));
        let mut module = ModuleIr::new("app");
        module.functions.push(builder.finish());
        let c = emit_module(&module);
        // a missing argument takes the default rather than being an arity error
        assert!(c.contains("if (by_bound[1] != NULL) {"), "{c}");
        assert!(c.contains("a1 = By_ShortFrom(10);"), "{c}");
        // and the runtime reports the one without a default, in cpython's wording
        assert!(
            c.contains("static const unsigned char by_required[] = { 1, 0 };"),
            "{c}"
        );
    }

    #[test]
    fn a_refcounted_default_hands_over_a_new_reference() {
        // the wrapper releases its arguments, so an interned literal handed straight
        // over would be released twice
        let mut builder = FunctionBuilder::new("greet", RType::STR);
        let name = builder.param("name", RType::STR);
        builder.defaults(vec![Some(Value::Str("hi".to_string()))]);
        builder.terminate(Terminator::Return(Value::Register(name)));
        let mut module = ModuleIr::new("app");
        module.functions.push(builder.finish());
        let c = emit_module(&module);
        assert!(c.contains("a0 = By_NewRef(by_str0);"), "{c}");
    }

    /// a class whose `__init__` takes `value`, with a default only the interpreted
    /// definition holds
    fn holder() -> ModuleIr {
        let mut builder = FunctionBuilder::new("__init__", RType::NONE);
        let receiver = builder.param("self", RType::OBJECT);
        let value = builder.param("value", RType::OBJECT);
        builder.defaults(vec![None, None]);
        builder.computed_defaults(vec![1]);
        builder.push(Op::SetField {
            receiver: Value::Register(receiver),
            class: "Holder".to_string(),
            field: "value".to_string(),
            value: Value::Register(value),
        });
        builder.terminate(Terminator::Return(Value::None));
        let mut init = builder.finish();
        init.owner = Some("Holder".to_string());
        let mut module = ModuleIr::new("app");
        module.classes.push(ClassIr {
            name: "Holder".to_string(),
            immutable: false,
            exported: true,
            base: None,
            inherited_init: false,
            generic: false,
            declares_slots: false,
            constants: Vec::new(),
            properties: Vec::new(),
            slot_aliases: Vec::new(),
            fields: vec![by_ir::function::FieldDecl {
                name: "value".to_string(),
                ty: RType::OBJECT,
                default: None,
                optional: false,
                defaulted_by: None,
            }],
            decorators: Vec::new(),
            methods: vec![init],
            resume: None,
            keywords: Vec::new(),
        });
        module
    }

    /// the body of the C function `signature` opens, which is what lets a claim be
    /// made about one boundary rather than about whichever emitted it first
    fn body_of<'a>(c: &'a str, signature: &str) -> &'a str {
        let start = c
            .find(signature)
            .unwrap_or_else(|| panic!("{signature} is not emitted:\n{c}"));
        let rest = &c[start..];
        let end = rest
            .find("\n}\n")
            .unwrap_or_else(|| panic!("{signature} does not end:\n{rest}"));
        &rest[..end]
    }

    #[test]
    fn a_constructors_computed_default_is_optional_and_hands_the_call_over() {
        let c = emit_module(&holder());
        let slot = body_of(&c, "static int By_app_Holder_Type_init(");
        // the arity is the *written* one: it is the value that is unavailable here,
        // not the parameter
        assert!(
            slot.contains("static const unsigned char by_required[] = { 0 };"),
            "{slot}"
        );
        // and the call that omits it goes to the definition holding the one object
        // every such call has to share
        assert!(
            slot.contains(
                "if (by_bound[0] == NULL) return By_InitInterpreted(byi_app_Holder___init__, \
                 \"Holder.__init__\", selfobj, args, kwds);"
            ),
            "{slot}"
        );
        // which means the slot needs the handle the method wrapper already takes
        assert!(
            c.contains("static PyObject *byi_app_Holder___init__ = NULL;"),
            "{c}"
        );
    }

    #[test]
    fn a_nested_functions_computed_default_is_filled_from_its_environment() {
        // a nested function has no interpreted definition to hand the call to, so its
        // boundary fills the omitted parameter itself, out of the environment the frame
        // parked the value in. there is nothing to defer to, and emitting a defer edge
        // anyway would have module init reaching for a twin under the environment
        // class's generated name
        let mut builder = FunctionBuilder::new("cb", RType::OBJECT);
        let _env = builder.param(
            "$env",
            RType::Instance {
                class: "outer$env".to_string(),
                exact: false,
            },
        );
        let held = builder.param("name", RType::OBJECT);
        builder.defaults(vec![None, None]);
        builder.computed_defaults(vec![1]);
        builder.defaults_held_by_the_receiver();
        builder.terminate(Terminator::Return(Value::Register(held)));
        let mut method = builder.finish();
        method.owner = Some("outer$env".to_string());
        let mut module = ModuleIr::new("app");
        module.classes.push(ClassIr {
            name: "outer$env".to_string(),
            immutable: false,
            exported: false,
            base: None,
            inherited_init: false,
            generic: false,
            declares_slots: false,
            constants: Vec::new(),
            properties: Vec::new(),
            slot_aliases: Vec::new(),
            fields: vec![by_ir::function::FieldDecl {
                name: "$default$cb$name".to_string(),
                ty: RType::OBJECT,
                default: None,
                optional: false,
                defaulted_by: None,
            }],
            decorators: Vec::new(),
            methods: vec![method],
            resume: None,
            keywords: Vec::new(),
        });
        let c = emit_module(&module);
        // the opening brace, because the forward declaration of the same boundary comes
        // first and `body_of` takes whichever it finds
        let wrapper = body_of(
            &c,
            "static PyObject *byw_app_outer_env_cb(PyObject *self, PyObject *const *args, \
             Py_ssize_t nargs, PyObject *kwnames) {",
        );
        // the parameter stays optional, and the omitted case reads the field rather
        // than reporting a missing argument
        assert!(
            wrapper.contains("static const unsigned char by_required[] = { 0 };"),
            "{wrapper}"
        );
        assert!(
            wrapper.contains("By_ReadCell(a0->by_gf__default_cb_name, \"name\", 0)"),
            "{wrapper}"
        );
        // and no edge to a definition that was never bound under a name
        assert!(!wrapper.contains("by_wrap_defer"), "{wrapper}");
        assert!(!c.contains("byi_app_outer_env_cb"), "{c}");
    }

    /// a class whose `__init__` is `def __init__(self, a, /, *rest, **extra)`: a
    /// receiver behind a positional-only marker, a `*args` and a `**kwargs` at once
    fn variadic_init() -> ModuleIr {
        let mut builder = FunctionBuilder::new("__init__", RType::NONE);
        let receiver = builder.param("self", RType::OBJECT);
        let a = builder.param("a", RType::OBJECT);
        let rest = builder.param("rest", RType::OBJECT);
        let extra = builder.param("extra", RType::OBJECT);
        builder.defaults(vec![None, None, None, None]);
        builder.variadic(true, true);
        // `self` is positional-only along with `a`, which is what a `/` after the two
        // of them means
        builder.binding_kinds(2, 0);
        for (name, value) in [("a", a), ("rest", rest), ("extra", extra)] {
            builder.push(Op::SetField {
                receiver: Value::Register(receiver),
                class: "Var".to_string(),
                field: name.to_string(),
                value: Value::Register(value),
            });
        }
        builder.terminate(Terminator::Return(Value::None));
        let mut init = builder.finish();
        init.owner = Some("Var".to_string());
        let mut module = ModuleIr::new("app");
        module.classes.push(ClassIr {
            name: "Var".to_string(),
            immutable: false,
            exported: true,
            base: None,
            inherited_init: false,
            generic: false,
            declares_slots: false,
            constants: Vec::new(),
            properties: Vec::new(),
            slot_aliases: Vec::new(),
            fields: ["a", "rest", "extra"]
                .into_iter()
                .map(|name| by_ir::function::FieldDecl {
                    name: name.to_string(),
                    ty: RType::OBJECT,
                    default: None,
                    optional: false,
                    defaulted_by: None,
                })
                .collect(),
            decorators: Vec::new(),
            methods: vec![init],
            resume: None,
            keywords: Vec::new(),
        });
        module
    }

    #[test]
    fn a_constructor_builds_its_variadic_parameters_rather_than_binding_them() {
        let c = emit_module(&variadic_init());
        let slot = body_of(&c, "static int By_app_Var_Type_init(");
        // only `a` is bound: the other two are what is *left over*, so treating them as
        // named parameters made a call supplying neither an arity error
        assert!(
            slot.contains("static const char *const by_names[] = { \"a\" };"),
            "{slot}"
        );
        assert!(
            slot.contains("static const unsigned char by_required[] = { 1 };"),
            "{slot}"
        );
        // `posonly` counts the receiver the slot is handed separately, so it shifts
        assert!(
            slot.contains(
                "if (By_BindInit(args, kwds, by_names, 1, by_required, 1, 0, by_bound, 1, 1, \
                 \"Var.__init__\", 0) < 0) return -1;"
            ),
            "{slot}"
        );
        assert!(slot.contains("a1 = By_PackInitArgs(args, 1);"), "{slot}");
        assert!(
            slot.contains("a2 = By_PackInitKwargs(kwds, by_names, 1, 1);"),
            "{slot}"
        );
    }

    #[test]
    fn the_method_wrapper_shifts_the_same_counts_the_slot_does() {
        // the two boundaries bind the same signature and must read it the same way —
        // an unshifted `posonly` here made `a`'s neighbour unreachable by name
        let c = emit_module(&variadic_init());
        // the brace is what tells the definition from the forward declaration
        let wrapper = body_of(
            &c,
            "byw_app_Var___init__(PyObject *self, PyObject *const *args, Py_ssize_t nargs, PyObject *kwnames) {",
        );
        assert!(
            wrapper.contains(
                "if (By_BindArgs(args, nargs, kwnames, by_names, 1, by_required, 1, 0, \
                 by_bound, 1, 1, \"Var.__init__\", 1) < 0) return NULL;"
            ),
            "{wrapper}"
        );
        assert!(
            wrapper.contains("a2 = By_PackArgs(args, nargs, 1);"),
            "{wrapper}"
        );
        assert!(
            wrapper.contains("a3 = By_PackKwargs(args, nargs, kwnames, by_names, 1, 1);"),
            "{wrapper}"
        );
    }

    /// the index a scan reaches this with is already in a register, and the fused
    /// comparison takes one as readily as a tagged int — so the representation the
    /// index arrives in is what picks the helper, and nothing is shifted out and
    /// straight back in
    #[test]
    fn a_character_comparison_reads_the_index_in_the_representation_it_arrives_in() {
        let compare = |width: RType| {
            let mut builder = FunctionBuilder::new("at", RType::BIT);
            let text = builder.param("s", RType::STR);
            let index = builder.local("i", width);
            let answer = builder.temp(RType::BIT);
            builder.push(Op::StrItemCompare {
                dest: answer,
                op: CmpOp::Eq,
                container: Value::Register(text),
                index: Value::Register(index),
                character: ' ',
            });
            builder.terminate(Terminator::Return(Value::Register(answer)));
            emit_module(&module_with(builder.finish()))
        };
        assert!(
            compare(RType::INT).contains("By_StrItemCompareChar(r0, r1, 32, Py_EQ)"),
            "a tagged index keeps the tagged helper"
        );
        assert!(
            compare(RType::fixed(IntWidth::I64))
                .contains("By_StrItemCompareCharI64(r0, r1, 32, Py_EQ)"),
            "a machine index reads through the machine helper"
        );
    }

    /// a closure takes the *address* of a table entry, so the index has to be into the
    /// table as emitted — past the synthetic entries a generic or resumable class carries,
    /// and not counting the `__new__` the table leaves out. it used to be read off the
    /// unfiltered method list with no prefix, which is right only for a class that has
    /// neither, and every closure environment built so far happens to be one.
    ///
    /// the two effects are asserted apart because together they cancel: a `__new__`
    /// dropped and a `__class_getitem__` added land on the number the old reading gave
    #[test]
    fn a_closure_points_at_the_entry_the_table_actually_holds() {
        let index_of_call = |generic: bool, with_new: bool| {
            let entry = |name: &str| {
                let mut builder = FunctionBuilder::new(name, RType::OBJECT);
                builder.param("self", RType::OBJECT);
                builder.terminate(Terminator::Return(Value::None));
                let mut method = builder.finish();
                method.owner = Some("Env".to_string());
                method
            };
            let mut class = appending_class();
            class.name = "Env".to_string();
            class.base = None;
            class.generic = generic;
            class.methods = if with_new {
                vec![entry("__new__"), entry("call")]
            } else {
                vec![entry("call")]
            };

            let mut builder = FunctionBuilder::new("make", RType::OBJECT);
            let env = builder.param("env", RType::OBJECT);
            let closure = builder.temp(RType::OBJECT);
            builder.push(Op::MakeClosure {
                dest: closure,
                class: "Env".to_string(),
                method: "call".to_string(),
                env: Value::Register(env),
            });
            builder.terminate(Terminator::Return(Value::Register(closure)));

            let mut module = module_with(builder.finish());
            module.classes = vec![class];
            emit_module(&module)
        };

        // a `__new__` is not in the table, so the method after it is still entry zero.
        // the unfiltered reading said one, which is the entry past the end here
        let dropped = index_of_call(false, true);
        assert!(
            dropped.contains("By_MakeClosure(&By_app_Env_Type_methods[0],"),
            "{dropped}"
        );

        // and a generic class opens with `__class_getitem__`, so its own first method is
        // entry one. the unfiltered reading said zero, which is `__class_getitem__`
        let shifted = index_of_call(true, false);
        assert!(
            shifted.contains("By_MakeClosure(&By_app_Env_Type_methods[1],"),
            "{shifted}"
        );
        let table = &shifted[shifted
            .find("static PyMethodDef By_app_Env_Type_methods[]")
            .expect("a table")..];
        let table = &table[..table.find("};").expect("a table end")];
        assert!(table.contains("__class_getitem__"), "{table}");
    }
}
