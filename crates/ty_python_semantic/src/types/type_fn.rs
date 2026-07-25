//! basedpython: `type def` type functions — proof of concept
//!
//! A `type def` is a function from types to a type, applied with `[]` in a type
//! expression and evaluated by *executing its body* in a real python
//! interpreter:
//!
//! ```by
//! type def F[X]:
//!     if X <= int:
//!         return int
//!     return str
//!
//! def f() -> F[bool]     # int
//! ```
//!
//! This module is deliberately the smallest thing that runs end to end. Compared
//! with the design in `docs/basedpython/development/type-def-design.md` it is
//! missing, on purpose:
//!
//! - **deferral**: an application whose arguments are not fully known evaluates
//!   to `Unknown` instead of staying symbolic until specialization
//! - **the oracle**: the argument is described *eagerly* into a self-contained
//!   python object (name, mro, literal value, union members) rather than being a
//!   proxy that queries ty back. so `X <= int` is answered from the shipped mro,
//!   which is nominal-only: structural and protocol relations are invisible
//! - **caching**: every application spawns an interpreter, every time. there is
//!   no memo, let alone the observation-trace decision tree
//! - **the worker pool, trust config, timeouts, and the transpiler lowering**
//!
//! What it *is* enough for is writing type functions and seeing what they infer.

use std::fmt::Write;
use std::process::{Command, Stdio};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use ruff_db::files::File;
use ruff_db::parsed::{ParsedModuleRef, parsed_module};
use ruff_db::source::source_text;
use ruff_text_size::Ranged;

use crate::Db;
use crate::place::{builtins_symbol, imported_symbol};
use crate::types::function::FunctionType;
use crate::types::literal::LiteralValueTypeKind;
use crate::types::tuple::TupleType;
use crate::types::{ClassBase, KnownClass, Type, UnionType};
use ty_module_resolver::{ModuleName, SearchPath, file_to_module, resolve_module_confident};

/// The result of applying a `type def`.
#[derive(Clone, Debug, PartialEq, Eq, get_size2::GetSize, salsa::SalsaValue)]
pub(crate) enum TypeFnOutcome<'db> {
    /// The body returned a type.
    Type(Type<'db>),
    /// The body returned `TypeError(...)`; the string is the author's message.
    TypeError(String),
    /// The application could not be evaluated. The string is a diagnostic-worthy
    /// explanation (a python traceback, an unusable argument, a missing
    /// interpreter).
    Failed(String),
}

/// The arguments of one type-function application, interned so the evaluation can
/// be a Salsa query.
#[salsa::interned(debug, heap_size = ruff_memory_usage::heap_size)]
pub(crate) struct TypeFnArguments<'db> {
    #[returns(deref)]
    pub(crate) arguments: Box<[Type<'db>]>,
}

// The Salsa heap is tracked separately.
impl get_size2::GetSize for TypeFnArguments<'_> {}

/// Applies `function` — a `type def` — to `arguments`.
///
/// This is a Salsa query, so an application is evaluated once per revision
/// rather than once per occurrence. Because [`describe_type`] reads the
/// arguments' class structure *inside* the query, editing a class a type
/// function looked at invalidates the result the ordinary way — the eager
/// description is what buys correct incrementality here, where the design's
/// lazy oracle needs observation traces for the same guarantee.
#[salsa::tracked(returns(ref), heap_size = ruff_memory_usage::heap_size)]
pub(crate) fn evaluate_type_fn<'db>(
    db: &'db dyn Db,
    function: FunctionType<'db>,
    arguments: TypeFnArguments<'db>,
) -> TypeFnOutcome<'db> {
    let arguments = arguments.arguments(db);
    let file = function.file(db);

    if let Err(refusal) = execution_is_permitted(db, file) {
        return TypeFnOutcome::Failed(refusal);
    }

    let module = parsed_module(db, file).load(db);

    let Some(source) = type_fn_python_source(db, function, file, &module) else {
        return TypeFnOutcome::Failed("`type def` has no body to execute".to_string());
    };

    let mut argument_json = String::from("[");
    for (i, argument) in arguments.iter().enumerate() {
        if i > 0 {
            argument_json.push(',');
        }
        let Some(described) = describe_type(db, *argument, Some(i), 0) else {
            return TypeFnOutcome::Failed(format!(
                "cannot describe `{}` to a type function; the proof of concept \
                 only handles class instances, literals, unions and `None`",
                argument.display(db)
            ));
        };
        argument_json.push_str(&described);
    }
    argument_json.push(']');

    let script = format!("{PRELUDE}\n{source}\n{}", driver(&argument_json));

    match run_python(&script) {
        Err(error) => TypeFnOutcome::Failed(error),
        Ok(output) => interpret_result(db, arguments, &output),
    }
}

/// Whether a type function defined in `file` may be executed.
///
/// Executing a type function runs arbitrary code with the checker's authority, so
/// merely *checking* a project must not run code that came from somewhere else.
/// Only first-party files — the ones the author is already responsible for — are
/// executed; anything from site-packages, a vendored stub, or the standard library
/// is refused and degrades to the declared return type.
///
/// `BY_NO_TYPE_FUNCTIONS=1` disables execution entirely, for checking untrusted
/// code (CI on a pull request, a web playground) where even first-party code
/// should not run.
fn execution_is_permitted(db: &dyn Db, file: File) -> Result<(), String> {
    if std::env::var_os("BY_NO_TYPE_FUNCTIONS").is_some() {
        return Err(
            "type functions are disabled (`BY_NO_TYPE_FUNCTIONS` is set), so this \
             application uses the declared return type"
                .to_string(),
        );
    }

    let is_first_party = file_to_module(db, file)
        .and_then(|module| module.search_path(db).map(SearchPath::is_first_party))
        .unwrap_or(false);

    if is_first_party {
        Ok(())
    } else {
        Err(
            "this type function is not first-party, and a type function is only \
             executed when it belongs to the project being checked"
                .to_string(),
        )
    }
}

/// The declared return type of a `type def`, i.e. the annotation in
/// `type def F[X] -> int | str:`.
///
/// This is the type an *unreduced* application behaves as, so it is the only
/// thing that makes `F[T]` usable inside generic code. Returns `None` when the
/// type function is unannotated.
pub(crate) fn declared_return_type<'db>(
    db: &'db dyn Db,
    function: FunctionType<'db>,
) -> Option<Type<'db>> {
    let returns = function.signature(db).overloads.first()?.return_ty;
    // an unannotated `type def` has no declared return; ty models a missing
    // annotation as `Unknown`, which is exactly the "no bound" case
    if returns.is_unknown() {
        None
    } else {
        Some(returns)
    }
}

/// Checks that an application passes as many arguments as the type function
/// declares. Without this an arity mistake reaches the interpreter and surfaces as
/// a python traceback instead of a diagnostic.
pub(crate) fn arity_mismatch<'db>(
    db: &'db dyn Db,
    function: FunctionType<'db>,
    arguments: &[Type<'db>],
) -> Option<(usize, usize)> {
    let generic_context = function.signature(db).overloads.first()?.generic_context?;
    let expected = generic_context.variables(db).count();
    (expected != arguments.len()).then_some((expected, arguments.len()))
}

/// Checks a type function's type-parameter bounds against the arguments of one
/// application, before anything is executed.
///
/// A bound is the cheapest diagnostic a type function can have: no interpreter is
/// started and no cache entry is created for an argument that could never be
/// valid. Returns the offending `(index, argument, bound)` for the first argument
/// outside its bound.
pub(crate) fn first_bound_violation<'db>(
    db: &'db dyn Db,
    function: FunctionType<'db>,
    arguments: &[Type<'db>],
) -> Option<(usize, Type<'db>, Type<'db>)> {
    let generic_context = function.signature(db).overloads.first()?.generic_context?;
    for (index, (typevar, argument)) in generic_context
        .variables(db)
        .zip(arguments.iter())
        .enumerate()
    {
        let Some(bound) = typevar.typevar(db).upper_bound(db) else {
            continue;
        };
        // the argument reads as a type expression, so compare the *instance* it
        // denotes against the bound's instance — `F[bool]` under `X: int` asks
        // whether `bool` is assignable to `int`
        if !argument.is_assignable_to(db, bound) {
            return Some((index, *argument, bound));
        }
    }
    None
}

/// Rebuilds the `type def` as an ordinary python function.
///
/// The body is taken verbatim from the source — including its indentation, so a
/// fresh `def` header can simply be prefixed — and the type parameters become
/// the parameters.
fn type_fn_python_source<'db>(
    db: &'db dyn Db,
    function: FunctionType<'db>,
    file: File,
    module: &ParsedModuleRef,
) -> Option<String> {
    let node = function.node(db, file, module);
    let text = source_text(db, file);

    let first = node.body.first()?;
    let last = node.body.last()?;

    // normally the body starts on its own line, and extending to the start of that
    // line keeps its original indentation so a fresh header can be prefixed. but a
    // one-line `type def F[X]: return int` has its body on the *header's* line, and
    // extending there would splice the `type def` line itself into the output — so
    // that case is emitted inline after the synthesized header instead
    let line_start = text[..usize::from(first.range().start())]
        .rfind('\n')
        .map_or(0, |newline| newline + 1);
    let body_is_on_its_own_line = line_start > usize::from(node.range().start());
    let body = if body_is_on_its_own_line {
        format!("\n{}", &text[line_start..usize::from(last.range().end())])
    } else {
        format!(
            " {}",
            &text[usize::from(first.range().start())..usize::from(last.range().end())]
        )
    };

    let parameters = node.type_params.as_ref().map(|type_params| {
        type_params
            .iter()
            .map(|type_param| type_param.name().id.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    })?;

    Some(format!("def __by_type_fn__({parameters}):{body}\n"))
}

/// Describes a type as a self-contained JSON object for the python side.
///
/// This is the proof-of-concept stand-in for the design's lazy `TypeInfo` proxy:
/// everything the body might ask is shipped up front, which is why only the
/// nominal relations are answerable.
fn describe_type<'db>(
    db: &'db dyn Db,
    ty: Type<'db>,
    handle: Option<usize>,
    depth: u32,
) -> Option<String> {
    // a pathologically nested union must not blow the stack while being described
    if depth > MAX_DESCRIPTION_DEPTH {
        return None;
    }
    let handle_field = match handle {
        Some(index) => index.to_string(),
        None => "null".to_string(),
    };
    let instance = match ty {
        Type::NominalInstance(_) | Type::LiteralValue(_) => ty,
        Type::ClassLiteral(class) => {
            // `F[int]` passes the *instance* type, matching how `int` reads in a
            // type expression
            Type::instance(db, class.default_specialization(db))
        }
        Type::Union(union) => {
            let mut members = String::from("[");
            for (i, member) in union.elements(db).iter().enumerate() {
                if i > 0 {
                    members.push(',');
                }
                members.push_str(&describe_type(db, *member, None, depth + 1)?);
            }
            members.push(']');
            return Some(format!(
                r#"{{"kind":"union","handle":{handle_field},"name":null,"qualname":null,"mro":[],"literal":null,"members":{members}}}"#
            ));
        }
        _ => return None,
    };

    let literal = match instance {
        Type::LiteralValue(literal) => match literal.kind() {
            LiteralValueTypeKind::Bool(value) => Some(value.to_string()),
            LiteralValueTypeKind::Int(value) => Some(value.as_i64().to_string()),
            LiteralValueTypeKind::String(value) => Some(json_string(value.value(db))),
            _ => None,
        },
        _ => None,
    };

    // a literal is described by the class it falls back to (`Literal[9]` → `int`)
    // plus its value, so `X <= int` and `X.literal` both work
    let instance_class = match instance {
        Type::NominalInstance(nominal) => nominal.class(db),
        other => match other.literal_fallback_instance(db)? {
            Type::NominalInstance(nominal) => nominal.class(db),
            _ => return None,
        },
    };

    let mro: String = instance_class
        .iter_mro(db)
        .filter_map(|base| match base {
            ClassBase::Class(class) => Some(json_string(class.name(db).as_ref())),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(",");
    let mro = format!("[{mro}]");

    let name = instance_class.name(db).to_string();
    Some(format!(
        r#"{{"kind":"instance","handle":{handle_field},"name":{},"qualname":{},"mro":{mro},"literal":{},"members":[]}}"#,
        json_string(&name),
        json_string(&name),
        literal.unwrap_or_else(|| "null".to_string()),
    ))
}

/// How deep a nested union may be before the description gives up.
const MAX_DESCRIPTION_DEPTH: u32 = 32;

/// Marks the protocol line in the child's output, so anything the body itself
/// wrote cannot be mistaken for the result.
const RESULT_SENTINEL: &str = "\u{1}by-type-fn\u{1}";

/// Maps the python side's answer back to a type.
///
/// `arguments` is needed because a type function that returns one of its own
/// arguments answers with that argument's *handle*: round-tripping by name would
/// lose the specialization (`list[int]` would come back as bare `list`) and would
/// fail outright for anything that is not a builtin.
fn interpret_result<'db>(
    db: &'db dyn Db,
    arguments: &[Type<'db>],
    output: &str,
) -> TypeFnOutcome<'db> {
    // the body's stdout is redirected away from the protocol, but be defensive:
    // take the last sentinel-prefixed line rather than trusting all of stdout
    let Some(line) = output
        .lines()
        .rev()
        .find_map(|line| line.strip_prefix(RESULT_SENTINEL))
    else {
        return TypeFnOutcome::Failed(format!("type function produced no result: {output:?}"));
    };

    let (tag, payload) = line.split_once(' ').unwrap_or((line, ""));
    match tag {
        "TYPE" => match resolve_graph(db, arguments, payload) {
            Ok(ty) => TypeFnOutcome::Type(ty),
            Err(error) => {
                TypeFnOutcome::Failed(format!("type function returned an unusable type: {error}"))
            }
        },
        "ERROR" => TypeFnOutcome::TypeError(unescape(payload)),
        "CRASH" => TypeFnOutcome::Failed(unescape(payload)),
        _ => TypeFnOutcome::Failed(format!("type function produced no result: {output:?}")),
    }
}

/// Mirrors the escaping the python side applies to literal text and messages.
fn unescape(payload: &str) -> String {
    let mut out = String::with_capacity(payload.len());
    let mut chars = payload.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('e') => out.push('\u{1e}'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// Resolves the python side's answer — a flat graph of nodes, each referring to
/// earlier ones by index — back to a type.
///
/// A flat graph rather than a name is what lets a *composed* type form survive:
/// `list[X]` is a generic node whose argument is the handle of the application's
/// own argument, so the specialization is exact rather than reconstructed from a
/// spelling that would have lost it.
fn resolve_graph<'db>(
    db: &'db dyn Db,
    arguments: &[Type<'db>],
    encoded: &str,
) -> Result<Type<'db>, String> {
    let mut nodes: Vec<Type<'db>> = Vec::new();
    for node in encoded.split('\u{1e}') {
        let (kind, payload) = node.split_once(':').unwrap_or((node, ""));
        let resolved = match kind {
            // one of the application's own arguments, returned exactly as it came in
            "a" => payload
                .parse::<usize>()
                .ok()
                .and_then(|index| arguments.get(index).copied())
                .ok_or_else(|| format!("unknown argument handle `{payload}`"))?,
            // a class object, resolved through ty's module resolver
            "c" => resolve_qualified_name(db, payload)
                .ok_or_else(|| format!("`{payload}` does not name a type"))?,
            "i" => payload
                .parse::<i64>()
                .map(Type::int_literal)
                .map_err(|_| format!("`{payload}` is not an integer literal"))?,
            "s" => Type::string_literal(db, unescape(payload).as_str()),
            "b" => Type::bool_literal(payload == "true"),
            "y" => Type::bytes_literal(db, unescape(payload).as_bytes()),
            // `list[int]`: an origin plus arguments, both by node index
            "g" => {
                let mut parts = payload.split(',').map(|index| {
                    index
                        .parse::<usize>()
                        .ok()
                        .and_then(|index| nodes.get(index).copied())
                        .ok_or_else(|| "malformed generic form".to_string())
                });
                let origin = parts
                    .next()
                    .ok_or_else(|| "empty generic form".to_string())??;
                let arguments = parts.collect::<Result<Vec<_>, _>>()?;
                specialize(db, origin, &arguments)?
            }
            "u" => {
                let members = payload
                    .split(',')
                    .map(|index| {
                        index
                            .parse::<usize>()
                            .ok()
                            .and_then(|index| nodes.get(index).copied())
                            .ok_or_else(|| "malformed union".to_string())
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                UnionType::from_elements(db, members)
            }
            other => return Err(format!("unknown type form `{other}`")),
        };
        nodes.push(resolved);
    }
    nodes
        .pop()
        .ok_or_else(|| "type function returned nothing".to_string())
}

/// Applies type arguments to a generic origin, as a subscript in a type
/// expression would.
fn specialize<'db>(
    db: &'db dyn Db,
    origin: Type<'db>,
    arguments: &[Type<'db>],
) -> Result<Type<'db>, String> {
    // a node resolves to the *instance* a bare name denotes in a type expression
    // (`list` → `list[Unknown]`), so specializing one means going back to the class
    // it came from and applying the arguments to that
    let class_literal = match origin {
        Type::NominalInstance(nominal) => nominal.class(db).class_literal(db),
        Type::ClassLiteral(class) => class,
        Type::GenericAlias(alias) => alias.origin(db).into(),
        _ => {
            return Err(format!(
                "`{}` cannot take type arguments",
                origin.display(db)
            ));
        }
    };

    // a tuple is not specialized by its generic context — its arguments are the
    // element types
    if class_literal.is_known(db, KnownClass::Tuple) {
        return Ok(Type::tuple(TupleType::heterogeneous(
            db,
            arguments.iter().copied(),
        )));
    }

    let specialized = class_literal.apply_specialization(db, |generic_context| {
        generic_context.specialize_partial(db, arguments.iter().copied().map(Some))
    });
    Ok(Type::instance(db, specialized))
}

/// Resolves a dotted `module.Class` reference (or a bare builtin) to its instance
/// type, through ty's module resolver.
fn resolve_qualified_name<'db>(db: &'db dyn Db, qualname: &str) -> Option<Type<'db>> {
    let qualname = qualname.trim();
    if matches!(qualname, "None" | "NoneType" | "builtins.NoneType") {
        return Some(Type::none(db));
    }

    let place = match qualname.rsplit_once('.') {
        None => builtins_symbol(db, qualname).place,
        Some(("builtins", name)) => builtins_symbol(db, name).place,
        Some((module, name)) => {
            let module_name = ModuleName::new(module)?;
            let module = resolve_module_confident(db, &module_name)?;
            imported_symbol(db, Some(module.file(db)?), name, None).place
        }
    };
    match place.ignore_possibly_undefined() {
        Some(Type::ClassLiteral(class)) => {
            Some(Type::instance(db, class.default_specialization(db)))
        }
        _ => None,
    }
}

/// How long a single type function may run before it is killed.
///
/// The checker waits while a type function runs, so an accidental `while True`
/// must not hang the check — or an editor keystroke — forever.
const TIMEOUT: Duration = Duration::from_secs(10);

/// At most this many type functions run at once.
///
/// ty checks files in parallel, so without a cap a project with many distinct
/// applications would fork one interpreter per rayon worker, each holding a
/// [`TIMEOUT`]-long slot.
const MAX_CONCURRENT: usize = 4;

static RUNNING: (Mutex<usize>, Condvar) = (Mutex::new(0), Condvar::new());

/// Blocks until a slot is free, and releases it on drop.
struct Slot;

impl Slot {
    fn acquire() -> Self {
        let (lock, condvar) = &RUNNING;
        let mut running = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while *running >= MAX_CONCURRENT {
            running = condvar
                .wait(running)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        *running += 1;
        Slot
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        let (lock, condvar) = &RUNNING;
        let mut running = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *running = running.saturating_sub(1);
        condvar.notify_one();
    }
}

fn run_python(script: &str) -> Result<String, String> {
    let _slot = Slot::acquire();
    let mut child = Command::new("python3")
        .arg("-c")
        .arg(script)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| {
            format!("could not start `python3` to evaluate a type function: {error}")
        })?;

    // `wait_timeout` is not in std, so poll. the interval is invisible next to a
    // python interpreter's startup cost
    let deadline = Instant::now() + TIMEOUT;
    while child
        .try_wait()
        .map_err(|error| format!("could not wait for `python3`: {error}"))?
        .is_none()
    {
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!(
                "the type function did not finish within {}s and was killed",
                TIMEOUT.as_secs()
            ));
        }
        std::thread::sleep(Duration::from_millis(5));
    }

    let output = child
        .wait_with_output()
        .map_err(|error| format!("could not read the type function's output: {error}"))?;

    if !output.status.success() {
        return Err(format!(
            "type function evaluation failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn json_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn driver(arguments_json: &str) -> String {
    format!(
        r#"
import contextlib as __contextlib, json as __json, sys as __sys, traceback as __traceback

__args = [_ByTypeInfo(__d) for __d in __json.loads({literal})]
__sentinel = {sentinel}
# the body's own stdout must not be mistaken for the protocol: anything it prints
# is redirected to stderr, which is surfaced as diagnostic detail instead
try:
    with __contextlib.redirect_stdout(__sys.stderr):
        __result = __by_type_fn__(*__args)
except BaseException:
    __out = "CRASH " + __traceback.format_exc().replace("\n", "\\n")
else:
    __out = _by_encode(__result)
__sys.stdout.write(__sentinel + __out + "\n")
"#,
        literal = python_string_literal(arguments_json),
        sentinel = python_string_literal(RESULT_SENTINEL),
    )
}

fn python_string_literal(value: &str) -> String {
    let mut out = String::from("'");
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\x{:02x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('\'');
    out
}

/// The python-side `TypeInfo` and result encoder.
const PRELUDE: &str = r#"
import types as _by_types, typing as _by_typing

_by_union_type = getattr(_by_types, "UnionType", None)

class _ByTypeInfo:
    def __init__(self, d):
        self.kind = d["kind"]
        self.handle = d["handle"]
        self.name = d["name"]
        self.qualname = d["qualname"]
        self.mro = tuple(d["mro"])
        self.literal = d["literal"]
        self.members = tuple(_ByTypeInfo(m) for m in d["members"])

    def __repr__(self):
        if self.kind == "union":
            return " | ".join(repr(m) for m in self.members)
        if self.literal is not None:
            return "Literal[%r]" % (self.literal,)
        return self.name

    def _names(self, other):
        if isinstance(other, _ByTypeInfo):
            return (other.name,)
        if isinstance(other, type):
            return (other.__name__,)
        raise TypeError("expected a type, got %r" % (other,))

    def is_subtype_of(self, other):
        names = self._names(other)
        if self.kind == "union":
            return all(m.is_subtype_of(other) for m in self.members)
        return any(name in self.mro for name in names)

    def is_equivalent_to(self, other):
        return self.name in self._names(other) and self.literal is None

    def __le__(self, other):
        return self.is_subtype_of(other)

    def __lt__(self, other):
        return self.is_subtype_of(other) and not self.is_equivalent_to(other)

    def __ge__(self, other):
        raise TypeError("a type is not comparable on the right of `<`; write it on the left")

    __gt__ = __ge__

    def __eq__(self, other):
        # must be total: `typing`'s `_type_check` does `arg in (Any, NoReturn, ...)`,
        # so raising here would make a proxy unusable with any typing constructor
        if not isinstance(other, (_ByTypeInfo, type)):
            return NotImplemented
        return self.is_equivalent_to(other)

    # defining `__eq__` without `__hash__` would make every argument unhashable,
    # so `X in {int, str}` would raise inside a body
    def __hash__(self):
        return hash((self.kind, self.name, self.literal))

    # `typing.Optional[X]` / `typing.Union[X, ...]` run `_type_check`, which only
    # accepts callables — without this a proxy cannot be composed with the typing
    # constructors at all
    def __call__(self):
        raise TypeError("a TypeInfo is not callable; it stands for a type")

    def __or__(self, other):
        return _ByUnion((self, other))

    __ror__ = __or__


class _ByUnion:
    def __init__(self, parts):
        self.parts = parts

    def __or__(self, other):
        return _ByUnion(self.parts + (other,))

    __ror__ = __or__


class _ByEncoder:
    # emits a flat graph: each node is `kind:payload`, nodes joined by \x1e, and a
    # composed form refers to earlier nodes by index. the last node is the result
    def __init__(self):
        self.nodes = []

    def emit(self, node):
        self.nodes.append(node)
        return len(self.nodes) - 1

    def escape(self, text):
        return text.replace("\\", "\\\\").replace("\x1e", "\\e").replace("\n", "\\n")

    def encode(self, value):
        # an argument comes back by handle, so its specialization survives exactly
        if isinstance(value, _ByTypeInfo):
            if value.handle is not None:
                return self.emit("a:%d" % (value.handle,))
            if value.kind == "union":
                return self.union(value.members)
            raise _ByUnusable(value)

        if isinstance(value, _ByUnion):
            return self.union(value.parts)

        if value is None:
            return self.emit("c:builtins.NoneType")

        # a bare value in a type position is its literal type, matching how `1`
        # reads as `Literal[1]` in ordinary basedpython source
        if isinstance(value, bool):
            return self.emit("b:%s" % ("true" if value else "false",))
        if isinstance(value, int):
            return self.emit("i:%d" % (value,))
        if isinstance(value, str):
            return self.emit("s:%s" % (self.escape(value),))
        if isinstance(value, bytes):
            return self.emit("y:%s" % (self.escape(value.decode("latin-1")),))

        origin = _by_typing.get_origin(value)
        if origin is not None:
            args = _by_typing.get_args(value)
            # `Literal[1, 2]` is a union of literal types, not a generic form
            if origin is _by_typing.Literal:
                return self.union(args)
            # `types.UnionType` (the `X | Y` form) only exists on 3.10+, and the
            # interpreter running a type function is whatever the project uses
            if origin is _by_typing.Union or (
                _by_union_type is not None and origin is _by_union_type
            ):
                return self.union(args)
            references = [self.encode(origin)]
            references.extend(self.encode(arg) for arg in args)
            return self.emit("g:%s" % (",".join(str(r) for r in references),))

        if isinstance(value, type):
            module = getattr(value, "__module__", None) or "builtins"
            return self.emit("c:%s.%s" % (module, value.__qualname__))

        raise _ByUnusable(value)

    def union(self, parts):
        references = [self.encode(part) for part in parts]
        return self.emit("u:%s" % (",".join(str(r) for r in references),))


class _ByUnusable(Exception):
    pass


def _by_encode(result):
    if isinstance(result, TypeError):
        return "ERROR " + str(result).replace("\n", "\\n")
    encoder = _ByEncoder()
    try:
        encoder.encode(result)
    except _ByUnusable as unusable:
        return "CRASH type function returned %r, which is not a type" % (unusable.args[0],)
    return "TYPE " + "\x1e".join(encoder.nodes)
"#;
