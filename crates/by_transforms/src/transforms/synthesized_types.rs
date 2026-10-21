//! Names the class each type expression that lowers to one stands for — a typed dict
//! literal, an anonymous named tuple type, an inline protocol — in one walk over the module.
//!
//! The three nest in one another every way round, `(p: {"a": int})`, `{"a": (p: int)}`,
//! `protocol(x: (p: int))`, and the class an outer one lowers to declares the classes of the
//! ones inside it as the types of its fields. So the walk names a type only once it has named
//! every type inside it, prints each class body through one type-expression lowerer that writes
//! every name given so far in place of its type, and declares the classes in the order they were
//! named — each after every class its body reads, which a `NamedTuple`, whose field annotations
//! are evaluated as the class is made, depends on.
//!
//! The passes that lower each kind replace a type with the name given here, and every other
//! lowering that prints a type expression whole is handed the names as substitutions
//! ([`PassContext::type_substitutions`](super::ast_driver::PassContext::type_substitutions)),
//! so a synthesized type inside a callable arrow, a tuple type or an `__init__` annotation is
//! written as its class wherever it stands.

use std::collections::{BTreeSet, HashMap};

use indexmap::IndexMap;
use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{Expr, Stmt};
use ruff_text_size::{Ranged, TextRange};
use ty_python_semantic::AnonNamedTupleId;

use super::anon_named_tuple;
use super::ast_driver::Lowering;
use super::callable::CallableSyntax;
use super::generics::PolyfilledTypeParams;
use super::protocol_type;
use super::quotes::WrittenQuotes;
use super::repeated_underscore::WrittenNames;
use super::type_expr_walker::{
    Recurse, TypeExprVisitor, TypePos, walk_one_type_expr, walk_type_positions_skipping,
};
use super::typed_dict_literal;
use crate::config::Config;
use crate::type_info::TypeInfo;

/// the lowerings a pass writes itself when it replaces a type with a class whose body the namer
/// printed: every lowering the shared type-expression lowerer writes, a `typeof` among them, and
/// each synthesized type nested inside, which is written as its class
pub(crate) const CLASS_BODY: &[Lowering] = &[
    Lowering::Callable,
    Lowering::OptionalType,
    Lowering::LiteralType,
    Lowering::JustFloat,
    Lowering::DynamicKeyword,
    Lowering::FloatConst,
    Lowering::SymbolicTypeOp,
    Lowering::Typeof,
    Lowering::AnonNamedTuple,
    Lowering::ProtocolType,
    Lowering::TypedDictLiteral,
];

/// which kind of class a synthesized type lowers to
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SynthesizedKind {
    TypedDict,
    NamedTuple,
    Protocol,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum ClassShape {
    TypedDict(typed_dict_literal::Shape),
    NamedTuple(anon_named_tuple::Shape),
    Protocol(protocol_type::Shape),
}

/// Every synthesized type of a module, the class each names, and what declaring the classes takes.
#[derive(Default)]
pub(crate) struct SynthesizedTypes {
    /// each synthesized type's source range, its kind and the class it names, in naming order
    nodes: Vec<(TextRange, SynthesizedKind, String)>,
    /// every anonymous named tuple class, by shape. the named tuple lowering reuses a class
    /// for a value form, or a plain tuple it constructs, of the same shape
    named_tuples: IndexMap<anon_named_tuple::Shape, String>,
    /// the class of each anonymous named tuple the checker knows, by its identity there, which
    /// a plain tuple literal the checker reads as one is constructed as
    named_tuple_ids: Vec<(AnonNamedTupleId, String)>,
    /// the class declarations, each after every class it reads
    pub(crate) declarations: String,
    /// the import lines the declarations read
    pub(crate) imports: Vec<String>,
    /// the runtime helpers they call
    pub(crate) helpers: Vec<crate::runtime::Helper>,
    /// why a type could not be declared
    pub(crate) errors: Vec<String>,
}

impl SynthesizedTypes {
    /// name every synthesized type in `stmts`, where `folds` are the operations `symbolic_type_op`
    /// folded, written in a class body as the type they fold to, and `claimed` their ranges
    #[expect(clippy::too_many_arguments)]
    pub(crate) fn name_all(
        source: &str,
        written: WrittenNames,
        quotes: &WrittenQuotes,
        types: &dyn TypeInfo,
        config: &Config,
        stmts: &[Stmt],
        folds: &[(TextRange, String)],
        claimed: &[TextRange],
    ) -> Self {
        let mut namer = Namer::new(source, written, quotes, types, claimed, config);
        for (range, rendered) in folds {
            namer.callable.add_substitution(*range, rendered.clone());
        }
        let polyfilled = PolyfilledTypeParams::decide(source, written, stmts, config);
        let mut scopes = TypeParameterScopes {
            polyfilled: &polyfilled,
            scopes: &mut namer.type_parameter_scopes,
        };
        for stmt in stmts {
            scopes.visit_stmt(stmt);
        }
        walk_type_positions_skipping(stmts, Some(types), claimed, &mut namer);
        // `protocol(...)` and `(name: T)` are no python wherever they appear, so the ones in a
        // value position, an alias `P = (name: str)` say, are named too
        let mut values = ValuePositions { namer: &mut namer };
        for stmt in stmts {
            values.visit_stmt(stmt);
        }
        namer.finish()
    }

    /// the source range of each synthesized type of `kind`, with the class it names
    pub(crate) fn of_kind(&self, kind: SynthesizedKind) -> Vec<(TextRange, String)> {
        self.nodes
            .iter()
            .filter(|(_, node_kind, _)| *node_kind == kind)
            .map(|(range, _, name)| (*range, name.clone()))
            .collect()
    }

    /// the source range of every synthesized type, with the class it names
    pub(crate) fn substitutions(&self) -> impl Iterator<Item = (TextRange, String)> + '_ {
        self.nodes
            .iter()
            .map(|(range, _, name)| (*range, name.clone()))
    }

    /// the class the synthesized type of `kind` written at exactly `range` names
    pub(crate) fn class_at(&self, range: TextRange, kind: SynthesizedKind) -> Option<&str> {
        self.nodes
            .iter()
            .find(|(candidate, node_kind, _)| *candidate == range && *node_kind == kind)
            .map(|(_, _, name)| name.as_str())
    }

    /// every anonymous named tuple class declared here, by shape
    pub(crate) fn named_tuples(&self) -> &IndexMap<anon_named_tuple::Shape, String> {
        &self.named_tuples
    }

    /// the class declared here for each anonymous named tuple the checker identifies
    /// ([`TypeInfo::anon_named_tuple_id`])
    pub(crate) fn named_tuple_identities(
        &self,
    ) -> impl Iterator<Item = (AnonNamedTupleId, String)> + '_ {
        self.named_tuple_ids.iter().cloned()
    }

    /// whether the module has no synthesized type at all
    pub(crate) fn is_empty(&self) -> bool {
        self.nodes.is_empty() && self.errors.is_empty()
    }
}

/// The walk in progress: what the kinds of synthesized type print their class bodies with, and
/// what has been named so far.
pub(crate) struct Namer<'src> {
    source: &'src str,
    types: &'src dyn TypeInfo,
    pub(crate) written: WrittenNames<'src>,
    /// where a class body's types are written as strings
    pub(crate) quotes: &'src WrittenQuotes,
    /// the one lowerer every class body is printed through, holding a substitution for each type
    /// named so far and for each fold
    pub(crate) callable: CallableSyntax<'src>,
    /// the names each type parameter of a generic is read by from module scope, where a class
    /// lands, keyed by the range of the statement declaring the generic: the polyfill's `_T`, or
    /// `A.__type_params__[0]` for a list kept native
    type_parameter_scopes: Vec<(TextRange, HashMap<String, String>)>,
    classes: IndexMap<ClassShape, String>,
    nodes: Vec<(TextRange, SynthesizedKind, String)>,
    named_tuple_ids: Vec<(AnonNamedTupleId, String)>,
    pub(crate) errors: Vec<String>,
    /// the `(module, name)` of each name a class body reads that the shared lowerer does not
    /// import itself: the `Literal` of a typed dict field's literal type, an `Intersection`
    pub(crate) imports: BTreeSet<(&'static str, &'static str)>,
    /// set when a class body spells a nested optional `T??` with the runtime `Optional`
    pub(crate) needs_optional_runtime: bool,
}

impl<'src> Namer<'src> {
    fn new(
        source: &'src str,
        written: WrittenNames<'src>,
        quotes: &'src WrittenQuotes,
        types: &'src dyn TypeInfo,
        claimed: &'src [TextRange],
        config: &Config,
    ) -> Self {
        Self {
            source,
            types,
            written,
            quotes,
            callable: CallableSyntax::new(source, written, quotes, config)
                .with_types(types)
                .with_claimed_ranges(claimed),
            type_parameter_scopes: Vec::new(),
            classes: IndexMap::new(),
            nodes: Vec::new(),
            named_tuple_ids: Vec::new(),
            errors: Vec::new(),
            imports: BTreeSet::new(),
            needs_optional_runtime: false,
        }
    }

    /// what the checker knows of the module
    pub(crate) fn types(&self) -> &'src dyn TypeInfo {
        self.types
    }

    pub(crate) fn src(&self, range: TextRange) -> &'src str {
        &self.source[usize::from(range.start())..usize::from(range.end())]
    }

    /// what the type written at exactly `range` is printed as, when it is already lowered: a
    /// class named before it, or a fold
    pub(crate) fn substitution_for(&self, range: TextRange) -> Option<String> {
        self.callable.substitution_for(range).map(str::to_owned)
    }

    /// `expr` printed as python, every type inside it named so far written as its class
    pub(crate) fn lower(&mut self, expr: &Expr) -> String {
        self.callable.lower_type_expr(expr)
    }

    /// `rendered`, the text of a type written at `range`, reading each type parameter of the
    /// generics around it by the name it has at module scope, where the class is declared
    pub(crate) fn read_type_parameters(&self, range: TextRange, rendered: &str) -> String {
        let mut renames: HashMap<&str, &str> = HashMap::new();
        for (scope, frame) in &self.type_parameter_scopes {
            if scope.contains_range(range) {
                renames.extend(frame.iter().map(|(k, v)| (k.as_str(), v.as_str())));
            }
        }
        if renames.is_empty() {
            return rendered.to_owned();
        }
        super::source_util::rename_identifiers(rendered, &renames)
    }

    fn is_named(&self, range: TextRange) -> bool {
        self.nodes.iter().any(|(candidate, ..)| *candidate == range)
    }

    /// name the type at `range` as the class declaring `shape`, or as `known`, a class named
    /// already for a type the checker reads as the same one
    fn name(
        &mut self,
        range: TextRange,
        kind: SynthesizedKind,
        shape: ClassShape,
        known: Option<String>,
    ) -> String {
        let name = if let Some(known) = known {
            known
        } else if let Some(existing) = self.classes.get(&shape) {
            existing.clone()
        } else {
            let name = match &shape {
                ClassShape::TypedDict(shape) => shape.class_name(self.written),
                ClassShape::NamedTuple(shape) => shape.class_name(self.written),
                ClassShape::Protocol(shape) => shape.class_name(self.written),
            };
            self.classes.insert(shape, name.clone());
            name
        };
        // an enclosing type's class body names this one wherever it sits in it — inside a
        // subscript, a union, a callable arrow — not only when it is a whole field type
        self.callable.add_substitution(range, name.clone());
        self.nodes.push((range, kind, name.clone()));
        name
    }

    fn finish(mut self) -> SynthesizedTypes {
        let written = self.written;
        let mut declarations = self.callable.class_defs().to_owned();
        let mut kinds = BTreeSet::new();
        let mut named_tuples = IndexMap::new();
        for (shape, name) in &self.classes {
            let declaration = match shape {
                ClassShape::TypedDict(shape) => {
                    kinds.insert(("typing_extensions", "TypedDict"));
                    shape.class_def(
                        name,
                        &written.imported("typing_extensions", "TypedDict"),
                        self.quotes,
                    )
                }
                ClassShape::NamedTuple(shape) => {
                    kinds.insert(("typing", "NamedTuple"));
                    named_tuples.insert(shape.clone(), name.clone());
                    shape.class_def(name, &written.imported("typing", "NamedTuple"))
                }
                ClassShape::Protocol(shape) => {
                    kinds.insert(("typing", "Protocol"));
                    shape.class_def(name, &written.imported("typing", "Protocol"))
                }
            };
            declarations.push_str(&declaration);
            declarations.push('\n');
        }
        let (mut imports, mut helpers) = self.callable.take_requirements();
        imports.extend(
            kinds
                .into_iter()
                .chain(self.imports.iter().copied())
                .map(|(module, name)| written.import_from(module, &[name])),
        );
        if self.needs_optional_runtime {
            helpers.push(crate::runtime::OPTIONAL);
        }
        SynthesizedTypes {
            nodes: self.nodes,
            named_tuples,
            named_tuple_ids: self.named_tuple_ids,
            declarations,
            imports,
            helpers,
            errors: self.errors,
        }
    }
}

impl TypeExprVisitor for Namer<'_> {
    fn visit(&mut self, expr: &Expr, _pos: TypePos) -> Recurse {
        // a type named already, reached a second time from a value position, keeps its name
        if self.is_named(expr.range()) {
            return Recurse::Stop;
        }
        Recurse::Descend
    }

    /// every type inside a synthesized one has to be named before it, so the walk goes on into
    /// them, where the passes that print a type whole stop
    fn descends_into_wholly_lowered(&self) -> bool {
        true
    }

    fn leave(&mut self, expr: &Expr) {
        if self.is_named(expr.range()) {
            return;
        }
        match expr {
            Expr::Dict(dict) => {
                if let Some(shape) = typed_dict_literal::shape(self, dict) {
                    self.name(
                        dict.range(),
                        SynthesizedKind::TypedDict,
                        ClassShape::TypedDict(shape),
                        None,
                    );
                }
            }
            Expr::Tuple(tuple) if tuple.is_anon_named_tuple => {
                match anon_named_tuple::type_shape(self, tuple) {
                    Ok(Some(shape)) => {
                        // the checker reads two spellings of one type, `(a: List[int])` and
                        // `(a: list[int])`, as one class, which both are written as
                        let id = self.types.anon_named_tuple_id(tuple);
                        let known = id.and_then(|id| {
                            self.named_tuple_ids
                                .iter()
                                .find(|(candidate, _)| *candidate == id)
                                .map(|(_, name)| name.clone())
                        });
                        let is_known = known.is_some();
                        let name = self.name(
                            tuple.range(),
                            SynthesizedKind::NamedTuple,
                            ClassShape::NamedTuple(shape),
                            known,
                        );
                        if let Some(id) = id
                            && !is_known
                        {
                            self.named_tuple_ids.push((id, name));
                        }
                    }
                    Ok(None) => {}
                    Err(message) => self.errors.push(message),
                }
            }
            Expr::ProtocolType(protocol) => {
                let shape = protocol_type::shape(self, protocol);
                self.name(
                    protocol.range(),
                    SynthesizedKind::Protocol,
                    ClassShape::Protocol(shape),
                    None,
                );
            }
            _ => {}
        }
    }
}

/// Finds the inline protocols and anonymous named tuple types standing where a value is written,
/// each named as the type it is.
struct ValuePositions<'a, 'src> {
    namer: &'a mut Namer<'src>,
}

impl<'ast> Visitor<'ast> for ValuePositions<'_, '_> {
    fn visit_expr(&mut self, expr: &'ast Expr) {
        let is_type = match expr {
            Expr::ProtocolType(_) => true,
            Expr::Tuple(tuple) => tuple.is_anon_named_tuple,
            _ => false,
        };
        if is_type {
            walk_one_type_expr(expr, self.namer);
            return;
        }
        walk_expr(self, expr);
    }
}

/// Collects the names each generic's type parameters are read by from module scope, keyed by the
/// range of the statement declaring the generic.
struct TypeParameterScopes<'a> {
    polyfilled: &'a PolyfilledTypeParams,
    scopes: &'a mut Vec<(TextRange, HashMap<String, String>)>,
}

impl TypeParameterScopes<'_> {
    fn record(&mut self, range: TextRange, type_params: Option<&ruff_python_ast::TypeParams>) {
        if let Some(frame) = type_params.and_then(|tp| self.polyfilled.hoisted_renames(tp))
            && !frame.is_empty()
        {
            self.scopes.push((range, frame));
        }
    }
}

impl<'ast> Visitor<'ast> for TypeParameterScopes<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        match stmt {
            Stmt::ClassDef(class) => self.record(class.range(), class.type_params.as_deref()),
            Stmt::FunctionDef(function) => {
                self.record(function.range(), function.type_params.as_deref());
            }
            Stmt::TypeAlias(alias) => self.record(alias.range(), alias.type_params.as_deref()),
            _ => {}
        }
        walk_stmt(self, stmt);
    }
}
