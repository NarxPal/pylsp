use std::fmt::format;

use dashmap::DashMap;
use rustpython_ast::Suite;
use rustpython_parser::{Parse, ast};
use tokio::io::{stdin, stdout};
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::{
    DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams, Hover,
    HoverContents, HoverParams, HoverProviderCapability, MarkedString, MessageType,
    ServerCapabilities, TextDocumentItem, TextDocumentSyncCapability, TextDocumentSyncKind,
};
use tower_lsp::{
    Client, LanguageServer,
    lsp_types::{InitializeParams, InitializeResult, Url},
};
use tower_lsp::{LspService, Server};

struct Backend {
    client: Client, // ide/editor is the client side in the protocol
    files: DashMap<Url, (String, Option<Suite>)>,
}

impl Backend {
    // custom helper method to avoid repetitiveness
    async fn update_file(&self, uri: Url, text: String) {
        // One place for parsing logic
        let parsed_ast = match ast::Suite::parse(&text, uri.as_str()) {
            Ok(suite) => {
                self.client
                    .log_message(
                        MessageType::INFO,
                        format!("Successfully parsed {} statements", suite.len()),
                    )
                    .await;
                Some(suite)
            }
            Err(err) => {
                // You can log errors here once for both open/change
                self.client
                    .log_message(MessageType::LOG, format!("AST Parse Error: {}", err))
                    .await;
                None
            }
        };

        self.files.insert(uri, (text, parsed_ast));
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    // initializeResult returns success object to client

    async fn initialize(&self, _: InitializeParams) -> Result<InitializeResult> {
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                // handles how lsp server gets notified of file changes (it could be either save, close, open or edit)
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),

                // enable hover tooltip when user hover over their code in ide
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                ..ServerCapabilities::default() // let other fields stay as they are as defaults
            },
            ..Default::default()
        })
    }

    // did_open, when user opens a file, client sends the file content to server
    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        self.client
            .log_message(MessageType::INFO, "did_open ran")
            .await;

        // let uri = params.text_document.uri;
        // let content = params.text_document.text;

        // destructring above commented code
        let TextDocumentItem { uri, text, .. } = params.text_document;

        self.update_file(uri, text).await;
    }

    // did_close, remove file content when file is closed
    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri;

        self.client
            .log_message(MessageType::INFO, format!("closed and cleared: {}", uri))
            .await;

        self.files.remove(&uri);
    }

    // params contains the changed data sent by the editor
    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        self.client
            .log_message(MessageType::INFO, "did_change received")
            .await;

        if let Some(change) = params.content_changes.into_iter().next() {
            self.update_file(params.text_document.uri, change.text)
                .await;
        }
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        self.client
            .log_message(MessageType::INFO, "hover msg received")
            .await;

        let uri = params.text_document_position_params.text_document.uri;
        if let Some(_content) = self.files.get(&uri) {
            return Ok(Some(Hover {
                contents: HoverContents::Scalar(MarkedString::String(
                    "Hello from Rust!".to_string(),
                )),
                range: None,
            }));
        }
        Ok(None)
    }

    async fn shutdown(&self) -> Result<()> {
        // Result(), doesn't return json-rpc result
        Ok(())
    }
}

#[tokio::main]
async fn main() {
    /*
    summary:

        // service, is actual engine that process the incoming req
        // clientSocket, is a handle used by server to send notification
        // closure returns the Backend which will internally do backend.initialize, .did_change, etc
        */
    let (service, client_socket) = LspService::new(|client| Backend {
        client,
        files: DashMap::new(),
    });

    // stdin, listen to editor
    // stdout , send back to editor
    // client_socket, it lets the server to call the editor
    Server::new(stdin(), stdout(), client_socket)
        // serve, tells the server to use the rules which i provided via service which includes initialize, did_change and shutdown
        .serve(service)
        .await;
}
