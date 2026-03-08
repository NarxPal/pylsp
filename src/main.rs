use dashmap::DashMap;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::{
    HoverProviderCapability, ServerCapabilities, TextDocumentSyncCapability, TextDocumentSyncKind,
};
use tower_lsp::{
    Client, LanguageServer,
    lsp_types::{InitializeParams, InitializeResult, Url},
};

struct Backend {
    client: Client, // ide/editor is the client side in the protocol
    files: DashMap<Url, String>,
}

impl LanguageServer for Backend {
    // initializeResult returns success object to client
    fn initialize(&self, _: InitializeParams) -> Result<InitializeResult> {
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                // handles how lsp server gets notified of file changes (it could be either save, close, open or edit)
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),

                // enable hover tooltip when user hover over their code in ide
                hover_provider: Some((HoverProviderCapability::Simple(true))),
                ..Default::exhaustive()
            },
            ..Default::default()
        })
    }

    fn shutdown(&self) -> Result<()> {
        // Result(), doesn't return json-rpc result
        Ok(())
    }
}

fn main() {}
