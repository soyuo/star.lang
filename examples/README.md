# Reference interpreter examples

Run a source file from the repository root with:

```powershell
cargo run -p star-interpreter --example run_star -- examples/calculator.star
```

The files use Star's three-character source alphabet and normal-mode formatting
whitespace. `calculator.star` reads two decimal integers on separate lines and
writes their sum. `fibonacci.star` prints `55` by recursively evaluating the
tenth Fibonacci number. `factorial.star` prints `720` by recursively computing
`6!`.

`brainfuck.star` reads a Brainfuck program on the first input line and its data
on the second. It supports `+ - < > [ ] . ,`, ignores other characters, uses
8-bit wrapping cells, and grows its tape in either direction. Output cells from
`0` through `127` are emitted as ASCII; larger cells are emitted as the
corresponding Unicode scalar encoded in UTF-8.
