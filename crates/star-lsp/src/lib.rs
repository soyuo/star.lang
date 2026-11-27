//! Language Server Protocol support for Star source files.

use std::collections::{HashMap, HashSet};
use std::error::Error;

use lsp_server::{Connection, Message, Notification, Request, Response};
use num_bigint::BigUint;
use num_traits::Zero;
use serde_json::{json, Value};
use star_interpreter::semantic;
use star_lexer::{normalize_source, LexMode, NormalizedSource};
use star_parser::{format_program, parse, InstructionOpcode, Program, Span, Statement};

const TOKEN_TYPES: [&str; 3] = ["operator", "keyword", "string"];
const BASIC_COMPLETION_ORDER: u8 = 0;
const EXTENDED_COMPLETION_ORDER: u8 = 1;

/// Starts the stdio language server and serves requests until the client exits.
pub fn run() -> Result<(), Box<dyn Error + Send + Sync>> {
    let (connection, io_threads) = Connection::stdio();
    let _client_capabilities = connection.initialize(server_capabilities())?;
    let mut documents = HashMap::<String, Document>::new();
    let mut shutdown = false;

    while let Ok(message) = connection.receiver.recv() {
        match message {
            Message::Notification(notification) => {
                if notification.method == "exit" {
                    break;
                }
                if notification.method == "textDocument/didOpen" {
                    if let Some(item) = notification.params.get("textDocument") {
                        let uri = item.get("uri").and_then(Value::as_str);
                        let text = item.get("text").and_then(Value::as_str);
                        if let (Some(uri), Some(text)) = (uri, text) {
                            let version = item.get("version").and_then(Value::as_i64);
                            let document = Document::new(text.to_owned());
                            publish_diagnostics(&connection, uri, version, &document.diagnostics)?;
                            documents.insert(uri.to_owned(), document);
                        }
                    }
                } else if notification.method == "textDocument/didChange" {
                    if let Some(identifier) = notification.params.get("textDocument") {
                        let uri = identifier.get("uri").and_then(Value::as_str);
                        let version = identifier.get("version").and_then(Value::as_i64);
                        let text = notification
                            .params
                            .get("contentChanges")
                            .and_then(Value::as_array)
                            .and_then(|changes| changes.last())
                            .and_then(|change| change.get("text"))
                            .and_then(Value::as_str);
                        if let (Some(uri), Some(text)) = (uri, text) {
                            let document = Document::new(text.to_owned());
                            publish_diagnostics(&connection, uri, version, &document.diagnostics)?;
                            documents.insert(uri.to_owned(), document);
                        }
                    }
                } else if notification.method == "textDocument/didClose" {
                    if let Some(uri) = notification
                        .params
                        .pointer("/textDocument/uri")
                        .and_then(Value::as_str)
                    {
                        documents.remove(uri);
                        publish_diagnostics(&connection, uri, None, &[])?;
                    }
                }
            }
            Message::Request(request) => {
                if request.method == "shutdown" {
                    shutdown = true;
                    connection
                        .sender
                        .send(Message::Response(Response::new_ok(request.id, Value::Null)))?;
                    continue;
                }
                let result = if shutdown {
                    Err((-32002, "server has shut down".to_owned()))
                } else {
                    handle_request(&request, &documents)
                };
                let response = match result {
                    Ok(value) => Response::new_ok(request.id, value),
                    Err((code, message)) => Response::new_err(request.id, code, message),
                };
                connection.sender.send(Message::Response(response))?;
            }
            Message::Response(_) => {}
        }
    }

    io_threads.join()?;
    Ok(())
}

fn server_capabilities() -> Value {
    json!({
        "positionEncoding": "utf-16",
        "textDocumentSync": 1,
        "completionProvider": { "triggerCharacters": ["*", "'", "\""] },
        "hoverProvider": true,
        "definitionProvider": true,
        "referencesProvider": true,
        "renameProvider": true,
        "codeActionProvider": true,
        "signatureHelpProvider": { "triggerCharacters": ["*", "'"] },
        "documentFormattingProvider": true,
        "documentSymbolProvider": true,
        "foldingRangeProvider": true,
        "inlayHintProvider": true,
        "semanticTokensProvider": {
            "legend": { "tokenTypes": TOKEN_TYPES, "tokenModifiers": [] },
            "full": true,
            "range": false
        }
    })
}

fn publish_diagnostics(
    connection: &Connection,
    uri: &str,
    version: Option<i64>,
    diagnostics: &[Value],
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let mut params = json!({ "uri": uri, "diagnostics": diagnostics });
    if let Some(version) = version {
        params["version"] = json!(version);
    }
    connection
        .sender
        .send(Message::Notification(Notification::new(
            "textDocument/publishDiagnostics".to_owned(),
            params,
        )))?;
    Ok(())
}

fn handle_request(
    request: &Request,
    documents: &HashMap<String, Document>,
) -> Result<Value, (i32, String)> {
    let uri = request
        .params
        .pointer("/textDocument/uri")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let document = documents.get(uri);

    match request.method.as_str() {
        "textDocument/completion" => Ok(completions()),
        "textDocument/hover" => Ok(document
            .and_then(|document| hover(document, &request.params))
            .unwrap_or(Value::Null)),
        "textDocument/semanticTokens/full" => Ok(document
            .map(semantic_tokens)
            .unwrap_or_else(|| json!({ "data": [] }))),
        "textDocument/inlayHint" => Ok(document
            .map(|document| inlay_hints(document, &request.params))
            .unwrap_or_else(|| json!([]))),
        "textDocument/formatting" => Ok(document
            .and_then(format_document)
            .unwrap_or_else(|| json!([]))),
        "textDocument/codeAction" => Ok(document
            .map(|document| code_actions(document, uri))
            .unwrap_or_else(|| json!([]))),
        "textDocument/signatureHelp" => Ok(document
            .and_then(|document| signature_help(document, &request.params))
            .unwrap_or(Value::Null)),
        "textDocument/documentSymbol" => {
            Ok(document.map(document_symbols).unwrap_or_else(|| json!([])))
        }
        "textDocument/foldingRange" => {
            Ok(document.map(folding_ranges).unwrap_or_else(|| json!([])))
        }
        "textDocument/definition" => Ok(document
            .and_then(|document| definition(document, uri, &request.params))
            .unwrap_or(Value::Null)),
        "textDocument/references" => Ok(document
            .map(|document| references(document, uri, &request.params))
            .unwrap_or_else(|| json!([]))),
        "textDocument/rename" => document
            .and_then(|document| rename(document, uri, &request.params))
            .ok_or_else(|| {
                (
                    -32602,
                    "rename requires a directly encoded non-negative numeric ID".to_owned(),
                )
            }),
        _ => Err((
            -32601,
            format!("unsupported LSP method: {}", request.method),
        )),
    }
}

#[derive(Clone, Debug)]
struct SourceToken {
    kind: TokenKind,
    span: Span,
    normalized_start: usize,
    normalized_end: usize,
}

#[derive(Clone, Debug)]
enum TokenKind {
    Instruction(InstructionOpcode),
    Literal(String),
}

#[derive(Clone, Debug)]
struct IdentifierSite {
    id: String,
    span: Span,
    definition: bool,
}

#[derive(Clone, Debug)]
struct Document {
    text: String,
    normalized: Option<NormalizedSource>,
    program: Option<Program>,
    tokens: Vec<SourceToken>,
    identifiers: Vec<IdentifierSite>,
    diagnostics: Vec<Value>,
}

impl Document {
    fn new(text: String) -> Self {
        let mut document = Self {
            text,
            normalized: None,
            program: None,
            tokens: Vec::new(),
            identifiers: Vec::new(),
            diagnostics: Vec::new(),
        };
        let normalized = match normalize_source(document.text.as_bytes(), LexMode::Normal) {
            Ok(normalized) => normalized,
            Err(error) => {
                document.diagnostics.push(diagnostic(
                    span_for_position(error.position),
                    format!("{error}"),
                    "source",
                ));
                return document;
            }
        };
        document.tokens = scan_tokens(&normalized);
        match parse(&normalized) {
            Ok(program) => {
                if let Err(error) = semantic::analyze(&program) {
                    document.diagnostics.push(diagnostic(
                        error.span,
                        format!("{error}"),
                        "semantic",
                    ));
                }
                document.identifiers = collect_identifier_sites(&document.tokens);
                document.program = Some(program);
            }
            Err(error) => {
                document
                    .diagnostics
                    .push(diagnostic(error.span, format!("{error}"), "syntax"))
            }
        }
        document.normalized = Some(normalized);
        document
    }
}

fn diagnostic(span: Span, message: String, code: &str) -> Value {
    json!({
        "range": range_json(span),
        "severity": 1,
        "code": code,
        "source": "star",
        "message": message
    })
}

fn span_for_position(position: star_common::SourcePosition) -> Span {
    Span {
        start: position,
        end: star_common::SourcePosition {
            byte_offset: position.byte_offset.saturating_add(1),
            line: position.line,
            column: position.column.saturating_add(1),
        },
    }
}

fn range_json(span: Span) -> Value {
    json!({
        "start": { "line": span.start.line.saturating_sub(1), "character": span.start.column.saturating_sub(1) },
        "end": { "line": span.end.line.saturating_sub(1), "character": span.end.column.saturating_sub(1) }
    })
}

fn scan_tokens(source: &NormalizedSource) -> Vec<SourceToken> {
    let bytes = source.as_bytes();
    let mut tokens = Vec::new();
    let mut cursor = 0;
    while cursor < bytes.len() {
        if is_source_whitespace(bytes[cursor]) {
            cursor += 1;
            continue;
        }
        let start = cursor;
        if bytes[cursor] == b'*' {
            while cursor < bytes.len() && bytes[cursor] == b'*' {
                cursor += 1;
            }
            if cursor < bytes.len() && bytes[cursor] == b'\'' {
                cursor += 1;
                let count = cursor - start - 1;
                if let Ok(ordinal) = u8::try_from(count) {
                    if let Some(span) = source.span(start, cursor) {
                        tokens.push(SourceToken {
                            kind: TokenKind::Instruction(InstructionOpcode::Basic(ordinal)),
                            span,
                            normalized_start: start,
                            normalized_end: cursor,
                        });
                    }
                }
            }
        } else if bytes[cursor] == b'\'' {
            cursor += 1;
            let payload_start = cursor;
            let mut opcode = 0u8;
            let mut bits = 0usize;
            while cursor < bytes.len()
                && bits < 8
                && (bytes[cursor] == b'*' || bytes[cursor] == b'\'')
            {
                opcode = (opcode << 1) | u8::from(bytes[cursor] == b'*');
                bits += 1;
                cursor += 1;
            }
            if bits == 8 && cursor < bytes.len() && bytes[cursor] == b'"' {
                cursor += 1;
                if let Some(span) = source.span(start, cursor) {
                    tokens.push(SourceToken {
                        kind: TokenKind::Instruction(InstructionOpcode::Extended(opcode)),
                        span,
                        normalized_start: start,
                        normalized_end: cursor,
                    });
                }
            } else {
                cursor = payload_start;
            }
        } else if bytes[cursor] == b'"' {
            cursor += 1;
            let payload_start = cursor;
            while cursor < bytes.len() && bytes[cursor] != b'"' {
                cursor += 1;
            }
            if cursor < bytes.len() {
                let bits = bytes[payload_start..cursor]
                    .iter()
                    .map(|byte| if *byte == b'*' { '1' } else { '0' })
                    .collect();
                cursor += 1;
                if let Some(span) = source.span(start, cursor) {
                    tokens.push(SourceToken {
                        kind: TokenKind::Literal(bits),
                        span,
                        normalized_start: start,
                        normalized_end: cursor,
                    });
                }
            }
        } else {
            cursor += 1;
        }
    }
    tokens
}

fn is_source_whitespace(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\r' | b'\n')
}

fn collect_identifier_sites(tokens: &[SourceToken]) -> Vec<IdentifierSite> {
    let mut sites = Vec::new();
    let mut seen_definitions = HashSet::<String>::new();
    for pair in tokens.windows(2) {
        let (TokenKind::Literal(bits), TokenKind::Instruction(opcode)) =
            (&pair[0].kind, &pair[1].kind)
        else {
            continue;
        };
        let Some(mnemonic) = opcode.mnemonic() else {
            continue;
        };
        if !matches!(
            mnemonic,
            "DECLARE" | "CONST" | "DELETE" | "LOAD" | "STORE" | "LOAD_GLOBAL" | "STORE_GLOBAL"
        ) {
            continue;
        }
        let canonical = bits.trim_start_matches('0');
        let id = if canonical.is_empty() { "0" } else { canonical };
        let id = id.to_owned();
        let definition = match mnemonic {
            "DECLARE" | "CONST" => {
                seen_definitions.insert(id.clone());
                true
            }
            "STORE_GLOBAL" => seen_definitions.insert(id.clone()),
            _ => false,
        };
        sites.push(IdentifierSite {
            id,
            span: pair[0].span,
            definition,
        });
    }
    sites
}

fn token_at<'a>(document: &'a Document, params: &Value) -> Option<&'a SourceToken> {
    let line = params.pointer("/position/line")?.as_u64()? as usize;
    let character = params.pointer("/position/character")?.as_u64()? as usize;
    document.tokens.iter().find(|token| {
        let start_line = token.span.start.line.saturating_sub(1);
        let end_line = token.span.end.line.saturating_sub(1);
        let start_column = token.span.start.column.saturating_sub(1);
        let end_column = token.span.end.column.saturating_sub(1);
        (line > start_line || (line == start_line && character >= start_column))
            && (line < end_line || (line == end_line && character < end_column))
    })
}

fn hover(document: &Document, params: &Value) -> Option<Value> {
    let token = token_at(document, params)?;
    match &token.kind {
        TokenKind::Instruction(opcode) => {
            let mnemonic = opcode.mnemonic()?;
            let spelling = opcode.source_spelling()?;
            Some(json!({
                "contents": { "kind": "markdown", "value": format!("**{mnemonic}**\n\nSource token: `{spelling}`") },
                "range": range_json(token.span)
            }))
        }
        TokenKind::Literal(bits) => Some(json!({
            "contents": { "kind": "markdown", "value": format!("**Bit literal**\n\n{} payload bits (most-significant-bit first).", bits.len()) },
            "range": range_json(token.span)
        })),
    }
}

fn completions() -> Value {
    let items: Vec<Value> = InstructionOpcode::assigned_opcodes()
        .enumerate()
        .filter_map(|(index, opcode)| {
            let mnemonic = opcode.mnemonic()?;
            let spelling = opcode.source_spelling()?;
            let order = match opcode {
                InstructionOpcode::Basic(_) => BASIC_COMPLETION_ORDER,
                InstructionOpcode::Extended(_) => EXTENDED_COMPLETION_ORDER,
            };
            Some(json!({
                "label": mnemonic,
                "kind": 3,
                "detail": format!("Source token: {spelling}"),
                "insertText": spelling,
                "sortText": format!("{order}{index:03}")
            }))
        })
        .collect();
    json!({ "isIncomplete": false, "items": items })
}

fn semantic_tokens(document: &Document) -> Value {
    let Some(normalized) = &document.normalized else {
        return json!({ "data": [] });
    };
    let mut encoded = Vec::<(usize, usize, usize, u32, u32)>::new();
    for token in &document.tokens {
        let (token_type, include_whitespace) = match &token.kind {
            TokenKind::Instruction(opcode) => {
                let structural = opcode.mnemonic().is_some_and(|mnemonic| {
                    matches!(
                        mnemonic,
                        "IF" | "ELSE"
                            | "END"
                            | "LOOP"
                            | "WHILE"
                            | "FOR"
                            | "FUNC"
                            | "ASYNC_FUNC"
                            | "SCOPE_BEGIN"
                            | "SCOPE_END"
                            | "TRY"
                            | "CATCH"
                            | "FINALLY"
                    )
                });
                (if structural { 1 } else { 0 }, false)
            }
            TokenKind::Literal(_) => (2, true),
        };
        let mut start: Option<(usize, usize)> = None;
        let mut previous: Option<(usize, usize)> = None;
        for offset in token.normalized_start..token.normalized_end {
            if !include_whitespace && is_source_whitespace(normalized.as_bytes()[offset]) {
                continue;
            }
            let Some(position) = normalized.position_at(offset) else {
                continue;
            };
            let current = (
                position.line.saturating_sub(1),
                position.column.saturating_sub(1),
            );
            if let (Some(begin), Some(last)) = (start, previous) {
                if current.0 != last.0 || current.1 != last.1 + 1 {
                    encoded.push((begin.0, begin.1, last.1 - begin.1 + 1, token_type, 0));
                    start = Some(current);
                }
            } else {
                start = Some(current);
            }
            previous = Some(current);
        }
        if let (Some(begin), Some(last)) = (start, previous) {
            encoded.push((begin.0, begin.1, last.1 - begin.1 + 1, token_type, 0));
        }
    }
    encoded.sort_unstable_by_key(|token| (token.0, token.1));
    let mut data = Vec::with_capacity(encoded.len() * 5);
    let mut previous_line = 0usize;
    let mut previous_start = 0usize;
    for (line, start, length, token_type, modifiers) in encoded {
        let delta_line = line - previous_line;
        let delta_start = if delta_line == 0 {
            start - previous_start
        } else {
            start
        };
        let (Ok(delta_line), Ok(delta_start), Ok(length)) = (
            u32::try_from(delta_line),
            u32::try_from(delta_start),
            u32::try_from(length),
        ) else {
            continue;
        };
        data.extend([delta_line, delta_start, length, token_type, modifiers]);
        previous_line = line;
        previous_start = start;
    }
    json!({ "resultId": "star-v1", "data": data })
}

fn inlay_hints(document: &Document, params: &Value) -> Value {
    let range = params.get("range");
    let hints: Vec<Value> = document
        .tokens
        .iter()
        .filter_map(|token| {
            let TokenKind::Instruction(opcode) = token.kind else {
                return None;
            };
            let mnemonic = opcode.mnemonic()?;
            let position = json!({
                "line": token.span.end.line.saturating_sub(1),
                "character": token.span.end.column.saturating_sub(1)
            });
            if !position_in_range(&position, range) {
                return None;
            }
            Some(json!({
                "position": position,
                "label": format!(" {mnemonic}"),
                "kind": 1,
                "paddingLeft": true,
                "tooltip": format!("Star instruction `{mnemonic}`; source bytes are unchanged.")
            }))
        })
        .collect();
    json!(hints)
}

fn position_in_range(position: &Value, range: Option<&Value>) -> bool {
    let Some(range) = range else {
        return true;
    };
    let Some(line) = position.get("line").and_then(Value::as_u64) else {
        return false;
    };
    let Some(character) = position.get("character").and_then(Value::as_u64) else {
        return false;
    };
    let start = (
        range
            .pointer("/start/line")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        range
            .pointer("/start/character")
            .and_then(Value::as_u64)
            .unwrap_or(0),
    );
    let end = (
        range
            .pointer("/end/line")
            .and_then(Value::as_u64)
            .unwrap_or(u64::MAX),
        range
            .pointer("/end/character")
            .and_then(Value::as_u64)
            .unwrap_or(u64::MAX),
    );
    (line, character) >= start && (line, character) <= end
}

fn format_document(document: &Document) -> Option<Value> {
    let program = document.program.as_ref()?;
    let formatted = format_program(program).ok()?;
    Some(json!([{
        "range": full_document_range(&document.text),
        "newText": formatted
    }]))
}

fn code_actions(document: &Document, uri: &str) -> Value {
    let Some(program) = &document.program else {
        return json!([]);
    };
    let Ok(formatted) = format_program(program) else {
        return json!([]);
    };
    if formatted == document.text {
        return json!([]);
    }
    let mut changes = serde_json::Map::new();
    changes.insert(
        uri.to_owned(),
        json!([{ "range": full_document_range(&document.text), "newText": formatted }]),
    );
    json!([{
        "title": "Format Star document",
        "kind": "source",
        "edit": { "changes": changes }
    }])
}

fn signature_help(document: &Document, params: &Value) -> Option<Value> {
    let token = token_at(document, params).or_else(|| token_ending_at(document, params))?;
    let TokenKind::Instruction(opcode) = token.kind else {
        return None;
    };
    let mnemonic = opcode.mnemonic()?;
    let (label, documentation, parameters): (&str, &str, &[(&str, &str)]) = match mnemonic {
        "CALL" => (
            "CALL(arg1..argN, argc, function_id) -> result",
            "Stack, bottom to top: `arg1, ..., argN, argc, function_id`. `argc` is a non-negative Int matching the argument values. See `spec/function-abi.md`.",
            &[
                ("arg1..argN", "Argument values in left-to-right parameter order."),
                ("argc", "Non-negative Int equal to the number of arguments."),
                ("function_id", "Numeric ID of a function in the current module."),
            ],
        ),
        "TAIL_CALL" => (
            "TAIL_CALL(arg1..argN, argc, function_id) -> caller result",
            "Uses the CALL stack order and replaces the current frame. The tail call must directly provide the caller's result.",
            &[
                ("arg1..argN", "Argument values in left-to-right parameter order."),
                ("argc", "Non-negative Int equal to the number of arguments."),
                ("function_id", "Numeric ID of a function in the current module."),
            ],
        ),
        "CALL_VALUE" => (
            "CALL_VALUE(arg1..argN, argc, callable) -> result",
            "Stack, bottom to top: `arg1, ..., argN, argc, callable`. The callable is a Function, Closure, or NativeFunction.",
            &[
                ("arg1..argN", "Argument values in left-to-right parameter order."),
                ("argc", "Non-negative Int equal to the number of arguments."),
                ("callable", "Function, Closure, or NativeFunction value."),
            ],
        ),
        "MODULE_CALL" => (
            "MODULE_CALL(arg1..argN, argc, module, symbol_id) -> result",
            "Calls an exported function. Stack, bottom to top: arguments, argc, module handle, symbol ID.",
            &[
                ("arg1..argN", "Argument values in left-to-right parameter order."),
                ("argc", "Non-negative Int equal to the number of arguments."),
                ("module", "Initialized module handle."),
                ("symbol_id", "Numeric ID of the exported function."),
            ],
        ),
        "METHOD_CALL" => (
            "METHOD_CALL(arg1..argN, argc, object, method_id) -> result",
            "Calls a method with the object as receiver. Stack, bottom to top: arguments, argc, object, method ID.",
            &[
                ("arg1..argN", "Argument values in left-to-right parameter order."),
                ("argc", "Non-negative Int equal to the number of arguments."),
                ("object", "Method receiver."),
                ("method_id", "Numeric method ID."),
            ],
        ),
        _ => return None,
    };
    let parameters: Vec<Value> = parameters
        .iter()
        .map(|(name, detail)| json!({ "label": name, "documentation": detail }))
        .collect();
    Some(json!({
        "signatures": [{
            "label": label,
            "documentation": { "kind": "markdown", "value": documentation },
            "parameters": parameters
        }],
        "activeSignature": 0
    }))
}

fn token_ending_at<'a>(document: &'a Document, params: &Value) -> Option<&'a SourceToken> {
    let line = params.pointer("/position/line")?.as_u64()? as usize;
    let character = params.pointer("/position/character")?.as_u64()? as usize;
    document.tokens.iter().find(|token| {
        token.span.end.line.saturating_sub(1) == line
            && token.span.end.column.saturating_sub(1) == character
    })
}

fn full_document_range(text: &str) -> Value {
    let mut line = 0u32;
    let mut character = 0u32;
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                line = line.saturating_add(1);
                character = 0;
            }
            '\n' => {
                line = line.saturating_add(1);
                character = 0;
            }
            _ => character = character.saturating_add(ch.len_utf16() as u32),
        }
    }
    json!({ "start": { "line": 0, "character": 0 }, "end": { "line": line, "character": character } })
}

fn document_symbols(document: &Document) -> Value {
    let Some(program) = &document.program else {
        return json!([]);
    };
    let mut symbols = Vec::new();
    collect_symbols(&program.statements, &mut symbols);
    json!(symbols)
}

fn collect_symbols(statements: &[Statement], output: &mut Vec<Value>) {
    for statement in statements {
        let (name, kind, span, children): (&str, u8, Span, Vec<&[Statement]>) = match statement {
            Statement::If(block) => {
                let mut children = vec![&block.then_body[..]];
                if let Some(else_body) = &block.else_body {
                    children.push(else_body);
                }
                ("IF", 3, block.span, children)
            }
            Statement::Loop(block) => {
                let mut children = vec![&block.body[..]];
                if let Some(condition) = &block.condition {
                    children.push(condition);
                }
                ("LOOP", 3, block.span, children)
            }
            Statement::For(block) => ("FOR", 3, block.span, vec![&block.body]),
            Statement::Function(block) => (
                if block.kind == star_parser::FunctionKind::Asynchronous {
                    "ASYNC_FUNC"
                } else {
                    "FUNC"
                },
                12,
                block.span,
                vec![&block.body],
            ),
            Statement::Scope(block) => ("SCOPE", 3, block.span, vec![&block.body]),
            Statement::Try(block) => {
                let mut children = vec![&block.try_body[..]];
                if let Some(catch_body) = &block.catch_body {
                    children.push(catch_body);
                }
                if let Some(finally_body) = &block.finally_body {
                    children.push(finally_body);
                }
                ("TRY", 3, block.span, children)
            }
            _ => continue,
        };
        let range = range_json(span);
        output.push(json!({
            "name": format!("{name} block"),
            "kind": kind,
            "range": range,
            "selectionRange": range
        }));
        for child in children {
            collect_symbols(child, output);
        }
    }
}

fn folding_ranges(document: &Document) -> Value {
    let Some(program) = &document.program else {
        return json!([]);
    };
    let mut ranges = Vec::new();
    collect_folds(&program.statements, &mut ranges);
    json!(ranges)
}

fn collect_folds(statements: &[Statement], output: &mut Vec<Value>) {
    for statement in statements {
        let (span, children): (Span, Vec<&[Statement]>) = match statement {
            Statement::If(block) => {
                let mut children = vec![&block.then_body[..]];
                if let Some(else_body) = &block.else_body {
                    children.push(else_body);
                }
                (block.span, children)
            }
            Statement::Loop(block) => {
                let mut children = vec![&block.body[..]];
                if let Some(condition) = &block.condition {
                    children.push(condition);
                }
                (block.span, children)
            }
            Statement::For(block) => (block.span, vec![&block.body]),
            Statement::Function(block) => (block.span, vec![&block.body]),
            Statement::Scope(block) => (block.span, vec![&block.body]),
            Statement::Try(block) => {
                let mut children = vec![&block.try_body[..]];
                if let Some(catch_body) = &block.catch_body {
                    children.push(catch_body);
                }
                if let Some(finally_body) = &block.finally_body {
                    children.push(finally_body);
                }
                (block.span, children)
            }
            _ => continue,
        };
        let start = span.start.line.saturating_sub(1);
        let end = span.end.line.saturating_sub(1);
        if end > start {
            output.push(json!({ "startLine": start, "endLine": end, "kind": "region" }));
        }
        for child in children {
            collect_folds(child, output);
        }
    }
}

fn site_at<'a>(document: &'a Document, params: &Value) -> Option<&'a IdentifierSite> {
    let line = params.pointer("/position/line")?.as_u64()? as usize;
    let character = params.pointer("/position/character")?.as_u64()? as usize;
    document.identifiers.iter().find(|site| {
        let start_line = site.span.start.line.saturating_sub(1);
        let end_line = site.span.end.line.saturating_sub(1);
        let start_character = site.span.start.column.saturating_sub(1);
        let end_character = site.span.end.column.saturating_sub(1);
        (line > start_line || (line == start_line && character >= start_character))
            && (line < end_line || (line == end_line && character < end_character))
    })
}

fn definition(document: &Document, uri: &str, params: &Value) -> Option<Value> {
    let site = site_at(document, params)?;
    let definitions: Vec<&IdentifierSite> = document
        .identifiers
        .iter()
        .filter(|candidate| candidate.id == site.id && candidate.definition)
        .collect();
    (definitions.len() == 1).then(|| location(uri, definitions[0].span))
}

fn references(document: &Document, uri: &str, params: &Value) -> Value {
    let Some(site) = site_at(document, params) else {
        return json!([]);
    };
    if document
        .identifiers
        .iter()
        .filter(|candidate| candidate.id == site.id && candidate.definition)
        .count()
        != 1
    {
        return json!([]);
    }
    let include_declaration = params
        .pointer("/context/includeDeclaration")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let locations: Vec<Value> = document
        .identifiers
        .iter()
        .filter(|candidate| {
            candidate.id == site.id && (include_declaration || !candidate.definition)
        })
        .map(|candidate| location(uri, candidate.span))
        .collect();
    json!(locations)
}

fn rename(document: &Document, uri: &str, params: &Value) -> Option<Value> {
    let site = site_at(document, params)?;
    if document
        .identifiers
        .iter()
        .filter(|candidate| candidate.id == site.id && candidate.definition)
        .count()
        != 1
    {
        return None;
    }
    let new_name = params.get("newName")?.as_str()?;
    let value = BigUint::parse_bytes(new_name.as_bytes(), 10)?;
    let bits = if value.is_zero() {
        "0".to_owned()
    } else {
        value.to_str_radix(2)
    };
    let replacement = format!(
        "\"{}\"",
        bits.chars()
            .map(|bit| if bit == '1' { '*' } else { '\'' })
            .collect::<String>()
    );
    let changes: Vec<Value> = document
        .identifiers
        .iter()
        .filter(|candidate| candidate.id == site.id)
        .map(|candidate| json!({ "range": range_json(candidate.span), "newText": replacement }))
        .collect();
    let mut changes_by_uri = serde_json::Map::new();
    changes_by_uri.insert(uri.to_owned(), json!(changes));
    Some(json!({ "changes": changes_by_uri }))
}

fn location(uri: &str, span: Span) -> Value {
    json!({ "uri": uri, "range": range_json(span) })
}
