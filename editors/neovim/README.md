# Neovim

Add this directory's `ftdetect/star.lua` to your runtime path, then source
`lsp.lua` to start the server for `.star` buffers. By default it runs `star
lsp`; set `vim.env.STAR_LSP` to a direct `star-lsp` executable path to omit the
`lsp` argument.

The Tree-sitter grammar and query are under `../tree-sitter-star`. Generate the
parser with the Tree-sitter CLI and install its parser as `star`; the query in
`queries/star/highlights.scm` highlights instructions and bit literals. The
language server remains responsible for catalog validation and readable
mnemonic inlay hints.
