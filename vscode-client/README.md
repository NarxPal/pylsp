# Python LS Rust Client

This folder contains a minimal VS Code extension that launches the Rust language server binary from `../target/debug/python-ls-rust`.

## Run

1. Open this repository in VS Code.
2. From `vscode-client/`, run `npm install`.
3. From the project root, run `cargo build`.
4. Start the `Run Python LS Rust Client` launch configuration.
5. In the Extension Development Host window, open `sample.py` with `File -> Open File...`, edit once, then hover to see the server response.
