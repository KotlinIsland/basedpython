//! access sites of a member declared with a visibility keyword (basedpython)
//!
//! a visibility keyword renames the member it is written on: `private def
//! helper()` in a class body lowers to `def __helper()`, which python
//! name-mangles to `_A__helper` while class `A`'s body is executed, and
//! `protected def helper()` lowers to `def _helper()`. Either way the access
//! sites keep the name the source wrote, so they have to be pointed at the same
//! attribute:
//!
//! ```by
//! class A:
//!     private def helper(self) -> int:
//!         return 1
//!
//!     def use(self) -> int:
//!         return self.helper()
//! ```
//!
//! →
//!
//! ```python
//! class A:
//!     def __helper(self) -> int:
//!         return 1
//!
//!     def use(self) -> int:
//!         return self._A__helper()
//! ```
//!
//! the mangled name is written out rather than left to python: python mangles
//! lexically, so `self.__helper` would mean `_B__helper` in a subclass's body
//! and `__helper` outside a class altogether, while `_A__helper` names the same
//! attribute from every one of those places

use std::collections::HashSet;

use ruff_python_ast::helpers::{MemberVisibility, declaration_marker_visibility};
use ruff_python_ast::statement_visitor::{self, StatementVisitor};
use ruff_python_ast::visitor::{Visitor, walk_except_handler, walk_expr, walk_pattern, walk_stmt};
use ruff_python_ast::{self as ast, Expr, ExprContext, Stmt};
use ruff_text_size::{Ranged, TextRange, TextSize};

use super::ast_driver::{PassContext, TypeAwarePass};
use super::modifiers::module_private_name;
use super::source_util::header_end;
use crate::type_info::TypeInfo;

pub(crate) struct VisibilityRenamePass;

impl TypeAwarePass for VisibilityRenamePass {
    fn run(&self, stmts: &[Stmt], types: &dyn TypeInfo, ctx: &mut PassContext) {
        let module_private: HashSet<String> = types.private_module_symbols().into_iter().collect();
        let mut declarations = RenamedDeclarations::default();
        declarations.visit_body(stmts);
        let mut renamer = Renamer {
            types,
            edits: Vec::new(),
            header_end: TextSize::new(0),
            module_private,
            declarations: declarations.targets,
            scope: LexicalScope::Module,
            globals: HashSet::new(),
        };
        for stmt in stmts {
            renamer.visit_stmt(stmt);
        }
        ctx.text_edits.extend(renamer.edits);
    }
}

/// the names a declaration's own lowering renames — every annotated target whose
/// marker carries a visibility keyword, at any depth. the `modifiers` pass writes
/// those along with the keyword prefix it erases, so they are not references for
/// this pass to rename a second time. a type alias's name is not among them:
/// `modifiers` only erases its keyword, and the name is renamed here like every
/// other reference to it
#[derive(Default)]
struct RenamedDeclarations {
    targets: HashSet<TextRange>,
}

impl<'a> StatementVisitor<'a> for RenamedDeclarations {
    fn visit_stmt(&mut self, stmt: &'a Stmt) {
        if let Stmt::AnnAssign(assign) = stmt
            && declaration_marker_visibility(&assign.annotation) != MemberVisibility::Public
            && let Expr::Name(name) = assign.target.as_ref()
        {
            self.targets.insert(name.range);
        }
        statement_visitor::walk_stmt(self, stmt);
    }
}

/// the kind of scope a statement is directly in
#[derive(Clone, Copy, PartialEq, Eq)]
enum LexicalScope {
    Module,
    Class,
    Function,
}

/// the names a function body declares `global` — python's `global` holds for the
/// whole body, wherever in it the statement is written, and not for a nested
/// function or class
fn declared_globals(body: &[Stmt]) -> HashSet<String> {
    #[derive(Default)]
    struct Globals(HashSet<String>);
    impl<'a> StatementVisitor<'a> for Globals {
        fn visit_stmt(&mut self, stmt: &'a Stmt) {
            match stmt {
                Stmt::Global(global) => {
                    self.0
                        .extend(global.names.iter().map(|name| name.as_str().to_owned()));
                }
                Stmt::FunctionDef(_) | Stmt::ClassDef(_) => {}
                _ => statement_visitor::walk_stmt(self, stmt),
            }
        }
    }
    let mut globals = Globals::default();
    globals.visit_body(body);
    globals.0
}

/// whether a `def` or `class` carries the synthetic decorator a visibility
/// keyword parses to — its name is then renamed by the `modifiers` pass
fn has_visibility_modifier(decorators: &[ast::Decorator]) -> bool {
    decorators.iter().any(|decorator| {
        matches!(
            &decorator.expression,
            Expr::Name(name)
                if name.ctx == ExprContext::Invalid
                    && matches!(name.id.as_str(), "private" | "protected")
        )
    })
}

struct Renamer<'a> {
    types: &'a dyn TypeInfo,
    edits: Vec<(ruff_text_size::TextRange, String)>,
    /// the offset past the enclosing `def`'s header, or zero outside one
    header_end: TextSize,
    /// the module-level names this file declares `private`
    module_private: HashSet<String>,
    /// see [`RenamedDeclarations`]
    declarations: HashSet<TextRange>,
    /// the kind of scope the statement being visited is directly in
    scope: LexicalScope,
    /// the names the enclosing function declares `global`
    globals: HashSet<String>,
}

impl Renamer<'_> {
    /// `__slots__` and `__match_args__` name members by string, so a string that
    /// names a member a visibility keyword renames is renamed too. python mangles a
    /// `__slots__` entry as it does the class body's own names, so it takes the
    /// class-body spelling; `__match_args__` is read with `getattr`, which does not,
    /// so it takes the spelling that reaches the attribute from anywhere
    fn rename_member_name_lists(&mut self, class: &ast::StmtClassDef) {
        for stmt in &class.body {
            let (targets, value) = match stmt {
                Stmt::Assign(assign) => (assign.targets.as_slice(), assign.value.as_ref()),
                Stmt::AnnAssign(assign) => match &assign.value {
                    Some(value) => (std::slice::from_ref(assign.target.as_ref()), value.as_ref()),
                    None => continue,
                },
                _ => continue,
            };
            let Some(list) = targets.iter().find_map(|target| match target {
                Expr::Name(name) if matches!(name.id.as_str(), "__slots__" | "__match_args__") => {
                    Some(name.id.as_str())
                }
                _ => None,
            }) else {
                continue;
            };
            let elements = match value {
                Expr::Tuple(tuple) => tuple.elts.as_slice(),
                Expr::List(list) => list.elts.as_slice(),
                Expr::Set(set) => set.elts.as_slice(),
                Expr::StringLiteral(_) => std::slice::from_ref(value),
                _ => continue,
            };
            for element in elements {
                let Expr::StringLiteral(string) = element else {
                    continue;
                };
                let [part] = string.value.as_slice() else {
                    continue;
                };
                let Some((in_body, anywhere)) = self
                    .types
                    .class_member_spellings(class, part.value.as_ref())
                else {
                    continue;
                };
                let renamed = if list == "__slots__" {
                    in_body
                } else {
                    anywhere
                };
                self.edits.push((part.content_range(), renamed));
            }
        }
    }

    /// whether a binding of `name` written here binds the module's private
    /// symbol — at module level, or in a function that declares it `global`
    fn binds_module(&self, name: &str) -> bool {
        self.module_private.contains(name)
            && match self.scope {
                LexicalScope::Module => true,
                LexicalScope::Function => self.globals.contains(name),
                LexicalScope::Class => false,
            }
    }

    /// renames an identifier that binds a module-level private symbol
    fn rename_binding(&mut self, name: &ast::Identifier) {
        if self.binds_module(name.as_str()) {
            self.edits
                .push((name.range, module_private_name(name.as_str())));
        }
    }

    /// an import that rebinds a module-level private symbol binds its
    /// underscored name instead. an alias is renamed; an import written without
    /// one gains one, since the name after `import` is what is being imported
    fn rename_import_binding(&mut self, alias: &ast::Alias, plain_import: bool) {
        let name = alias.name.as_str();
        let bound = match &alias.asname {
            Some(asname) => asname.as_str(),
            None if plain_import => name.split('.').next().unwrap_or(name),
            None => name,
        };
        if !self.binds_module(bound) {
            return;
        }
        let renamed = module_private_name(bound);
        match &alias.asname {
            Some(asname) => self.edits.push((asname.range, renamed)),
            // `import a.b` binds its top-level package, which no alias keeps: the
            // checker reports the rebinding instead
            None if plain_import && name.contains('.') => {}
            None => self.edits.push((
                TextRange::empty(alias.name.range.end()),
                format!(" as {renamed}"),
            )),
        }
    }
}

impl<'ast> Visitor<'ast> for Renamer<'_> {
    fn visit_except_handler(&mut self, handler: &'ast ast::ExceptHandler) {
        let ast::ExceptHandler::ExceptHandler(handler_node) = handler;
        if let Some(name) = &handler_node.name {
            self.rename_binding(name);
        }
        walk_except_handler(self, handler);
    }

    fn visit_pattern(&mut self, pattern: &'ast ast::Pattern) {
        match pattern {
            ast::Pattern::MatchAs(ast::PatternMatchAs {
                name: Some(name), ..
            })
            | ast::Pattern::MatchStar(ast::PatternMatchStar {
                name: Some(name), ..
            })
            | ast::Pattern::MatchMapping(ast::PatternMatchMapping {
                rest: Some(name), ..
            }) => self.rename_binding(name),
            // `case A(x=...)` reads `x` off the subject, so it names the attribute
            // an access would
            ast::Pattern::MatchClass(class_pattern) => {
                for keyword in &class_pattern.arguments.keywords {
                    if let Some(renamed) = self
                        .types
                        .class_pattern_keyword_name(class_pattern, keyword.attr.as_str())
                    {
                        self.edits.push((keyword.attr.range, renamed));
                    }
                }
            }
            _ => {}
        }
        walk_pattern(self, pattern);
    }

    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        // a statement the parser synthesized from an `init(…)` parameter carries
        // that parameter's range, so it sits inside the signature rather than the
        // body. an edit keyed on one would rename the parameter; the `init_method`
        // pass writes those lines itself, visibility spelled out and all
        if stmt.range().start() < self.header_end {
            return;
        }
        // `global count` names the module's binding by definition, so it follows
        // the rename wherever it is written
        if let Stmt::Global(global) = stmt {
            for name in &global.names {
                if self.module_private.contains(name.as_str()) {
                    self.edits
                        .push((name.range, module_private_name(name.as_str())));
                }
            }
        }
        match stmt {
            Stmt::FunctionDef(function) => {
                if !has_visibility_modifier(&function.decorator_list) {
                    self.rename_binding(&function.name);
                }
                let enclosing = (
                    std::mem::replace(&mut self.header_end, header_end(function)),
                    std::mem::replace(&mut self.scope, LexicalScope::Function),
                    std::mem::replace(&mut self.globals, declared_globals(&function.body)),
                );
                walk_stmt(self, stmt);
                (self.header_end, self.scope, self.globals) = enclosing;
                return;
            }
            Stmt::ClassDef(class) => {
                if !has_visibility_modifier(&class.decorator_list) {
                    self.rename_binding(&class.name);
                }
                self.rename_member_name_lists(class);
                let enclosing = std::mem::replace(&mut self.scope, LexicalScope::Class);
                walk_stmt(self, stmt);
                self.scope = enclosing;
                return;
            }
            Stmt::Import(import) => {
                for alias in &import.names {
                    self.rename_import_binding(alias, true);
                }
            }
            Stmt::ImportFrom(import) => {
                for alias in &import.names {
                    if alias.name.as_str() != "*" {
                        self.rename_import_binding(alias, false);
                    }
                }
            }
            _ => {}
        }
        walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        // a reference to a module-level private symbol is renamed only when it is
        // one — a parameter, a local or a class attribute that shares the name is a
        // different binding, and renaming it would split it from its uses — and a
        // bare name in a class body when it is one of the class's own restricted
        // members. ty is asked where each resolves
        if let Expr::Name(name) = expr
            && name.ctx != ExprContext::Invalid
            // a name the parser synthesized has no source to rename
            && !name.range.is_empty()
            && !self.declarations.contains(&name.range)
        {
            if self.module_private.contains(name.id.as_str())
                && self.types.resolves_to_module_scope(name) == Some(true)
            {
                self.edits
                    .push((name.range, module_private_name(name.id.as_str())));
            } else if let Some(renamed) = self.types.class_body_member_name(name) {
                self.edits.push((name.range, renamed));
            }
        }
        if let Expr::Attribute(attribute) = expr
            && let Some(mangled) = self.types.restricted_member_name(attribute)
        {
            self.edits.push((attribute.attr.range(), mangled));
        }
        walk_expr(self, expr);
    }
}

#[cfg(test)]
mod tests {
    use crate::{Config, transpile};
    use indoc::indoc;

    fn out(input: &str) -> String {
        transpile(input, &Config::test_default()).unwrap()
    }

    #[test]
    fn a_call_reaches_the_mangled_definition() {
        let out = out(indoc! {"
            class A:
                private def helper(self) -> int:
                    return 1

                def use(self) -> int:
                    return self.helper()
        "});
        assert!(out.contains("def __helper(self) -> int:"), "got:\n{out}");
        assert!(out.contains("return self._A__helper()"), "got:\n{out}");
    }

    #[test]
    fn a_call_from_a_nested_scope_reaches_the_declaring_class() {
        // the comprehension is its own scope, and python mangles lexically —
        // the declaring class is what the written-out name records
        let out = out(indoc! {"
            class A:
                private def helper(self) -> int:
                    return 1

                def use(self) -> list[int]:
                    return [self.helper() for _ in range(2)]
        "});
        assert!(out.contains("[self._A__helper() for _"), "got:\n{out}");
    }

    #[test]
    fn a_protected_member_keeps_one_name_across_the_hierarchy() {
        // `protected` renames to `_helper`, which python does not mangle, so a
        // subclass reaches the same attribute under the same name
        let out = out(indoc! {"
            class A:
                protected def helper(self) -> int:
                    return 1

            class B(A):
                def use(self) -> int:
                    return self.helper()
        "});
        assert!(out.contains("def _helper(self) -> int:"), "got:\n{out}");
        assert!(out.contains("return self._helper()"), "got:\n{out}");
    }

    #[test]
    fn a_private_attribute_is_reached_by_its_mangled_name() {
        let out = out(indoc! {"
            class A:
                private count: int = 0

                def bump(self) -> int:
                    self.count = self.count + 1
                    return self.count
        "});
        assert!(out.contains("__count: int = 0"), "got:\n{out}");
        assert!(
            out.contains("self._A__count = self._A__count + 1"),
            "got:\n{out}"
        );
    }

    #[test]
    fn an_init_parameter_attribute_is_reached_by_the_name_it_was_written_with() {
        // the parameter keeps its own name; only the attribute is renamed
        let out = out(indoc! {"
            class A:
                init(private let x: int)

                def get(self) -> int:
                    return self.x
        "});
        assert!(out.contains("def __init__(self, x: int):"), "got:\n{out}");
        assert!(out.contains("self.__x: int = x"), "got:\n{out}");
        assert!(out.contains("return self._A__x"), "got:\n{out}");
    }

    #[test]
    fn a_visibility_keyword_composes_with_a_class_variable() {
        let out = out(indoc! {"
            class Outer:
                private class var count: int = 0
                protected class let LIMIT: int = 3
                private class total = 0

                def read(self) -> int:
                    return self.count + self.LIMIT + self.total
        "});
        assert!(out.contains("__count: ClassVar[int] = 0"), "got:\n{out}");
        assert!(out.contains("_LIMIT: Final[int] = 3"), "got:\n{out}");
        assert!(out.contains("__total: ClassVar = 0"), "got:\n{out}");
        assert!(
            out.contains("return self._Outer__count + self._LIMIT + self._Outer__total"),
            "got:\n{out}"
        );
    }

    #[test]
    fn a_class_variable_is_reached_through_the_class_object() {
        // python mangles the declaration in the class body, so an access through
        // the class itself has to name the same attribute
        let out = out(indoc! {"
            class Registry:
                private class var made: int = 0

                @classmethod
                def total(cls) -> int:
                    return cls.made

                def bump(self) -> None:
                    Registry.made += 1
        "});
        assert!(out.contains("return cls._Registry__made"), "got:\n{out}");
        assert!(out.contains("Registry._Registry__made += 1"), "got:\n{out}");
    }

    #[test]
    fn a_module_level_private_variable_is_renamed_where_it_is_the_symbol() {
        let out = out(indoc! {"
            private count: int = 0

            def bump() -> int:
                global count
                count = count + 1
                return count

            def shadow(count: int) -> int:
                return count

            class Holder:
                count = 5

                def get(self) -> int:
                    return count
        "});
        assert!(out.contains("_count: int = 0"), "got:\n{out}");
        assert!(out.contains("global _count"), "got:\n{out}");
        assert!(out.contains("_count = _count + 1"), "got:\n{out}");
        // a parameter and a class attribute that share the name are bindings of
        // their own, and are left alone with every use of them
        assert!(
            out.contains("def shadow(count: int) -> int:\n    return count\n"),
            "got:\n{out}"
        );
        assert!(out.contains("class Holder:\n    count"), "got:\n{out}");
        // a method reads past its class body to the module
        assert!(out.contains("        return _count"), "got:\n{out}");
    }

    #[test]
    fn a_member_is_renamed_wherever_the_class_body_declares_it() {
        // a declaration inside a nested block, a decorated method and a nested
        // class are members like any other, and a bare name in the class body
        // reads the declaration under the spelling python mangles there
        let out = out(indoc! {"
            class Shape:
                if True:
                    private sides: int = 3
                private scale: int = 2
                doubled = scale * 2

                private class Inner:
                    pass

                @staticmethod
                private def make() -> int:
                    return 4

                def area(self) -> int:
                    return self.sides * self.scale + self.make()

                def inner(self) -> object:
                    return Shape.Inner()
        "});
        assert!(out.contains("        __sides: int = 3"), "got:\n{out}");
        assert!(out.contains("doubled: int = __scale * 2"), "got:\n{out}");
        assert!(out.contains("class __Inner:"), "got:\n{out}");
        assert!(out.contains("    def __make() -> int:"), "got:\n{out}");
        assert!(
            out.contains("self._Shape__sides * self._Shape__scale + self._Shape__make()"),
            "got:\n{out}"
        );
        assert!(out.contains("return Shape._Shape__Inner()"), "got:\n{out}");
    }

    #[test]
    fn a_name_list_names_the_member_the_way_its_reader_looks_it_up() {
        // python mangles a `__slots__` entry like a name in the class body, but
        // reads a `__match_args__` entry and a class pattern's keyword with
        // `getattr`, which mangles nothing
        let out = out(indoc! {"
            class Slotted:
                __slots__ = ('x',)
                private x: int

            class Point:
                __match_args__ = ('x',)
                private x: int

                def unpack(self) -> int:
                    match self:
                        case Point(x=v):
                            return v
                    return -1
        "});
        assert!(out.contains("__slots__ = ('__x',)"), "got:\n{out}");
        assert!(
            out.contains("__match_args__ = ('_Point__x',)"),
            "got:\n{out}"
        );
        assert!(out.contains("case Point(_Point__x=v):"), "got:\n{out}");
    }

    #[test]
    fn a_protected_member_is_reached_through_super_and_a_union() {
        let out = out(indoc! {"
            class Base:
                protected def hook(self) -> int:
                    return 1

            class Child(Base):
                protected override def hook(self) -> int:
                    return super().hook() + 1

                def pick(self, o: Child | Other) -> int:
                    return o.hook()

            class Other(Base): ...
        "});
        assert!(out.contains("return super()._hook() + 1"), "got:\n{out}");
        assert!(out.contains("return o._hook()"), "got:\n{out}");
    }

    #[test]
    fn every_binding_of_a_module_level_private_name_is_renamed() {
        // an import, an `except` target and a match capture rebind the module's
        // symbol, so each has to bind the renamed one
        let out = out(indoc! {"
            private last: int = 0
            private err: Exception | None = None
            private def loads(s: str) -> int:
                return 0
            from json import loads

            try:
                raise ValueError('boom')
            except ValueError as err:
                pass

            match 7:
                case last:
                    pass
        "});
        assert!(out.contains("def _loads(s: str) -> int:"), "got:\n{out}");
        assert!(out.contains("loads as _loads"), "got:\n{out}");
        assert!(out.contains("except ValueError as _err:"), "got:\n{out}");
        assert!(out.contains("case _last:"), "got:\n{out}");
    }

    #[test]
    fn an_ordinary_method_is_untouched() {
        let out = out(indoc! {"
            class A:
                def helper(self) -> int:
                    return 1

                def use(self) -> int:
                    return self.helper()
        "});
        assert!(out.contains("return self.helper()"), "got:\n{out}");
    }

    #[test]
    fn a_same_named_method_on_another_class_is_untouched() {
        let out = out(indoc! {"
            class A:
                private def helper(self) -> int:
                    return 1

            class B:
                def helper(self) -> int:
                    return 2

                def use(self) -> int:
                    return self.helper()
        "});
        assert!(out.contains("return self.helper()"), "got:\n{out}");
    }
}
