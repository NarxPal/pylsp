const path = require("path");
const vscode = require("vscode"); // vs code extension api
const { LanguageClient, TransportKind } = require("vscode-languageclient/node");

let client;

function activate(context) {
  const serverPath = path.resolve(
    __dirname,
    "..",
    "target",
    "debug",
    "python-ls-rust"
  );

  const serverOptions = {
    command: serverPath,
    transport: TransportKind.stdio,
  };

  const clientOptions = {
    documentSelector: [{ scheme: "file", language: "python" }],
    outputChannel: vscode.window.createOutputChannel("Python LS Rust"),
  };

  client = new LanguageClient(
    "pythonLsRust",
    "Python LS Rust",
    serverOptions,
    clientOptions
  );

  context.subscriptions.push(client.start());
}

function deactivate() {
  if (!client) {
    return undefined;
  }

  return client.stop();
}

module.exports = {
  activate,
  deactivate,
};
