//! which compiled functions can reach themselves through native calls
//!
//! a native call pushes a C frame and no python one, so nothing the interpreter
//! counts stands between a function and the stack running out. a chain of such calls
//! with no cycle in it is as deep as the module is long, and the frames around it are
//! counted by whatever entered it; a cycle is as deep as its arguments say. so a
//! function on a cycle is the one place a depth has to be counted, and whether it is on
//! one is a question about the call graph alone — what the arguments are typed as has
//! nothing to add

use std::collections::{HashMap, HashSet};

use crate::function::{ModuleIr, qualify};
use crate::ops::Op;

/// the qualified name of every function that lies on a cycle of native calls,
/// including one that calls itself
pub fn recursive_functions(module: &ModuleIr) -> HashSet<String> {
    let names: Vec<String> = module
        .all_functions()
        .map(crate::function::Function::qualified_name)
        .collect();
    let index: HashMap<&str, usize> = names
        .iter()
        .enumerate()
        .map(|(at, name)| (name.as_str(), at))
        .collect();
    let edges: Vec<Vec<usize>> = module
        .all_functions()
        .map(|function| {
            let mut callees: Vec<usize> = function
                .blocks
                .iter()
                .flat_map(|block| &block.ops)
                .filter_map(|op| match op {
                    Op::CallNative { owner, callee, .. } => index
                        .get(qualify(owner.as_deref(), callee).as_str())
                        .copied(),
                    _ => None,
                })
                .collect();
            callees.sort_unstable();
            callees.dedup();
            callees
        })
        .collect();

    let mut recursive = HashSet::new();
    for component in strongly_connected(&edges) {
        let on_a_cycle = match component.as_slice() {
            [only] => edges[*only].contains(only),
            _ => true,
        };
        if on_a_cycle {
            recursive.extend(component.into_iter().map(|at| names[at].clone()));
        }
    }
    recursive
}

/// tarjan's strongly connected components, iteratively
///
/// a module is free to be one long chain of calls, and walking it recursively would
/// put the compiler itself at the mercy of the stack this analysis exists to protect
fn strongly_connected(edges: &[Vec<usize>]) -> Vec<Vec<usize>> {
    const UNVISITED: usize = usize::MAX;
    let count = edges.len();
    let mut order = vec![UNVISITED; count];
    let mut low = vec![0; count];
    let mut on_stack = vec![false; count];
    let mut stack: Vec<usize> = Vec::new();
    let mut components = Vec::new();
    let mut next = 0;

    for root in 0..count {
        if order[root] != UNVISITED {
            continue;
        }
        // each entry is a node and how many of its edges have been followed
        let mut walk: Vec<(usize, usize)> = vec![(root, 0)];
        order[root] = next;
        low[root] = next;
        next += 1;
        stack.push(root);
        on_stack[root] = true;
        while let Some(frame) = walk.last_mut() {
            let node = frame.0;
            if let Some(&target) = edges[node].get(frame.1) {
                frame.1 += 1;
                if order[target] == UNVISITED {
                    order[target] = next;
                    low[target] = next;
                    next += 1;
                    stack.push(target);
                    on_stack[target] = true;
                    walk.push((target, 0));
                } else if on_stack[target] {
                    low[node] = low[node].min(order[target]);
                }
                continue;
            }
            walk.pop();
            if let Some(&(parent, _)) = walk.last() {
                low[parent] = low[parent].min(low[node]);
            }
            if low[node] == order[node] {
                let mut component = Vec::new();
                while let Some(member) = stack.pop() {
                    on_stack[member] = false;
                    component.push(member);
                    if member == node {
                        break;
                    }
                }
                components.push(component);
            }
        }
    }
    components
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builder::FunctionBuilder;
    use crate::ops::{Terminator, Value};
    use crate::rtype::RType;

    fn calling(name: &str, callees: &[&str]) -> crate::function::Function {
        let mut builder = FunctionBuilder::new(name, RType::NONE);
        for callee in callees {
            builder.push(Op::CallNative {
                dest: None,
                owner: None,
                callee: (*callee).to_string(),
                args: Vec::new(),
            });
        }
        builder.terminate(Terminator::Return(Value::None));
        builder.finish()
    }

    fn module_of(functions: Vec<crate::function::Function>) -> ModuleIr {
        let mut module = ModuleIr::new("app");
        module.functions = functions;
        module
    }

    #[test]
    fn a_function_calling_itself_is_on_a_cycle() {
        let module = module_of(vec![
            calling("depth", &["depth"]),
            calling("entry", &["depth"]),
        ]);
        assert_eq!(
            recursive_functions(&module),
            HashSet::from(["depth".to_string()])
        );
    }

    #[test]
    fn functions_calling_each_other_are_on_one_cycle_and_a_chain_is_not() {
        let module = module_of(vec![
            calling("ping", &["pong"]),
            calling("pong", &["ping", "leaf"]),
            calling("leaf", &[]),
            calling("top", &["middle"]),
            calling("middle", &["leaf"]),
        ]);
        assert_eq!(
            recursive_functions(&module),
            HashSet::from(["ping".to_string(), "pong".to_string()])
        );
    }

    #[test]
    fn a_long_chain_of_calls_is_walked_without_recursion() {
        let names: Vec<String> = (0..200_000).map(|at| format!("f{at}")).collect();
        let functions = names
            .iter()
            .enumerate()
            .map(|(at, name)| match names.get(at + 1) {
                Some(next) => calling(name, &[next.as_str()]),
                None => calling(name, &["f0"]),
            })
            .collect();
        assert_eq!(recursive_functions(&module_of(functions)).len(), 200_000);
    }
}
