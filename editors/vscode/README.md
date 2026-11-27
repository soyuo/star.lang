# VS Code extension

Install Node.js, run `npm install` in this directory, and package the extension
with `npx @vscode/vsce package`. Install the resulting VSIX in VS Code. The
extension starts `star lsp` by default; use the `star.languageServer.path` and
`star.languageServer.args` settings to point at another executable.
