use std::collections::HashMap;
use std::ops::Range as StdRange;

use dashmap::DashMap;
use rustpython_ast::{Expr, ExprName, StmtClassDef, StmtFunctionDef, Suite, Visitor};
use rustpython_parser::{Parse, ast};
use tokio::io::{stdin, stdout};
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::{
    Diagnostic, DiagnosticSeverity, DidChangeTextDocumentParams, DidCloseTextDocumentParams,
    DidOpenTextDocumentParams, Hover, HoverContents, HoverParams, HoverProviderCapability,
    MarkedString, MessageType, Position, Range, ServerCapabilities, SymbolKind, TextDocumentItem,
    TextDocumentSyncCapability, TextDocumentSyncKind,
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

#[derive(Debug)]
struct SymbolInfo {
    name: String,
    kind: SymbolKind, // eg. variable, func, class
    location: StdRange<u32>,
    detail: Option<String>, // using "Option" since not every symbol will have detail
}

struct HoverVisitor<'a> {
    text: &'a str, // ('a) lifetime annotation
    pub symbol_table: HashMap<String, SymbolInfo>,
    target_offset: u32,
    found_name: Option<String>, // node/word found in the statement(entire line)
}

impl<'a> HoverVisitor<'a> {
    fn record_symbol(
        &mut self,
        outer_range: rustpython_parser::text_size::TextRange,
        name: &str,
        kind: SymbolKind,
        detail: Option<String>,
    ) {
        if let Some(name_range) = symbol_name_range(self.text, outer_range, name) {
            if self.target_offset >= name_range.start && self.target_offset < name_range.end {
                self.found_name = Some(name.to_string());
            }

            self.symbol_table.insert(
                name.to_string(),
                SymbolInfo {
                    name: name.to_string(),
                    kind,
                    location: name_range,
                    detail,
                },
            );
        }
    }
}

fn lsp_position_to_offset(text: &str, position: Position) -> Option<u32> {
    let mut current_line = 0u32;
    let mut current_col_utf16 = 0u32;

    // byte offset: is the index of the character which is measured in bytes
    // ch: actual char at that index
    for (byte_offset, ch) in text.char_indices() {
        // position.line: is just a line no. and not UTF-16 unit count.
        // position.char: is UTF-16 unit count. hence using .len_utf8() below
        if current_line == position.line && current_col_utf16 == position.character {
            return Some(byte_offset as u32);
        }

        if ch == '\n' {
            if current_line == position.line {
                break;
            }

            current_line += 1;
            current_col_utf16 = 0;
            continue;
        }

        if current_line == position.line {
            // current_col_utf16 is being compared to position.char above, .len_utf16() calculate the position based upon utf16 encoding, since position.char require utf16
            current_col_utf16 += ch.len_utf16() as u32;

            if current_col_utf16 == position.character {
                return Some((byte_offset + ch.len_utf8()) as u32);
            }

            if current_col_utf16 > position.character {
                return None;
            }
        }
    }

    if current_line == position.line && current_col_utf16 == position.character {
        Some(text.len() as u32)
    } else {
        None
    }
}

fn offset_to_lsp_position(text: &str, offset: usize) -> Position {
    let mut current_line = 0u32;
    let mut current_col_utf16 = 0u32;

    for (byte_offset, ch) in text.char_indices() {
        if byte_offset >= offset {
            // offset: starting byte position where error was detected
            break;
        }

        if ch == '\n' {
            current_line += 1; // increment line counter 
            current_col_utf16 = 0; // start counting from 0 
            continue;
        }

        // .len_utf16(): since lsp require column position to be counted in UTF-16 code units
        current_col_utf16 += ch.len_utf16() as u32;
    }

    Position::new(current_line, current_col_utf16)
}

fn apply_incremental_change(
    text: &mut String,
    range: Range,
    replacement: &str,
) -> std::result::Result<(), String> {
    let start = lsp_position_to_offset(text, range.start)
        .map(|offset| offset as usize) //since range require usize type
        .ok_or_else(|| format!("invalid start position: {:?}", range.start))?;
    let end = lsp_position_to_offset(text, range.end)
        .map(|offset| offset as usize)
        .ok_or_else(|| format!("invalid end position: {:?}", range.end))?;

    if start > end {
        return Err("change start is after end".to_string());
    }

    text.replace_range(start..end, replacement);
    Ok(())
}

fn symbol_name_range(
    text: &str,
    outer_range: rustpython_parser::text_size::TextRange, // directly using TextRange struct, instead of taking specific ast node type(eg. StmtFunctionDef, StmtClassDef)
    name: &str,
) -> Option<StdRange<u32>> {
    let start = outer_range.start().to_usize();
    let end = outer_range.end().to_usize();
    let snippet = &text[start..end];

    let local_start = snippet.find(name)?;
    let abs_start = start + local_start;
    let abs_end = abs_start + name.len();

    Some(abs_start as u32..abs_end as u32)
}

fn function_signature(node: &StmtFunctionDef) -> String {
    /*
        since there's no single "node.params" string which could get us the params, so we have to get params by building them using the argument nodes
    */

    let params = node
        .args
        .args
        .iter()
        .map(|arg| match &arg.def.annotation {
            // if args have type
            Some(annotation) => format!("{}: {}", arg.def.arg, render_expr(annotation.as_ref())),
            // if args have no type
            None => arg.def.arg.to_string(),
        })
        .collect::<Vec<_>>()
        .join(", ");

    match &node.returns {
        // if fn have return type
        Some(returns) => format!(
            "{}({}) -> {}",
            node.name,
            params,
            render_expr(returns.as_ref())
        ),
        // else no return type
        None => format!("{}({})", node.name, params),
    }
}

// Expr: refers to expression which is the return type of a fn
fn render_expr(expr: &Expr) -> String {
    match expr {
        Expr::Name(node) => node.id.to_string(),
        Expr::Attribute(node) => format!("{}.{}", render_expr(node.value.as_ref()), node.attr),
        Expr::Subscript(node) => format!(
            "{}[{}]",
            render_expr(node.value.as_ref()),
            render_expr(node.slice.as_ref())
        ),
        Expr::Tuple(node) => node
            .elts
            .iter()
            .map(render_expr)
            .collect::<Vec<_>>()
            .join(", "),
        _ => expr.python_name().to_string(),
    }
}

fn create_diagnostic(range: Range, message: String) -> Diagnostic {
    Diagnostic {
        range, // tells the editor which chars to highlight with squiggle
        severity: Some(DiagnosticSeverity::ERROR), // Red squiggle
        code: None, // code here refers to error code like E001
        source: Some("pylsp".to_string()), // extension source
        message, // description of error
        related_information: None,
        tags: None,
        data: None,
        code_description: None, // link to specific doc for the specific error
    }
}

impl<'a> Visitor for HoverVisitor<'a> {
    //visit_expr_name: will find the variable name
    fn visit_expr_name(&mut self, node: ExprName) {
        // range: stores the source code location, basically col and line no.
        let range = node.range;
        let start = range.start().to_u32();
        let end = range.end().to_u32();

        if self.target_offset >= start && self.target_offset < end {
            self.found_name = Some(node.id.to_string());
        }

        // will visit the name in the ast
        self.generic_visit_expr_name(node);
    }

    fn visit_stmt_function_def(&mut self, node: StmtFunctionDef) {
        self.record_symbol(
            node.range,
            node.name.as_str(),
            SymbolKind::FUNCTION,
            Some(function_signature(&node)),
        );

        // this default method, will walk inside the fn node, which includes it's body and related nodes
        self.generic_visit_stmt_function_def(node);
    }

    fn visit_stmt_class_def(&mut self, node: StmtClassDef) {
        self.record_symbol(
            node.range,
            node.name.as_str(),
            SymbolKind::CLASS,
            Some(format!("class {}", node.name)),
        );

        self.generic_visit_stmt_class_def(node);
    }
}

impl Backend {
    // custom helper method to avoid repetitiveness
    async fn update_file(&self, uri: Url, text: String) {
        // One place for parsing logic
        let parsed_ast = match ast::Suite::parse(&text, uri.as_str()) {
            Ok(suite) => {
                self.client
                    .publish_diagnostics(uri.clone(), vec![], None)
                    .await;
                self.client
                    .log_message(
                        MessageType::INFO,
                        format!("Successfully parsed {} statements", suite.len()),
                    )
                    .await;
                Some(suite)
            }
            Err(err) => {
                let error_position = offset_to_lsp_position(&text, err.offset.to_usize());
                let diagnostic = create_diagnostic(
                    Range::new(error_position, error_position),
                    format!("Syntax Error: {}", err.error),
                );

                self.client
                    .publish_diagnostics(uri.clone(), vec![diagnostic], None)
                    .await;

                // You can log errors here once for both open/change
                self.client
                    .log_message(MessageType::LOG, format!("AST Parse Error: {}", err))
                    .await;

                None
            }
        };
        // update file in memory
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
                    TextDocumentSyncKind::INCREMENTAL,
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

        let uri = params.text_document.uri;

        // entry: refer to tuple (String, Option<Suite>)
        if let Some(mut entry) = self.files.get_mut(&uri) {
            // params.content_changes is a vec
            for change in params.content_changes {
                if let Some(range) = change.range {
                    if let Err(err) = apply_incremental_change(&mut entry.0, range, &change.text) {
                        self.client
                            .log_message(
                                MessageType::ERROR,
                                format!("failed to apply incremental change: {}", err),
                            )
                            .await;
                        return;
                    }
                }
                // if no range field(which mean no changes), than "change.text" will contain entire file text
                else {
                    entry.0 = change.text;
                }
            }

            let updated_text = entry.0.clone();
            drop(entry);
            self.update_file(uri, updated_text).await;
        }
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        self.client
            .log_message(MessageType::INFO, "hover msg received")
            .await;

        let uri = params.text_document_position_params.text_document.uri;
        // position: tells the line & character where hover occured
        let position = params.text_document_position_params.position;

        if let Some(entry) = self.files.get(&uri) {
            // text: String, maybe_ast: Option<suite> for key "uri"
            let (text, maybe_ast) = entry.value();

            // nested pattern matching
            if let (Some(target_offset), Some(suite)) =
                (lsp_position_to_offset(text, position), maybe_ast)
            {
                // visitor will contains
                let mut visitor = HoverVisitor {
                    text: text,
                    symbol_table: HashMap::new(),
                    target_offset,
                    found_name: None,
                };

                for stmt in suite {
                    /*  visit_stmt take one node and start traversing/visiting it.
                        visit_stmt calls visit_stmt_function_def.
                        which than call generic_visit_stmt_function_def which will decide whether it's fn or expr name.
                    */
                    visitor.visit_stmt(stmt.clone());
                    if visitor.found_name.is_some() {
                        break; // exit for loop once name is found
                    }
                }

                if let Some(name) = &visitor.found_name {
                    if let Some(fn_info) = visitor.symbol_table.get(name) {
                        let hover_text = match &fn_info.detail {
                            Some(detail) => format!("{}\nkind: {:?}", detail, fn_info.kind),
                            None => format!("{}\nkind: {:?}", fn_info.name, fn_info.kind),
                        };

                        return Ok(Some(Hover {
                            contents: HoverContents::Scalar(MarkedString::String(hover_text)),
                            range: None,
                        }));
                    }

                    return Ok(Some(Hover {
                        contents: HoverContents::Scalar(MarkedString::String(format!(
                            "You are hovering over: {} ",
                            name
                        ))),
                        range: None,
                    }));
                }
            }
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

        // service: is actual engine that process the incoming req
        // clientSocket: is a handle used by server to send notification
        // closure returns the Backend which will internally do backend.initialize, .did_change, etc
        */
    let (service, client_socket) = LspService::new(|client| Backend {
        client,
        files: DashMap::new(),
    });

    // stdin: listen to editor
    // stdout: send back to editor
    // client_socket: it lets the server to call the editor
    Server::new(stdin(), stdout(), client_socket)
        // serve, tells the server to use the rules which i provided via service which includes initialize, did_change and shutdown
        .serve(service)
        .await;
}
