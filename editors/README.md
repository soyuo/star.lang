# Editor packages

These editor integrations start the `star lsp` command supplied by the Star
CLI. Put the CLI on `PATH`, or configure a direct language-server executable in
the editor package.

- VS Code: install dependencies and package the extension from `vscode/`.
- Zed: install `zed/` as an extension; its grammar points to
  `tree-sitter-star/` in this repository.
- Neovim: add `neovim/` to `runtimepath`, then source its `lsp.lua`.
- Tree-sitter: generate the parser in `tree-sitter-star/` with `npm install`
  and `npm run generate`.

The LSP provides semantic validation, readable mnemonic inlay hints, and
structural navigation while source files retain their three-glyph syntax.
