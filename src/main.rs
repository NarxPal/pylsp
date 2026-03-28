use std::collections::HashMap;
use std::ops::Range as StdRange;
use std::path::{Path, PathBuf};
use std::time::Instant;
use std::{env, fs};

use dashmap::DashMap;
use rustpython_ast::{Expr, ExprName, StmtClassDef, StmtFunctionDef, Suite, Visitor};
use rustpython_parser::{Parse, ast};
use tokio::io::{stdin, stdout};
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::{
    Diagnostic, DiagnosticSeverity, DidChangeTextDocumentParams, DidCloseTextDocumentParams,
    DidOpenTextDocumentParams, DocumentSymbol, DocumentSymbolParams, DocumentSymbolResponse,
    GotoDefinitionParams, GotoDefinitionResponse, Hover, HoverContents, HoverParams,
    HoverProviderCapability, Location, MarkedString, MessageType, OneOf, Position, Range,
    ReferenceParams, ServerCapabilities, SymbolKind, TextDocumentItem, TextDocumentSyncCapability,
    TextDocumentSyncKind,
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
    target_offset: u32,         // range
    found_name: Option<String>, // node/word found in the statement(entire line)
}

struct ReferencesVisitor {
    target_name: String, // symbol_name
    locations: Vec<StdRange<u32>>,
}

struct DocumentSymbolVisitor<'a> {
    text: &'a str,
    symbols: Vec<DocumentSymbol>,
}

struct ParseErrorDetail<'a> {
    path: &'a PathBuf,
    text: String, // file content
    line: u32,
    column: u32,
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

// convert {line, char} to byte_number
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

// convert byte_number to {line, char}
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

    let local_start = snippet.find(name)?; //get the name from func/class definition
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

impl Visitor for ReferencesVisitor {
    fn visit_expr_name(&mut self, node: ExprName) {
        if node.id.as_str() == self.target_name {
            self.locations
                .push(node.range.start().to_u32()..node.range.end().to_u32());
        }

        self.generic_visit_expr_name(node);
    }

    fn visit_stmt_function_def(&mut self, node: StmtFunctionDef) {
        if node.name.as_str() == self.target_name {
            self.locations
                .push(node.range.start().to_u32()..node.range.end().to_u32());
        }
        self.generic_visit_stmt_function_def(node);
    }

    fn visit_stmt_class_def(&mut self, node: StmtClassDef) {
        if node.name.as_str() == self.target_name {
            self.locations
                .push(node.range.start().to_u32()..node.range.end().to_u32());
        }
        self.generic_visit_stmt_class_def(node);
    }
}

impl<'a> Visitor for DocumentSymbolVisitor<'a> {
    fn visit_stmt_function_def(&mut self, node: StmtFunctionDef) {
        if let Some(name_range) = symbol_name_range(self.text, node.range, node.name.as_str()) {
            let selection_range = Range::new(
                offset_to_lsp_position(self.text, name_range.start as usize),
                offset_to_lsp_position(self.text, name_range.end as usize),
            );

            let range = Range::new(
                offset_to_lsp_position(self.text, node.range.start().to_usize()),
                offset_to_lsp_position(self.text, node.range.end().to_usize()),
            );

            self.symbols.push(DocumentSymbol {
                name: node.name.to_string(),
                detail: None,
                kind: SymbolKind::FUNCTION,
                tags: None,
                deprecated: None,
                range: range,
                selection_range: selection_range,
                children: None,
            });
        }

        self.generic_visit_stmt_function_def(node);
    }

    fn visit_stmt_class_def(&mut self, node: StmtClassDef) {
        self.generic_visit_stmt_class_def(node);
    }
}

impl Backend {
    fn find_symbol_at_offset<'a>(
        text: &'a str,
        suite: &'a Suite,
        target_offset: u32,
    ) -> HoverVisitor<'a> {
        let mut visitor = HoverVisitor {
            text,
            symbol_table: HashMap::new(),
            target_offset,
            found_name: None,
        };

        /*  visit_stmt take one node and start traversing/visiting it.
            visit_stmt calls visit_stmt_function_def.
            which than call generic_visit_stmt_function_def which will decide whether it's fn or expr name.
        */
        for stmt in suite {
            visitor.visit_stmt(stmt.clone());
            if visitor.found_name.is_some() {
                break;
            }
        }

        visitor
    }

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

                definition_provider: Some(OneOf::Left(true)),

                references_provider: Some(OneOf::Left(true)),

                document_symbol_provider: Some(OneOf::Left(true)),

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

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        self.client
            .log_message(MessageType::INFO, "goto_definition ran")
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
                let visitor = Backend::find_symbol_at_offset(text, suite, target_offset);

                if let Some(name) = &visitor.found_name {
                    if let Some(symbol) = visitor.symbol_table.get(name) {
                        let definition_range = Range::new(
                            offset_to_lsp_position(text, symbol.location.start as usize),
                            offset_to_lsp_position(text, symbol.location.end as usize),
                        );

                        return Ok(Some(GotoDefinitionResponse::Scalar(Location::new(
                            uri.clone(),
                            definition_range,
                        ))));
                    }
                }
            }
        }

        Ok(None)
    }

    async fn references(&self, params: ReferenceParams) -> Result<Option<Vec<Location>>> {
        self.client
            .log_message(MessageType::INFO, "references ran")
            .await;

        // position: tells the line & character where hover occured
        let position = params.text_document_position.position;

        let uri = params.text_document_position.text_document.uri;

        if let Some(entry) = self.files.get(&uri) {
            let (text, maybe_ast) = entry.value();

            // nested pattern matching
            if let (Some(target_offset), Some(suite)) =
                (lsp_position_to_offset(text, position), maybe_ast)
            {
                let visitor = Backend::find_symbol_at_offset(text, suite, target_offset);

                if let Some(name) = &visitor.found_name {
                    let mut references_visitor = ReferencesVisitor {
                        target_name: name.clone(),
                        locations: Vec::new(),
                    };

                    for stmt in suite {
                        references_visitor.visit_stmt(stmt.clone());
                    }

                    let mut locations = Vec::new();

                    for location in references_visitor.locations {
                        let range = Range::new(
                            offset_to_lsp_position(text, location.start as usize),
                            offset_to_lsp_position(text, location.end as usize),
                        );

                        locations.push(Location::new(uri.clone(), range));
                    }
                    return Ok(Some(locations));
                }
            }
        }

        Ok(None)
    }

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        self.client
            .log_message(MessageType::INFO, "fetch outline ran")
            .await;

        let uri = params.text_document.uri;

        if let Some(entry) = self.files.get(&uri) {
            // text: String, maybe_ast: Option<suite> for key "uri"
            let (text, maybe_ast) = entry.value();

            let mut symbol_visitor = DocumentSymbolVisitor {
                text,
                symbols: Vec::new(),
            };

            if let Some(suite) = maybe_ast {
                for stmt in suite {
                    symbol_visitor.visit_stmt(stmt.clone());
                }
            }

            return Ok(Some(DocumentSymbolResponse::Nested(symbol_visitor.symbols)));
        }

        Ok(None)
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
                let visitor = Backend::find_symbol_at_offset(text, suite, target_offset);

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

fn collect_py_files(dir: &Path, out: &mut Vec<PathBuf>) {
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            // if it's a folder
            if path.is_dir() {
                collect_py_files(&path, out);
            }
            // if it's a file
            else if path.extension().and_then(|e| e.to_str()) == Some("py") {
                out.push(path);
            }
        }
    }
}

fn run_batch_bench(root: &Path) {
    // pathbuf: a string which can handle path quirks like (\ or /)
    let mut files: Vec<PathBuf> = Vec::new();
    collect_py_files(root, &mut files);

    // instant: it's used for measuring the batched files parsing
    let start = Instant::now();
    // count how many .py files parsing succeeded and failed
    let mut passed = 0usize;
    let mut failed = 0usize;
    let mut diagnostic_err = 0usize;
    let mut err_in_files: Vec<ParseErrorDetail> = Vec::new();
    let mut total_lines_parsed = 0usize;

    for path in &files {
        match fs::read_to_string(path) {
            Ok(text) => {
                total_lines_parsed += text.lines().count();

                // to_string_lossy: converts C-style string into rust String
                match ast::Suite::parse(&text, &path.to_string_lossy()) {
                    Ok(_) => {
                        passed += 1;
                    }
                    Err(parse_err) => {
                        failed += 1;
                        diagnostic_err += 1;

                        let pos = offset_to_lsp_position(&text, parse_err.offset.to_usize());

                        err_in_files.push(ParseErrorDetail {
                            path,
                            text: parse_err.to_string(),
                            line: pos.line + 1, // editor line start from 1 and not 0
                            column: pos.line + 1,
                        })
                    }
                }
            }
            Err(_) => {
                failed += 1;
            }
        }
    }
    // total time spent since instant(start) was created
    let elapsed = start.elapsed();
    println!("files: {}", files.len()); // total number of file paths
    println!("total number of lines parsed: {}", total_lines_parsed);
    println!("passed: {}", passed);
    println!("failed: {}", failed);
    println!("diagnostic err files: {}", diagnostic_err);
    println!("elapsed_ms: {}", elapsed.as_millis());
    if elapsed.as_secs_f64() > 0.0 {
        println!(
            "files_per_sec: {:.2}", // round to two decimal places
            // convert files.len into f64 for calculating with time
            files.len() as f64 / elapsed.as_secs_f64()
        );
    }

    if err_in_files.is_empty() {
        println!("All files parsed successfully!");
    } else {
        println!("Found error while parsing bulk files");
        for err in err_in_files {
            println!(
                "File: {:?} at Line: {}, Col: {} - Error: {}",
                err.path, err.line, err.column, err.text
            );
        }
    }
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = env::args().collect();
    // --bench is custom arg
    if args.len() >= 3 && args[1] == "--bench" {
        println!("bench mode ");
        run_batch_bench(Path::new(&args[2]));
        return;
    }

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
