/*!
This module provides the trait implementations necessary to implement each of
the LSP request handlers.

Every request handler should live in its own module, with a module name
matching the LSP protocol request name as closely as possible. This should be
done even when there is tight coupling between multiple request handlers (like
type hierarchy) to make it easy to continue to find the right handler given
knowledge about the request name.

If request handlers need shared helper functions, they can go in a sibling
module. For example, see `super::type_hierarchy`.
*/

mod alignment_groups;
mod build_output;
mod call_hierarchy_incoming_calls;
mod call_hierarchy_outgoing_calls;
mod change_signature;
mod check_workspace;
mod classified_references;
mod code_action;
mod code_action_resolve;
mod code_lens;
mod complete_statement;
mod completion;
mod copied_imports;
mod data_flow;
mod declaration;
mod detached_transpile;
mod diagnostic;
mod doc_highlights;
mod docstring_stub;
mod document_inherited_members;
mod document_super_members;
mod document_symbols;
mod document_usage_counts;
mod entry_point;
mod evaluatable_expression;
mod execute_command;
mod explain_rule;
mod explain_transpilation;
mod expression_types;
mod folding_range;
mod goto_declaration;
mod goto_definition;
mod goto_implementation;
mod goto_type_definition;
mod hover;
mod injections;
mod inlay_hints;
mod module_name;
mod move_members;
mod move_symbol;
mod mro;
mod on_type_formatting;
mod overridable_members;
mod override_members;
mod pasted_imports;
mod prepare_call_hierarchy;
mod prepare_rename;
mod prepare_type_hierarchy;
pub(super) mod references;
mod rename;
mod run_modules;
mod selection_range;
mod semantic_tokens;
mod semantic_tokens_range;
mod shutdown;
mod signature_help;
mod super_members;
mod syntax_outline;
mod test_items;
mod test_subjects;
mod transpile;
mod transpile_for_build;
mod type_hierarchy_subtypes;
mod type_hierarchy_supertypes;
mod unwrap;
mod will_rename_files;
mod workspace_diagnostic;
mod workspace_symbols;

pub(super) use alignment_groups::AlignmentGroupsRequestHandler;
pub(super) use build_output::BuildOutputRequestHandler;
pub(super) use call_hierarchy_incoming_calls::CallHierarchyIncomingCallsRequestHandler;
pub(super) use call_hierarchy_outgoing_calls::CallHierarchyOutgoingCallsRequestHandler;
pub(super) use change_signature::{
    ChangeSignatureRequestHandler, PrepareChangeSignatureRequestHandler,
};
pub(super) use check_workspace::CheckWorkspaceRequestHandler;
pub(crate) use check_workspace::SuspendedWorkspaceRequestKind;
pub(super) use classified_references::ClassifiedReferencesRequestHandler;
pub(super) use code_action::CodeActionRequestHandler;
pub(super) use code_action_resolve::CodeActionResolveRequestHandler;
pub(super) use code_lens::CodeLensRequestHandler;
pub(super) use complete_statement::CompleteStatementRequestHandler;
pub(super) use completion::CompletionRequestHandler;
pub(super) use copied_imports::CopiedImportsRequestHandler;
pub(super) use data_flow::DataFlowRequestHandler;
pub(super) use declaration::DeclarationRequestHandler;
pub(super) use diagnostic::DocumentDiagnosticRequestHandler;
pub(super) use doc_highlights::DocumentHighlightRequestHandler;
pub(super) use docstring_stub::DocstringStubRequestHandler;
pub(super) use document_inherited_members::DocumentInheritedMembersRequestHandler;
pub(super) use document_super_members::DocumentSuperMembersRequestHandler;
pub(super) use document_symbols::DocumentSymbolRequestHandler;
pub(super) use document_usage_counts::DocumentUsageCountsRequestHandler;
pub(super) use entry_point::EntryPointHandler;
pub(super) use evaluatable_expression::EvaluatableExpressionRequestHandler;
pub(super) use execute_command::ExecuteCommand;
pub(super) use explain_rule::ExplainRuleHandler;
pub(super) use explain_transpilation::ExplainTranspilationHandler;
pub(super) use expression_types::ExpressionTypesRequestHandler;
pub(super) use folding_range::FoldingRangeRequestHandler;
pub(super) use goto_declaration::GotoDeclarationRequestHandler;
pub(super) use goto_definition::GotoDefinitionRequestHandler;
pub(super) use goto_implementation::GotoImplementationRequestHandler;
pub(super) use goto_type_definition::GotoTypeDefinitionRequestHandler;
pub(super) use hover::HoverRequestHandler;
pub(super) use injections::InjectionsRequestHandler;
pub(super) use inlay_hints::InlayHintRequestHandler;
pub(super) use module_name::ModuleNameHandler;
pub(super) use move_members::{
    ExtractSuperclassRequestHandler, PrepareMoveMembersRequestHandler, PullMembersUpRequestHandler,
    PushMembersDownRequestHandler,
};
pub(super) use move_symbol::{MoveSymbolRequestHandler, PrepareMoveSymbolRequestHandler};
pub(super) use mro::MroRequestHandler;
pub(super) use on_type_formatting::OnTypeFormattingRequestHandler;
pub(super) use overridable_members::OverridableMembersRequestHandler;
pub(super) use override_members::OverrideMembersRequestHandler;
pub(super) use pasted_imports::PastedImportsRequestHandler;
pub(super) use prepare_call_hierarchy::PrepareCallHierarchyRequestHandler;
pub(super) use prepare_rename::PrepareRenameRequestHandler;
pub(super) use prepare_type_hierarchy::PrepareTypeHierarchyRequestHandler;
pub(super) use references::ReferencesRequestHandler;
pub(super) use rename::RenameRequestHandler;
pub(super) use run_modules::RunModulesHandler;
pub(super) use selection_range::SelectionRangeRequestHandler;
pub(super) use semantic_tokens::SemanticTokensRequestHandler;
pub(super) use semantic_tokens_range::SemanticTokensRangeRequestHandler;
pub(super) use shutdown::ShutdownHandler;
pub(super) use signature_help::SignatureHelpRequestHandler;
pub(super) use super_members::SuperMembersRequestHandler;
pub(super) use syntax_outline::SyntaxOutlineRequestHandler;
pub(super) use test_items::TestItemsHandler;
pub(super) use test_subjects::{SubjectTestsRequestHandler, TestSubjectsRequestHandler};
pub(super) use transpile::TranspileRequestHandler;
pub(super) use transpile_for_build::TranspileForBuildRequestHandler;
pub(super) use type_hierarchy_subtypes::TypeHierarchySubtypesRequestHandler;
pub(super) use type_hierarchy_supertypes::TypeHierarchySupertypesRequestHandler;
pub(super) use unwrap::UnwrapRequestHandler;
pub(super) use will_rename_files::WillRenameFilesRequestHandler;
pub(super) use workspace_diagnostic::WorkspaceDiagnosticRequestHandler;
pub(super) use workspace_symbols::WorkspaceSymbolRequestHandler;
