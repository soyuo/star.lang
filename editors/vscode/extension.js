const vscode = require('vscode');
const { LanguageClient } = require('vscode-languageclient/node');

let client;

function activate(context) {
  const config = vscode.workspace.getConfiguration('star.languageServer');
  const command = config.get('path', 'star');
  const args = config.get('args', ['lsp']);
  const serverOptions = {
    command,
    args,
  };
  const clientOptions = {
    documentSelector: [{ scheme: 'file', language: 'star' }],
  };
  client = new LanguageClient('star', 'Star Language Server', serverOptions, clientOptions);
  context.subscriptions.push(client.start());
}

function deactivate() {
  return client ? client.stop() : undefined;
}

module.exports = { activate, deactivate };
