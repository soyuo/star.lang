# Editor and Language Server Support

Status: stable

`star lsp` starts the Star Language Server Protocol server on standard input and
standard output. It supports full-document synchronization, lexical/parser/
semantic diagnostics, instruction completion, hover, semantic tokens, mnemonic
inlay hints, call-opcode signature help, a document-formatting code action,
whole-document formatting, block symbols and folding, and
definition/references/rename for directly encoded numeric IDs in the current
open document.

Completion labels and inlay hints display instruction mnemonics such as `ADD`,
but completion inserts the catalog's actual source glyph spelling. Hints are
virtual editor text; they never modify the Star source file. Tree-sitter and
TextMate grammars highlight bit literals and instruction-shaped tokens. The
language server validates whether extended tokens are assigned opcodes and
whether the program's structure is valid.

ID navigation applies when an ID is an adjacent bit literal followed by one of
`DECLARE`, `CONST`, `DELETE`, `LOAD`, `STORE`, `LOAD_GLOBAL`, or `STORE_GLOBAL`.
Navigation and rename are limited to the current open file. A definition is
returned only when the ID has one unambiguous definition in that file. Rename
accepts a non-negative decimal ID and rewrites matching literals. IDs encoded
through computed stack values, function headers, imports, or unopened files are
not resolved by this initial server.

The server advertises UTF-16 LSP positions and publishes diagnostics after
open/change. Signature help reports stack ABI signatures for `CALL`,
`TAIL_CALL`, `CALL_VALUE`, `MODULE_CALL`, and `METHOD_CALL`; it does not infer a
runtime stack's argument values. The formatting action and formatting request
replace the complete document and are available only when parsing succeeds.
The Tree-sitter grammar handles source token shapes and formatting whitespace;
the LSP supplies structural symbols and folding ranges.

## Editor packages

- VS Code: install `editors/vscode` as an extension package; by default it
  starts `star lsp`. Set `star.languageServer.path` and `.args` if the executable
  is named differently.
- Zed: `editors/zed` registers the Tree-sitter grammar in
  `editors/tree-sitter-star` and starts `star lsp`.
- Neovim: add `editors/neovim` to `runtimepath`, then source
  `editors/neovim/lsp.lua`. The Tree-sitter grammar is under
  `editors/tree-sitter-star`.
- Tree-sitter: run `npm install` and `npm run generate` in
  `editors/tree-sitter-star`. The grammar recognizes basic instructions,
  extended instruction forms, and non-empty bit literals with optional ASCII
  formatting whitespace.

All editor integrations preserve the language rule that source contains only
the three syntax characters `*`, `'`, and `"`, plus formatting whitespace where
allowed. Mnemonic labels are display metadata and are never accepted as source
syntax.
