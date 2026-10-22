# basedpython identity-comparison operators. python has no such operator, so a
# `.py` file spelling one is a syntax error, while the comparison keeps its shape
x === y
x !== y

# chained with each other. a chain mixing in the `is` keyword is rejected in
# basedpython mode, and has its own test in `parser::tests`
a === b !== c
a !== b
