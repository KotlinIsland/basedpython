# basedpython identity-comparison operators. this fixture parses in python
# mode, where the lexer still produces the tokens — a `.py` file spelling them
# was never valid python, and the parser's job here is only to keep the shape
x === y
x !== y

# chained with each other. a chain mixing in the `is` keyword is rejected in
# basedpython mode, and has its own test in `parser::tests`
a === b !== c
a !== b
