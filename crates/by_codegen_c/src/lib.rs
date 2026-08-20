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

use std::collections::BTreeSet;
use std::fmt::Write;

use by_ir::function::{
    Binding, ClassBase, ClassIr, Function, KeywordValue, ModuleIr, RegisterDecl, Surface,
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

fn mangle_member(name: &str) -> String {
    by_ir::function::FieldDecl {
        name: name.to_string(),
        ty: RType::OBJECT,
        default: None,
        optional: false,
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

    for tuple in collect_tuples(module) {
        out.push_str(&emit_tuple_struct(module, &tuple));
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
fn collect_tuples(module: &ModuleIr) -> BTreeSet<Vec<RType>> {
    let mut tuples = BTreeSet::new();
    let visit = |ty: &RType, tuples: &mut BTreeSet<Vec<RType>>| {
        if let RType::Tuple(items) = ty {
            tuples.insert(items.to_vec());
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
    let _ = writeln!(out, " }} ByTuple{};", tuple_mangle(items));
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
         \x20   By_VisitManagedDict((PyObject *)self, visit, arg);\n\
         \x20   return 0;\n}}\n\n\
         static int {type_name}_clear({struct_name} *self) {{\n\
         {clears}\
         \x20   By_ClearManagedDict((PyObject *)self);\n\
         \x20   return 0;\n}}\n\n"
    )
}

/// `tp_dealloc`, `tp_traverse` and `tp_clear` for a class whose fields sit past a
/// base's instance
///
/// everything else about inheriting a layout says "supply no slots, the base allocates
/// and frees" — but the base cannot know about storage appended after its own data, so
/// without these three the appended fields simply leak. the traverse and clear are not
/// optional either: a base like `Exception` is a GC type, so ours is, and a field the
/// collector cannot see holds its cycle alive forever
fn emit_appended_storage(module: &ModuleIr, class: &ClassIr) -> String {
    let struct_name = class.struct_name(module.name.dotted());
    let type_name = class.type_name(module.name.dotted());
    // the *declaring* type, not `Py_TYPE(self)`: a python subclass of this class is a
    // different type whose data area is somewhere else again, and the base to chain to
    // is this class's base rather than that subclass's
    let declared = format!("((PyTypeObject *){type_name}_OBJ)");
    let fields = format!("({struct_name} *)By_TypeData(self, {declared})");
    let mut out = String::new();

    let mut visits = String::new();
    let mut clears = String::new();
    for field in class.fields.iter().filter(|field| collectable(field)) {
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
         \x20   {struct_name} *by_f = {fields};\n\
         {visits}\
         \x20   PyTypeObject *by_base = {declared}->tp_base;\n\
         \x20   if (by_base->tp_traverse) {{\n\
         \x20       int by_r = by_base->tp_traverse(self, visit, arg);\n\
         \x20       if (by_r) return by_r;\n\
         \x20   }}\n\
         \x20   if (!(by_base->tp_flags & Py_TPFLAGS_HEAPTYPE)) Py_VISIT(Py_TYPE(self));\n\
         \x20   return 0;\n}}\n\n\
         static int {type_name}_clear(PyObject *self) {{\n\
         \x20   {struct_name} *by_f = {fields};\n\
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
    for field in &class.fields {
        if let Some(release) = dec_ref(&field.ty, &format!("by_f->{}", field.member())) {
            let _ = writeln!(out_slot(&mut releases), "    {release}");
        }
    }
    // and dropped by exactly one rung, the same one the traverse reports it from: a base
    // that is itself a heap type has a deallocator of its own that drops it, and two
    // drops for the one reference free the type underneath everything still using it
    let _ = write!(
        out,
        "static void {type_name}_dealloc(PyObject *self) {{\n\
         \x20   PyTypeObject *by_type = Py_TYPE(self);\n\
         \x20   PyTypeObject *by_base = {declared}->tp_base;\n\
         \x20   if (PyType_HasFeature(by_type, Py_TPFLAGS_HAVE_GC)) PyObject_GC_UnTrack(self);\n\
         \x20   {{ {struct_name} *by_f = {fields};\n\
         {releases}\
         \x20   }}\n\
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
    let mut out = format!(
        "typedef struct {} {{\n{header}",
        class.struct_name(module.name.dotted())
    );
    // where a `return` puts its value. a resumable frame reports finishing by writing
    // here and handing back nothing, so that the slot python asks with — `am_send` —
    // can say what the frame returned without an exception ever being built. it is not
    // one of the frontend's fields because no python code can name it and nothing
    // parks across a suspension in it: it is written once, on the way out
    if class.resume.is_some() {
        out.push_str("    PyObject *by_returned;\n");
    }
    for field in &class.fields {
        let _ = writeln!(out, "    {} {};", ctype(module, &field.ty), field.member());
        // `tp_alloc` zeroes the instance, so "never written" is the state an object
        // starts in and the constructor has nothing to do
        if field.optional {
            let _ = writeln!(out, "    char {};", field.presence());
        }
    }
    let _ = writeln!(out, "}} {};", class.struct_name(module.name.dotted()));
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
    if external_storage(module, class) {
        out.push_str(&emit_appended_storage(module, class));
    } else {
        if keeps_a_dict {
            out.push_str(&emit_collected_instance(module, class));
        }
        // dealloc releases each refcounted field, then the object
        let _ = writeln!(
            out,
            "static void {type_name}_dealloc({struct_name} *self) {{"
        );
        // a collected instance is on the collector's list until it says otherwise, and
        // a list holding a half-freed object is what the next collection walks
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
            out.push_str("    By_ClearManagedDict((PyObject *)self);\n");
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
    let _ = writeln!(out, "    {}", bind_self(module, class, "selfobj"));
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
        let release = release_old(field);
        if !release.is_empty() {
            let _ = writeln!(out, "      {release}");
        }
        let _ = writeln!(out, "      self->{} = by_v;", field.member());
        // the byte beside an optional field is what a later read and the deallocation
        // both ask, so filling one here has to answer them
        if field.optional {
            let _ = writeln!(out, "      self->{} = 1;", field.presence());
        }
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
fn release_old(field: &by_ir::function::FieldDecl) -> String {
    let Some(release) = dec_ref(&field.ty, &format!("self->{}", field.member())) else {
        return String::new();
    };
    if field.optional {
        format!("if (self->{}) {{ {release} }}", field.presence())
    } else {
        release
    }
}

/// the assignment that records an optional field as written
fn mark_present(field: &by_ir::function::FieldDecl) -> String {
    if field.optional {
        format!("\x20   self->{} = 1;\n", field.presence())
    } else {
        String::new()
    }
}

/// the getters, setters, slot table and type spec python sees
fn emit_class_members(module: &ModuleIr, class: &ClassIr) -> String {
    let struct_name = class.struct_name(module.name.dotted());
    let type_name = class.type_name(module.name.dotted());
    let mut out = String::new();

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
            bind_self(module, class, "selfobj"),
            box_borrowed(&field.ty, &format!("self->{}", field.member()))
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
             \x20   if (by_value == NULL) {{\n\
             \x20       PyErr_SetString(PyExc_AttributeError, \"cannot delete an attribute\");\n\
             \x20       return -1;\n\
             \x20   }}\n\
             \x20   {} by_v = {};\n\
             \x20   if ({}) return -1;\n\
             \x20   {}\n\
             \x20   self->{} = by_v;\n\
             {}\
             \x20   return 0;\n}}\n",
            field.name,
            bind_self(module, class, "selfobj"),
            ctype(module, &field.ty),
            unbox_checked(module, &field.ty, "by_value"),
            error_check(&field.ty, "by_v"),
            release_old(field),
            field.member(),
            mark_present(field)
        );
    }
    let _ = writeln!(out, "static PyGetSetDef {type_name}_getset[] = {{");
    for field in &class.fields {
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
    // a class keeping an instance dict answers `__dict__` with it — but only where the
    // dict is the whole of an instance's state. a class with fields of its own keeps
    // them in its layout, and a mapping that named none of them would be an *empty*
    // answer where the interpreted class gives a full one: quiet, and wrong. the
    // refusal such a class already gives is at least loud
    if instance_dict(module, class) && class.fields.is_empty() {
        out.push_str(
            "    {\"__dict__\", PyObject_GenericGetDict, PyObject_GenericSetDict, NULL, NULL},\n",
        );
    }
    out.push_str("    {NULL, NULL, NULL, NULL, NULL}\n};\n\n");

    // the method table, using each method's python wrapper
    // `send` stores the value the `yield` expression evaluates to, then resumes.
    // `close` marks the machine exhausted — with `yield` inside `try` declined there
    // is no handler to run first
    if let Some(resume) = &class.resume {
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
             \x20   PyObject *by_old = self->{};\n\
             \x20   self->{} = By_NewRef(args[0]);\n\
             \x20   Py_XDECREF(by_old);\n\
             \x20   return By_StepGenerator(selfobj, &self->by_returned, &self->{state},\n\
             \x20                           (PyObject *(*)(PyObject *)){symbol});\n}}",
            mangle_member(crate::GENERATOR_SENT),
            mangle_member(crate::GENERATOR_SENT),
            state = mangle_member(crate::GENERATOR_STATE)
        );
        // `close` throws `GeneratorExit` in, which runs every enclosing `finally`, then
        // marks the machine exhausted whatever came back
        let _ = writeln!(
            out,
            "static PyObject *{type_name}_close(PyObject *selfobj, PyObject *const *args, Py_ssize_t nargs) {{\n\
             \x20   (void)args; (void)nargs;\n\
             \x20   {struct_name} *self = ({struct_name} *)selfobj;\n\
             \x20   int by_r = By_CloseGenerator(selfobj, &self->{}, &self->by_returned,\n\
             \x20                                &self->{state},\n\
             \x20                                (PyObject *(*)(PyObject *)){symbol});\n\
             \x20   By_FinishGenerator(&self->{state});\n\
             \x20   if (by_r < 0) return NULL;\n\
             \x20   Py_RETURN_NONE;\n}}",
            mangle_member(crate::GENERATOR_THROWN),
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
             \x20   return By_ThrowInto(selfobj, &(({struct_name} *)selfobj)->{},\n\
             \x20                       &(({struct_name} *)selfobj)->by_returned,\n\
             \x20                       &(({struct_name} *)selfobj)->{state}, args[0],\n\
             \x20                       (PyObject *(*)(PyObject *)){symbol});\n}}",
            mangle_member(crate::GENERATOR_THROWN),
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
    if class.generic {
        let _ = writeln!(
            out,
            "    {{\"__class_getitem__\", (PyCFunction){type_name}_class_getitem, METH_O | METH_CLASS, NULL}},"
        );
    }
    if class.resume.is_some() {
        let _ = writeln!(
            out,
            "    {{\"send\", (PyCFunction)(void(*)(void)){type_name}_send, METH_FASTCALL, NULL}},\n\
             \x20   {{\"throw\", (PyCFunction)(void(*)(void)){type_name}_throw, METH_FASTCALL, NULL}},\n\
             \x20   {{\"close\", (PyCFunction)(void(*)(void)){type_name}_close, METH_FASTCALL, NULL}},"
        );
    }
    // an async generator's cleanup is asked for through an *awaitable*, which is the
    // same shape `__anext__` hands back
    if class
        .resume
        .as_ref()
        .is_some_and(|resume| resume.surface == Surface::AsyncGenerator)
    {
        let _ = writeln!(
            out,
            "    {{\"aclose\", (PyCFunction)(void(*)(void)){type_name}_aclose, METH_FASTCALL, NULL}},\n\
             \x20   {{\"asend\", (PyCFunction)(void(*)(void)){type_name}_do_asend, METH_FASTCALL, NULL}},\n\
             \x20   {{\"athrow\", (PyCFunction)(void(*)(void)){type_name}_do_athrow, METH_FASTCALL, NULL}},"
        );
    }
    for method in &class.methods {
        // `METH_STATIC` and `METH_CLASS` are masked off before the calling convention
        // is read, so either combines with the fastcall the wrapper is written for.
        // what they change is the descriptor the type publishes — a `staticmethod` or a
        // `classmethod_descriptor` rather than a plain `method_descriptor`
        let _ = writeln!(
            out,
            "    {{\"{}\", (PyCFunction)(void(*)(void)){}, METH_FASTCALL | METH_KEYWORDS{}, NULL}},",
            method.name,
            method.wrapper_symbol(module.name.dotted()),
            method
                .binding
                .method_flag()
                .map(|flag| format!(" | {flag}"))
                .unwrap_or_default()
        );
    }
    out.push_str("    {NULL, NULL, 0, NULL}\n};\n\n");

    // a generator's state object *is* the iterator: `tp_iternext` drives `$resume`,
    // which returns the next yielded value or raises `StopIteration`
    let iterator = match &class.resume {
        None => String::new(),
        Some(resume) => {
            let symbol = class
                .methods
                .iter()
                .find(|method| method.name == resume.method)
                .map(|method| method.native_symbol(module.name.dotted()))
                .unwrap_or_default();
            let _ = writeln!(
                out,
                "static PyObject *{type_name}_iternext(PyObject *self) {{\n\
                 \x20   return By_StepGenerator(self, &(({struct_name} *)self)->by_returned,\n\
                 \x20                           &(({struct_name} *)self)->{state},\n\
                 \x20                           (PyObject *(*)(PyObject *)){symbol});\n}}",
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
                    "#if PY_VERSION_HEX >= 0x030A0000\n\
                     static PySendResult {type_name}_send_slot(PyObject *self, PyObject *by_arg,\n\
                     \x20                                     PyObject **by_result) {{\n\
                     \x20   return By_SendGenerator(self, &(({struct_name} *)self)->{sent},\n\
                     \x20                           &(({struct_name} *)self)->by_returned,\n\
                     \x20                           &(({struct_name} *)self)->{state},\n\
                     \x20                           (PyObject *(*)(PyObject *)){symbol},\n\
                     \x20                           by_arg, by_result);\n}}\n\
                     #endif",
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
                     \x20   if (by_carried != NULL && by_self->by_mode == 3) {{\n\
                     \x20       by_step = By_ThrowInto((PyObject *)by_gen, &by_gen->{thrown},\n\
                     \x20                              &by_gen->by_returned,\n\
                     \x20                              &by_gen->{state}, by_carried,\n\
                     \x20                              (PyObject *(*)(PyObject *)){symbol});\n\
                     \x20       Py_DECREF(by_carried);\n\
                     \x20   }} else {{\n\
                     \x20       if (by_carried != NULL) {{\n\
                     \x20           PyObject *by_old = by_gen->{sent};\n\
                     \x20           by_gen->{sent} = by_carried;\n\
                     \x20           Py_XDECREF(by_old);\n\
                     \x20       }}\n\
                     \x20       by_step = By_StepGenerator((PyObject *)by_gen, &by_gen->by_returned,\n\
                     \x20                                  &by_gen->{state},\n\
                     \x20                                  (PyObject *(*)(PyObject *)){symbol});\n\
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
                     #if PY_VERSION_HEX >= 0x030A0000\n\
                     \x20   .am_send = {type_name}_send_slot,\n\
                     #endif\n}};"
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
                    "#if PY_VERSION_HEX >= 0x030A0000\n\
                     static PyAsyncMethods {type_name}_async = {{\n\
                     \x20   .am_send = {type_name}_send_slot,\n}};\n\
                     #endif"
                );
                format!(
                    "#if PY_VERSION_HEX >= 0x030A0000\n             .tp_as_async = &{type_name}_async,\n#endif\n             .tp_iter = PyObject_SelfIter,\n             .tp_iternext = {type_name}_iternext,\n             .tp_finalize = {type_name}_finalize,\n"
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
    let collected = if external_storage(module, class) {
        " | Py_TPFLAGS_HAVE_GC"
    } else if instance_dict(module, class) {
        // the dict holds whatever a decorator's generated code put in it, so the
        // collector has to be able to walk it — and a managed one is only allowed on a
        // type it can walk
        " | Py_TPFLAGS_HAVE_GC | BY_MANAGED_DICT_FLAG"
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
        ("0".to_string(), init)
    } else {
        // a class with nothing to initialize supplies neither slot, so both are inherited
        // together — `object.__init__` refuses an argument only while `tp_new` is
        // `object`'s too, and filling one of the pair alone would take one silently
        let construction = if init.is_empty() {
            String::new()
        } else {
            format!("{init}\x20   {{Py_tp_new, (void *)PyType_GenericNew}},\n")
        };
        // a collected type has to hand the collector both halves, or an instance in a
        // cycle is never reached at all
        let walked = if instance_dict(module, class) {
            format!(
                "\x20   {{Py_tp_traverse, (void *){type_name}_traverse}},\n\
                 \x20   {{Py_tp_clear, (void *){type_name}_clear}},\n"
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
/// a class decorator is arbitrary python handed the class, and what it hands back is
/// often code it *generated* from what it read — `@dataclass` writes an `__init__` that
/// assigns one attribute per annotation. that code is ordinary python and assumes an
/// ordinary instance, so on an emitted one, whose whole state is its layout, every
/// assignment falls off: `E(3)` raised where the interpreted class answered. the decline
/// that would otherwise be the answer is not open here — a class has no runtime fallback
/// — so the class is given the one thing the generated code needs instead.
///
/// a managed dict costs the layout nothing: python keeps it in the pre-header, so the
/// struct, its base's prefix and every offset a compiled function reads are untouched.
/// what it costs is collection — a type with a dict of arbitrary values must be one the
/// collector walks — which is why only the classes that need it take it.
///
/// only a class that owns its layout from `object`. one standing on a base outside the
/// module takes that base's answer about a dict, and a spec claiming one anyway would be
/// claiming room the base never allocated — which is how 24 of the `encodings` modules
/// once segfaulted
fn instance_dict(module: &ModuleIr, class: &ClassIr) -> bool {
    heap_type(module, class) && !inherits_layout(module, class) && decorated_chain(module, class)
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
/// whole module interpreted — see `By_SpecClass`. the classes it builds there and the
/// classes the rest of module init must *not* build again have to be the same set, so
/// both ask this rather than re-deriving it
fn appends_storage_from_a_spec(module: &ModuleIr, class: &ClassIr) -> bool {
    external_storage(module, class)
        && class.keywords.is_empty()
        && !stands_on_an_emitted_base(module, class)
}

/// the class this one appends its storage to, where that is one this module also builds
/// from a spec
///
/// such a base is the one heap type a spec can be built on: its `tp_dealloc`,
/// `tp_traverse` and `tp_clear` are ones this module emitted, and each of those reads
/// the base to chain to from the type that *declared* it rather than from
/// `Py_TYPE(self)` — so the chain walks down to the outside base and stops, where
/// `subtype_dealloc` would come straight back. see `By_SpecSubclass`.
///
/// the base has to come first in the module's order, because that is the order module
/// init builds them in and the subclass's spec stands on the finished type. a class
/// statement cannot name a base declared after it, so this only ever rules out a shape
/// the source could not have written.
///
/// and the subclass has to declare only what it *adds*. the other layout model for an
/// in-module base is the struct extension, where a subclass restates its base's fields
/// so that a pointer to one is a pointer to the other — restating them in an appended
/// region instead would give the pair two copies of each, and the base's methods and the
/// subclass's would write different ones. so a field the base already stores is the
/// signal that this is the other model, and it is not appended over anything
fn appended_over_an_emitted_base<'a>(module: &'a ModuleIr, class: &ClassIr) -> Option<&'a ClassIr> {
    let wanted = class.base.as_ref()?.in_module()?;
    let base = module
        .classes
        .iter()
        .take_while(|candidate| candidate.name != class.name)
        .find(|candidate| candidate.name == wanted)?;
    let restates = class.fields.iter().any(|field| {
        base.fields
            .iter()
            .any(|inherited| inherited.name == field.name)
    });
    (appends_storage_from_a_spec(module, base) && !restates).then_some(base)
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
fn metaclass_construction(class: &ClassIr) -> bool {
    class.fields.is_empty()
        // a resumable class is a generator's state object: its state *is* its fields,
        // so this is already false, and nothing in the language can name it as a base
        && class.resume.is_none()
        && !decorates_a_method(class)
}

/// whether any of this class's methods carries a decorator
///
/// a method decorator is applied to the finished type, which is the only place a spec
/// leaves for it — but a metaclass decides from the *namespace*, before that, so the
/// two would disagree about what the class defines
fn decorates_a_method(class: &ClassIr) -> bool {
    class
        .methods
        .iter()
        .any(|method| !method.decorators.is_empty())
}

/// the class in this module named `name`, when it has one
fn class_named<'a>(module: &'a ModuleIr, name: &str) -> Option<&'a ClassIr> {
    module.classes.iter().find(|class| class.name == name)
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

/// the field storage of a receiver named by the IR, as an expression
fn receiver_fields(module: &ModuleIr, class: &str, receiver: &Value) -> String {
    match class_named(module, class) {
        Some(owner) if external_storage(module, owner) => {
            fields_of(module, owner, &value_expr(receiver))
        }
        // the register already *is* the storage, and saying so again would only add a
        // cast to every field access in the module
        _ => value_expr(receiver),
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
/// subclass, each need the opposite — so those classes pay for it
fn mutable_type(module: &ModuleIr, class: &ClassIr) -> bool {
    !class.decorators.is_empty() || is_base(module, class) || class.base.is_some()
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
    method: &Function,
    receiver: &str,
    args: &[&str],
) -> String {
    format!(
        "    if (PyObject_TypeCheck({receiver}, (PyTypeObject *){type_name}_OBJ)) {{\n\
         \x20       PyObject *by_argv[] = {{ {} }};\n\
         \x20       return {}({receiver}, by_argv, {}, NULL);\n    }}\n",
        args.join(", "),
        method.wrapper_symbol(module.name.dotted()),
        args.len()
    )
}

/// `nb_power`, which `__pow__` and `__rpow__` share with the three-argument `pow`
fn emit_power_adapter(module: &ModuleIr, class: &ClassIr, type_name: &str) -> String {
    let (name, reflected, _, field) = POWER;
    let (forward, backward) = (dunder(class, name), dunder(class, reflected));
    if forward.is_none() && backward.is_none() {
        return String::new();
    }
    let mut binary = String::new();
    for (method, receiver, other) in [(forward, "by_a", "by_b"), (backward, "by_b", "by_a")] {
        let Some(method) = method else { continue };
        binary.push_str(&on_our_operand(
            module,
            type_name,
            method,
            receiver,
            &[other],
        ));
    }
    // a modulus reaches only the left operand's `__pow__`, and a class that wrote a
    // two-parameter one raises there — which is what python raises too
    let ternary = forward.map_or_else(String::new, |method| {
        on_our_operand(module, type_name, method, "by_a", &["by_b", "by_c"])
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

/// whether the class fills this slot, by either of the methods that can
fn fills_slot(class: &ClassIr, name: &str) -> bool {
    dunder(class, name).is_some()
        || slot_companion(name).is_some_and(|other| dunder(class, other).is_some())
}

/// `mp_ass_subscript`, which `__setitem__` and `__delitem__` share
///
/// a NULL value is `del obj[key]`. a class with only one of the two still fills
/// the slot, and the half it does not have raises what python raises when a slot
/// looks a missing method up — an `AttributeError` naming the method, rather than
/// the `TypeError` an absent protocol would give
fn emit_ass_subscript_adapter(module: &ModuleIr, class: &ClassIr, symbol: &str) -> String {
    let half = |method: Option<&Function>, absent: &str, argc: usize| match method {
        Some(method) => format!(
            "        PyObject *by_r = {}(self, by_argv, {argc}, NULL);\n\
             \x20       if (by_r == NULL) return -1;\n\
             \x20       Py_DECREF(by_r);\n\
             \x20       return 0;\n",
            method.wrapper_symbol(module.name.dotted())
        ),
        None => format!(
            "        PyErr_SetString(PyExc_AttributeError, {});\n\x20       return -1;\n",
            c_string(absent)
        ),
    };
    format!(
        "static int {symbol}(PyObject *self, PyObject *by_key, PyObject *by_value) {{\n\
         \x20   PyObject *by_argv[] = {{ by_key, by_value }};\n\
         \x20   if (by_value == NULL) {{\n\
         {}\x20   }}\n\
         {}}}\n",
        half(dunder(class, "__delitem__"), "__delitem__", 1),
        half(dunder(class, "__setitem__"), "__setitem__", 2)
    )
}

/// the adapters that give each dunder method the signature its slot wants
///
/// each is a call into the method's own wrapper, so the argument binding, the
/// representation checks and the boxing are the ones every other call gets
fn emit_dunder_adapters(module: &ModuleIr, class: &ClassIr, type_name: &str) -> String {
    let mut out = String::new();
    for (name, _, _, shape) in DUNDER_SLOTS {
        if !fills_slot(class, name) {
            continue;
        }
        let symbol = format!("{type_name}_{}", name.trim_matches('_'));
        // the shared slot is built from the *class*, because either of the two
        // methods that fill it may be the absent one
        if matches!(shape, SlotShape::SetItem) {
            out.push_str(&emit_ass_subscript_adapter(module, class, &symbol));
            continue;
        }
        let Some(method) = dunder(class, name) else {
            continue;
        };
        let call = format!(
            "{}(self, NULL, 0, NULL)",
            method.wrapper_symbol(module.name.dotted())
        );
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
                     \x20   return {}(self, by_argv, 1, NULL);\n}}",
                    method.wrapper_symbol(module.name.dotted())
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
                     \x20   return By_CallSlot({}, self, by_args, by_kw);\n}}",
                    method.wrapper_symbol(module.name.dotted())
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
                     \x20   return {}(self, by_argv, 1, NULL);\n}}",
                    method.wrapper_symbol(module.name.dotted())
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
                     \x20   return {}(self, by_argv, 2, NULL);\n}}",
                    method.wrapper_symbol(module.name.dotted())
                );
            }
            SlotShape::Contains => {
                let _ = writeln!(
                    out,
                    "static int {symbol}(PyObject *self, PyObject *by_value) {{\n\
                     \x20   PyObject *by_argv[] = {{ by_value }};\n\
                     \x20   PyObject *by_r = {}(self, by_argv, 1, NULL);\n\
                     \x20   if (by_r == NULL) return -1;\n\
                     \x20   int by_v = PyObject_IsTrue(by_r);\n\
                     \x20   Py_DECREF(by_r);\n\
                     \x20   return by_v;\n}}",
                    method.wrapper_symbol(module.name.dotted())
                );
            }
            SlotShape::Hash => {
                let _ = writeln!(
                    out,
                    "static Py_hash_t {symbol}(PyObject *self) {{\n\
                     \x20   PyObject *by_r = {call};\n\
                     \x20   if (by_r == NULL) return -1;\n\
                     \x20   Py_hash_t by_h = PyObject_Hash(by_r);\n\
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
        .any(|(name, _)| dunder(class, name).is_some())
    {
        let _ = writeln!(
            out,
            "static PyObject *{type_name}_richcompare(PyObject *self, PyObject *other, int op) {{\n\
             \x20   PyObject *by_argv[] = {{ other }};\n\
             \x20   switch (op) {{"
        );
        for (name, opcode) in COMPARISONS {
            let Some(method) = dunder(class, name) else {
                continue;
            };
            let _ = writeln!(
                out,
                "    case {opcode}: return {}(self, by_argv, 1, NULL);",
                method.wrapper_symbol(module.name.dotted())
            );
        }
        // a comparison the class does not define is not an error: answering
        // `NotImplemented` is what lets the other operand's type try
        out.push_str("    }\n    Py_RETURN_NOTIMPLEMENTED;\n}\n");
    }

    for (name, reflected, _, field) in ARITHMETIC {
        let (forward, backward) = (dunder(class, name), dunder(class, reflected));
        if forward.is_none() && backward.is_none() {
            continue;
        }
        let _ = writeln!(
            out,
            "static PyObject *{type_name}_{field}(PyObject *by_a, PyObject *by_b) {{"
        );
        for (method, receiver, other) in [(forward, "by_a", "by_b"), (backward, "by_b", "by_a")] {
            let Some(method) = method else { continue };
            out.push_str(&on_our_operand(
                module,
                type_name,
                method,
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
    if dunder(class, "__len__").is_some() {
        let _ = writeln!(mapping, "    .mp_length = {type_name}_len,");
    }
    if dunder(class, "__getitem__").is_some() {
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
    if dunder(class, "__contains__").is_some() {
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
        if dunder(class, name).is_some() || dunder(class, reflected).is_some() {
            let _ = writeln!(out, "    .{field} = {type_name}_{field},");
        }
    }
    out.push_str(&sub_table_fields(class, type_name, "tp_as_number"));
    out
}

/// the value `tp_hash` takes, when the class does not set it itself
///
/// python makes a class that defines `__eq__` and not `__hash__` unhashable — two
/// objects that compare equal have to hash equal, and an inherited hash cannot
/// promise that. `type_new` does it for a class written in python; a type built
/// from a spec has to do it here or the compiled class would be hashable where
/// the interpreted one is not
fn inherited_hash(class: &ClassIr) -> Option<&'static str> {
    let defines_equality = COMPARISONS
        .iter()
        .any(|(name, _)| matches!(*name, "__eq__" | "__ne__") && dunder(class, name).is_some());
    (defines_equality && dunder(class, "__hash__").is_none())
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
        .any(|(name, _)| dunder(class, name).is_some())
    {
        slots.push((
            "Py_tp_richcompare".to_string(),
            format!("{type_name}_richcompare"),
        ));
    }
    if let Some(hash) = inherited_hash(class) {
        slots.push(("Py_tp_hash".to_string(), hash.to_string()));
    }
    for (name, reflected, slot, field) in ARITHMETIC.iter().copied().chain([POWER]) {
        if dunder(class, name).is_some() || dunder(class, reflected).is_some() {
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
        .any(|(name, _)| dunder(class, name).is_some())
    {
        let _ = writeln!(
            out,
            "             .tp_richcompare = {type_name}_richcompare,"
        );
    }
    if let Some(hash) = inherited_hash(class) {
        let _ = writeln!(out, "             .tp_hash = {hash},");
    }
    // `__bool__` names the number table already; an arithmetic method without one
    // still needs it pointed at
    if dunder(class, "__bool__").is_none() && !number_fields(class, type_name).is_empty() {
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
        _ => format!("By_NewRef((PyObject *)({expr}))"),
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

    for (index, decl) in function
        .registers
        .iter()
        .enumerate()
        .skip(function.param_count)
    {
        let _ = writeln!(
            out,
            "    {} {} = {};",
            ctype(module, &decl.ty),
            local(RegisterId(index)),
            decl.ty.undefined()
        );
        // a local some path reaches without writing carries the answer to whether it
        // was written. it starts at 0 because no path has written it yet
        if decl.may_be_unassigned {
            let _ = writeln!(
                out,
                "    char {} = 0;",
                by_ir::function::RegisterDecl::presence(RegisterId(index))
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
            out.push_str(&mark_assigned(function, op.dest()));
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
        out.push_str(&emit_cleanup(function, "    ", None));
        let _ = writeln!(out, "    return {};", function.ret.undefined());
    }

    out.push_str("}\n");
    out
}

/// release the registers an exit in `block` owns
///
/// `live` is the refcount pass's answer for this block, when it ran. without it
/// every owned register is released, which is correct and merely wasteful
fn emit_cleanup(function: &Function, indent: &str, live: Option<&[RegisterId]>) -> String {
    let owned = owned_registers(function);
    let mut out = String::new();
    for (index, decl) in function.registers.iter().enumerate() {
        if owned.get(index).copied() != Some(true) {
            continue; // a borrowed parameter belongs to the caller
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

fn c_string(text: &str) -> String {
    c_byte_string(text.as_bytes())
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
    let mut out = String::new();
    for value in values {
        let Value::Register(id) = value else {
            continue;
        };
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
fn mark_assigned(function: &Function, dest: Option<RegisterId>) -> String {
    let Some(dest) = dest else {
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
            // the frame's — so an assignment never has an error edge of its own, and
            // one whose destination holds no reference is a plain store
            if !decl.ty.is_refcounted() {
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
                if let Some(retain) = inc_ref(&field.ty, &value_expr(value)) {
                    let _ = writeln!(out, "      {retain}");
                }
                let _ = writeln!(
                    out,
                    "      by_new->{} = {};",
                    field.member(),
                    value_expr(value)
                );
                // the zero `tp_alloc` left says "never written", so a field filled here
                // has to say otherwise
                if field.optional {
                    let _ = writeln!(out, "      by_new->{} = 1;", field.presence());
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
        Op::RaiseWith { error, value } => {
            // a resumable frame's `return` is the one raise that is not really one: the
            // value is written into the state object and the frame hands back nothing,
            // so that `am_send` can report a return for the price of a pointer read
            // instead of building a `StopIteration` for its caller to unpack again.
            // whoever owes python an exception builds it from there, in `By_TakeReturn`.
            //
            // only the form that carries a value takes this route. a bare `return` is
            // lowered to the same op a written `raise StopIteration` is, and the two
            // must not be confused: one finishes the frame, the other is an exception
            // the body chose to raise
            if *error == by_ir::ops::StandardError::StopIteration && resumes(module, function) {
                let receiver = local(RegisterId(0));
                format!(
                    "    {{ PyObject *by_t = By_NewRef({});\n\
                     \x20     Py_XDECREF({receiver}->by_returned);\n\
                     \x20     {receiver}->by_returned = by_t; }}\n\
                     \x20   goto {};\n",
                    value_expr(value),
                    error_label(error_target)
                )
            } else {
                format!(
                    "    By_RaiseWith({}, {});\n    goto {};\n",
                    error.c_name(),
                    value_expr(value),
                    error_label(error_target)
                )
            }
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
            let Some(index) = owner
                .methods
                .iter()
                .position(|candidate| candidate.name == *method)
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
            let _ = writeln!(
                out,
                "      PyObject *by_t = By_LookupGlobal(by_module_dict, {slot});"
            );
            out.push_str(&commit_checked(function, *dest, error_target));
            out
        }
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
            // resolved on *every* call, not cached. caching would early-bind the
            // name, and early binding is a tier-3 assumption gated on `api.lock` —
            // at the default tier a module global may be rebound, and python
            // would see it. the lookup is the same dict probe the interpreter does
            //
            // the slot below holds the interned *name*, which is what makes it
            // sound to keep: it is what the lookup is keyed on, never its answer
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
            let _ = writeln!(
                out,
                "      by_fn = By_LookupGlobal(by_module_dict, {slot});"
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
            let call = match name.as_str() {
                "append" => "By_ListAppend",
                _ => "By_CallMethod",
            };
            let _ = writeln!(
                out,
                "      PyObject *by_t = {call}({}, {slot}, by_argv, {});",
                value_expr(receiver),
                args.len()
            );
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
            let fields = receiver_fields(module, class, receiver);
            let expr = format!("{fields}->{}", mangle_member(field));
            // an attribute `__init__` assigns on only some paths may not be there, and
            // python answers a read of one with `AttributeError` rather than a value
            let mut out = String::new();
            if let Some(decl) = field_decl(module, class, field)
                && decl.optional
            {
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
            let fields = receiver_fields(module, class, receiver);
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
                _ => "By_GetItem",
            };
            let expr = format!("{call}({}, {})", value_expr(container), value_expr(index));
            assign_checked(module, function, *dest, &expr, error_target)
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
                "By_StrItemCompareChar({}, {}, {}, {})",
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
        Op::GetIter { dest, src } => {
            let expr = format!("By_GetIter({})", value_expr(src));
            assign_checked(module, function, *dest, &expr, error_target)
        }
        Op::IterNext { dest, iter } => {
            // a null result is exhaustion *or* failure, so the check has to
            // consult the exception state rather than the value alone
            let expr = format!("By_IterNext({})", value_expr(iter));
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
            // frame holds — including the register just retained
            let _ = writeln!(out, "    {{ {} by_ret = {expr};", ctype(module, &ty));
            if let Some(retain) = inc_ref(&ty, "by_ret") {
                let _ = writeln!(out, "      {retain}");
            }
            out.push_str(&emit_cleanup(function, "      ", live));
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
            let mut out = emit_cleanup(function, "    ", live);
            let _ = writeln!(out, "    return {};", function.ret.undefined());
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
        // definition holds the one object every such call has to share
        .chain(
            function
                .computed_defaults
                .iter()
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
        match default {
            Some(default) => {
                let position = index - usize::from(is_method);
                let _ = writeln!(out, "    if (by_bound[{position}] != NULL) {{");
                let _ = writeln!(out, "        {name} = {unbox};");
                let _ = writeln!(out, "    }} else {{");
                let _ = writeln!(out, "        {name} = {};", default_expr(&decl.ty, default));
                let _ = writeln!(out, "    }}");
            }
            // a parameter with no default is guaranteed present: `By_BindArgs` reports
            // every missing one, in cpython's own wording
            None => {
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
    if !function.deferring.is_empty() || !function.computed_defaults.is_empty() {
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
    twins: usize,
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
    // the constants a metaclass construction writes into the namespace, and reads back off
    // the class to see whether it agreed. a class no interpreted `class` statement wrote
    // has no captured body to take a value off, and there is nothing to carry
    let (declare, constants) = match slot.filter(|_| !class.constants.is_empty()) {
        None => (String::new(), "NULL".to_string()),
        Some(slot) => {
            let names = class
                .constants
                .iter()
                .map(|name| c_string(name))
                .collect::<Vec<_>>()
                .join(", ");
            (
                format!(
                    "\x20     static const char *const by_constants[] = {{{names}}};\n\
                     \x20     By_ClassConstants by_carried = {{by_body[{slot}], by_constants, {}, by_twin, by_type, {twins}}};\n",
                    class.constants.len()
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

    out.push_str("static PyMethodDef by_methods[] = {\n");
    for function in &module.functions {
        if function.exported {
            let _ = writeln!(
                out,
                "    {{\"{}\", (PyCFunction)(void(*)(void)){}, METH_FASTCALL | METH_KEYWORDS, NULL}},",
                function.name,
                function.wrapper_symbol(module.name.dotted())
            );
        }
    }
    out.push_str("    {NULL, NULL, 0, NULL}\n};\n\n");

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
        .any(|class| appends_storage_from_a_spec(module, class))
    {
        conditions.push("!BY_HAS_TYPE_DATA");
    }
    // a class keeping an instance dict is the same question again: below 3.13 there is
    // no published way to walk or release a managed one, and a collected type that
    // cannot walk what it holds is worse than no compiled type at all
    if module
        .classes
        .iter()
        .any(|class| instance_dict(module, class))
    {
        conditions.push("!BY_HAS_MANAGED_DICT");
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
    let mut layout_guard = String::new();
    if !conditions.is_empty() {
        // the twin's source no longer carries the decorators init applies, so leaving
        // the module interpreted means applying them here — to the twin's own
        // definitions, which is where python would have run them
        let _ = write!(
            layout_guard,
            "    if ({}) {{\n{}{release_bodies}    return 0;\n    }}\n",
            conditions.join(" || "),
            twin_decorators(module)
        );
    }
    for class in &module.classes {
        if appends_storage_from_a_spec(module, class) {
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
            // giving up here leaves every interpreted definition standing, and their
            // decorators have been taken out of the source that built them — so this
            // exit has to run them for the same reason the guard above does. a module
            // with nothing to run keeps the plain one-line refusal
            let unwind = format!("{}{release_bodies}", twin_decorators(module));
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
        let _ = writeln!(
            twin_init,
            "    PyObject *by_twin[{count}];\n\
             \x20   PyObject *by_type[{count}] = {{NULL}};"
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
            "    if (By_AdoptTwinAttributes(by_twin, by_type, {count}) < 0) return -1;"
        );
        let names = twins
            .iter()
            .map(|class| c_string(&class.name))
            .collect::<Vec<_>>()
            .join(", ");
        let _ = writeln!(
            twin_remap,
            "    {{ static const char *const by_name[] = {{{names}}};\n\
             \x20     int by_remapped = By_RemapTwinAliases(dict, by_twin, by_name, {count});\n\
             \x20     for (Py_ssize_t by_at = 0; by_at < {count}; by_at++) Py_XDECREF(by_twin[by_at]);\n\
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
        let ready = if appends_storage_from_a_spec(module, class) {
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
                    twins.len()
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
                external_construction(module, class, &type_name, &pack, slot, twins.len())
            )
        } else {
            format!(
                "    {type_name} = PyType_FromSpec(&{type_name}_spec);\n\
                 \x20   if ({type_name} == NULL) return -1;"
            )
        };
        if !ready.is_empty() {
            let _ = writeln!(class_init, "{ready}");
        }
        // the type exists from here, so it is what stands for this class's twin in every
        // remap below. a class built in the layout guard was built before the array even
        // existed, which is why this is not written where the construction is
        if let Some(slot) = slot {
            let _ = writeln!(class_init, "    by_type[{slot}] = {type_name}_OBJ;");
        }
        // the awaitable `__anext__` hands back is a type of its own, and an unreadied
        // type has no `tp_free` — `PyObject_New` on one segfaults rather than failing
        if class
            .resume
            .as_ref()
            .is_some_and(|resume| resume.surface == Surface::AsyncGenerator)
        {
            let _ = writeln!(
                class_init,
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
                class_init,
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
                class_init,
                "    if (By_CopyClassConstant(by_body[{slot}], (PyTypeObject *){type_name}_OBJ, {}, by_twin, by_type, {}) < 0) return -1;",
                c_string(constant),
                twins.len()
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
                class_init,
                "    {{ static const char *const by_decorators[] = {{{names}}};\n\
                 \x20     if (By_DecoratedMethod({body}, (PyTypeObject *){type_name}_OBJ, dict, {}, {}, by_decorators, {}, by_twin, by_type, {}) < 0) return -1; }}",
                c_string(&class.name),
                c_string(&method.name),
                method.decorators.len(),
                twins.len()
            );
        }
        // a closure environment is a real type with a real layout, and nothing
        // should be able to name it
        if class.exported {
            let _ = writeln!(
                class_init,
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
         \x20       if (PyDict_GetItemString(dict, \"__builtins__\") == NULL &&\n\
         \x20           PyDict_SetItemString(dict, \"__builtins__\", PyEval_GetBuiltins()) < 0) {\n\
         \x20           return -1;\n\
         \x20       }\n\
         \x20       PyObject *result = By_ExecModuleBody(&by_fallback, dict);\n\
         \x20       if (result == NULL) return -1;\n\
         \x20       Py_DECREF(result);\n\
         \x20   }\n"
            .to_string()
    };
    let _ = write!(
        out,
        "static int by_exec(PyObject *module) {{\n\
         \x20   PyObject *dict = PyModule_GetDict(module);\n\
         \x20   if (dict == NULL) return -1;\n\
         \x20   by_module_dict = dict;\n\
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
         \x20   if (PyModule_AddFunctions(module, by_methods) < 0) return -1;\n\
         {decorators}\
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
        let mut module = module_with(add());
        module.classes.push(ClassIr {
            name: "Wrapped".to_string(),
            immutable: false,
            exported: true,
            base: Some(ClassBase::External(vec!["Exception".to_string()])),
            inherited_init: false,
            generic: false,
            constants: Vec::new(),
            fields: vec![by_ir::function::FieldDecl {
                name: "tag".to_string(),
                ty: RType::OBJECT,
                default: None,
                optional: false,
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
        let mut module = module_with(add());
        module.classes.push(appending_class());
        let mut inner = appending_class();
        inner.name = "Deeper".to_string();
        inner.base = Some(ClassBase::InModule("Wrapped".to_string()));
        inner.fields[0].name = "depth".to_string();
        module.classes.push(inner);
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

    /// the other layout model for an in-module base: the subclass restates its base's
    /// fields so that a pointer to one is a pointer to the other. an appended region
    /// would give the pair two copies of each, so such a class is not appended over
    /// anything and keeps the construction that answers from reality — which refuses
    #[test]
    fn a_subclass_restating_its_bases_fields_is_not_appended_over_it() {
        let mut module = module_with(add());
        module.classes.push(appending_class());
        let mut inner = appending_class();
        inner.name = "Deeper".to_string();
        inner.base = Some(ClassBase::InModule("Wrapped".to_string()));
        inner.fields.push(by_ir::function::FieldDecl {
            name: "depth".to_string(),
            ty: RType::OBJECT,
            default: None,
            optional: false,
        });
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

    /// a base that lays nothing out of its own is built by calling its metaclass, so its
    /// type is a `class` statement's after all — `subtype_dealloc` and the recursion
    /// that comes with it. only a base built from a spec here can be chained to
    #[test]
    fn a_base_this_module_does_not_build_from_a_spec_is_not_one_to_stand_on() {
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

        assert!(!c.contains("By_SpecSubclass"), "{c}");
        assert!(
            c.contains(
                "By_app_Deeper_Type = By_SpecClass(dict, \"Deeper\", &By_app_Deeper_Type_spec);"
            ),
            "{c}"
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
            constants: Vec::new(),
            fields: vec![by_ir::function::FieldDecl {
                name: "tag".to_string(),
                ty: RType::OBJECT,
                default: None,
                optional: false,
            }],
            decorators: Vec::new(),
            methods: Vec::new(),
            resume: None,
            keywords: Vec::new(),
        }
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
        // the return retains the result, and the frame releases its own
        // temporary — but not the parameters, which the caller still owns.
        // releasing them here *and* in the wrapper was a double-decref that only
        // survived because small ints are unrefcounted
        assert!(c.contains("By_IncRefTagged(by_ret);"), "{c}");
        assert!(c.contains("By_DecRefTagged(r2);"), "{c}");
        let body = c
            .split("static ByTagged by_app_add(ByTagged r0, ByTagged r1) {")
            .nth(1)
            .and_then(|rest| rest.split("static PyObject").next())
            .expect("the body is emitted");
        assert!(!body.contains("By_DecRefTagged(r0);"), "{body}");
        assert!(!body.contains("By_DecRefTagged(r1);"), "{body}");
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
            constants: Vec::new(),
            fields: vec![by_ir::function::FieldDecl {
                name: "x".to_string(),
                ty: RType::INT,
                default: None,
                optional: false,
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
            constants: Vec::new(),
            fields: vec![by_ir::function::FieldDecl {
                name: "value".to_string(),
                ty: RType::OBJECT,
                default: None,
                optional: false,
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
            constants: Vec::new(),
            fields: ["a", "rest", "extra"]
                .into_iter()
                .map(|name| by_ir::function::FieldDecl {
                    name: name.to_string(),
                    ty: RType::OBJECT,
                    default: None,
                    optional: false,
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
}
