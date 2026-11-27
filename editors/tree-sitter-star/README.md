# Tree-sitter grammar for Star

This grammar recognizes basic instructions, assigned-shape extended instruction
tokens, and non-empty bit literals. It accepts Star's ASCII formatting
whitespace, including whitespace between the bits of a token. The LSP performs
catalog assignment and structural validation and supplies semantic highlights,
block symbols, and folding ranges.

Generate the parser with `npm install` followed by `npm run generate`. Zed's
extension references this directory as a subpath of the `soyuo/star.lang` Git
repository.
