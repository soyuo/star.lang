# star.lang

## Rust workspace

The initial Rust workspace is split into `star-common`, `star-lexer`, `star-parser`, `star-interpreter`, and the `star` command-line binary. The crates form a dependency chain from common source types through the lexer and parser to the interpreter and CLI. Implementations are introduced by later roadmap items.

Run `cargo check --workspace` to check that all workspace crates build.
세상에 "별 언어"가 다 있다.
