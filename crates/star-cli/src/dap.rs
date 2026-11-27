use std::collections::BTreeSet;
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use star_interpreter::runtime::{
    DebugBinding, DebugBreakpoint, DebugCommand, DebugController, DebugSnapshot, DebugStopReason,
};

const MAX_DAP_MESSAGE_BYTES: usize = 1024 * 1024;
const MAX_DAP_HEADER_BYTES: usize = 8 * 1024;

#[derive(Clone)]
pub(crate) struct DapOutput {
    stdout: Arc<Mutex<io::Stdout>>,
    sequence: Arc<AtomicU64>,
}

impl DapOutput {
    pub(crate) fn new() -> Self {
        Self {
            stdout: Arc::new(Mutex::new(io::stdout())),
            sequence: Arc::new(AtomicU64::new(1)),
        }
    }

    fn send(&self, mut message: Value) -> io::Result<()> {
        let Some(object) = message.as_object_mut() else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "DAP message must be a JSON object",
            ));
        };
        object.insert(
            "seq".to_owned(),
            json!(self.sequence.fetch_add(1, Ordering::Relaxed)),
        );
        let payload = serde_json::to_vec(&message)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let mut stdout = self
            .stdout
            .lock()
            .map_err(|_| io::Error::other("DAP output lock poisoned"))?;
        write!(stdout, "Content-Length: {}\r\n\r\n", payload.len())?;
        stdout.write_all(&payload)?;
        stdout.flush()
    }

    fn response(
        &self,
        request: &Value,
        success: bool,
        body: Value,
        message: Option<&str>,
    ) -> io::Result<()> {
        let mut response = json!({
            "type": "response",
            "request_seq": request.get("seq").and_then(Value::as_u64).unwrap_or(0),
            "command": request.get("command").and_then(Value::as_str).unwrap_or(""),
            "success": success,
            "body": body,
        });
        if let Some(message) = message {
            response["message"] = json!(message);
        }
        self.send(response)
    }

    fn event(&self, event: &str, body: Value) -> io::Result<()> {
        self.send(json!({"type":"event", "event":event, "body":body}))
    }

    pub(crate) fn finish(&self, exit_code: i32) -> io::Result<()> {
        self.event("exited", json!({"exitCode":exit_code}))?;
        self.event("terminated", json!({}))
    }
}

pub(crate) struct DapGuestOutput {
    output: DapOutput,
    pending_utf8: Vec<u8>,
}

impl DapGuestOutput {
    pub(crate) fn new(output: DapOutput) -> Self {
        Self {
            output,
            pending_utf8: Vec::new(),
        }
    }

    fn send_text(&self, text: &str) -> io::Result<()> {
        if !text.is_empty() {
            self.output
                .event("output", json!({"category":"stdout", "output":text}))?;
        }
        Ok(())
    }
}

impl Write for DapGuestOutput {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        let mut combined = std::mem::take(&mut self.pending_utf8);
        combined.extend_from_slice(bytes);
        match std::str::from_utf8(&combined) {
            Ok(text) => self.send_text(text)?,
            Err(error) => {
                let valid_up_to = error.valid_up_to();
                self.send_text(
                    std::str::from_utf8(&combined[..valid_up_to])
                        .expect("valid UTF-8 prefix was reported by the decoder"),
                )?;
                if error.error_len().is_none() {
                    self.pending_utf8
                        .extend_from_slice(&combined[valid_up_to..]);
                } else {
                    self.send_text(&String::from_utf8_lossy(&combined[valid_up_to..]))?;
                }
            }
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if !self.pending_utf8.is_empty() {
            let pending = std::mem::take(&mut self.pending_utf8);
            self.send_text(&String::from_utf8_lossy(&pending))?;
        }
        self.output
            .stdout
            .lock()
            .map_err(|_| io::Error::other("DAP output lock poisoned"))?
            .flush()
    }
}

pub(crate) struct DapController {
    stdin: BufReader<io::Stdin>,
    output: DapOutput,
    source_path: PathBuf,
    breakpoints: Vec<DebugBreakpoint>,
    valid_locations: BTreeSet<(usize, usize)>,
    initialized: bool,
    launched: bool,
}

impl DapController {
    pub(crate) fn new(
        source_path: &Path,
        output: DapOutput,
        valid_locations: BTreeSet<(usize, usize)>,
    ) -> io::Result<Self> {
        Ok(Self {
            stdin: BufReader::new(io::stdin()),
            output,
            source_path: source_path.canonicalize()?,
            breakpoints: Vec::new(),
            valid_locations,
            initialized: false,
            launched: false,
        })
    }

    pub(crate) fn configure(&mut self) -> io::Result<Vec<DebugBreakpoint>> {
        loop {
            let request = read_message(&mut self.stdin)?.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "DAP client closed before configuration",
                )
            })?;
            if request.get("type").and_then(Value::as_str) != Some("request") {
                continue;
            }
            match request.get("command").and_then(Value::as_str).unwrap_or("") {
                "initialize" => {
                    self.initialized = true;
                    self.output.response(
                        &request,
                        true,
                        json!({
                            "supportsConfigurationDoneRequest":true,
                            "supportsTerminateRequest":true,
                            "supportsConditionalBreakpoints":false,
                            "supportsFunctionBreakpoints":false,
                            "supportsStepBack":false,
                        }),
                        None,
                    )?;
                    self.output.event("initialized", json!({}))?;
                }
                "launch" => {
                    if !self.initialized {
                        self.output.response(
                            &request,
                            false,
                            json!({}),
                            Some("send initialize before launch"),
                        )?;
                        continue;
                    }
                    if let Some(program) = request
                        .get("arguments")
                        .and_then(|arguments| arguments.get("program"))
                        .and_then(Value::as_str)
                    {
                        let requested = Path::new(program)
                            .canonicalize()
                            .unwrap_or_else(|_| PathBuf::from(program));
                        if requested != self.source_path {
                            self.output.response(
                                &request,
                                false,
                                json!({}),
                                Some("launch program differs from the source file passed to `star debug --dap`"),
                            )?;
                            continue;
                        }
                    }
                    self.launched = true;
                    self.output.response(&request, true, json!({}), None)?;
                }
                "setBreakpoints" => self.set_breakpoints(&request)?,
                "configurationDone" => {
                    if !self.launched {
                        self.output.response(
                            &request,
                            false,
                            json!({}),
                            Some("send launch before configurationDone"),
                        )?;
                        continue;
                    }
                    self.output.response(&request, true, json!({}), None)?;
                    return Ok(self.breakpoints.clone());
                }
                "disconnect" | "terminate" => {
                    self.output.response(&request, true, json!({}), None)?;
                    return Err(io::Error::new(
                        io::ErrorKind::Interrupted,
                        "DAP client ended the debug session before execution",
                    ));
                }
                command => self.output.response(
                    &request,
                    false,
                    json!({}),
                    Some(&format!(
                        "unsupported DAP configuration request `{command}`"
                    )),
                )?,
            }
        }
    }

    fn set_breakpoints(&mut self, request: &Value) -> io::Result<()> {
        let source = request
            .get("arguments")
            .and_then(|value| value.get("source"));
        let path = source
            .and_then(|value| value.get("path"))
            .and_then(Value::as_str)
            .map(Path::new);
        let path_matches = path
            .and_then(|path| path.canonicalize().ok())
            .is_some_and(|path| path == self.source_path);
        let requested = request
            .get("arguments")
            .and_then(|value| value.get("breakpoints"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        self.breakpoints.clear();
        let mut responses = Vec::with_capacity(requested.len());
        for (index, breakpoint) in requested.iter().enumerate() {
            let line = breakpoint
                .get("line")
                .and_then(Value::as_u64)
                .and_then(|line| usize::try_from(line).ok());
            let column = breakpoint
                .get("column")
                .and_then(Value::as_u64)
                .and_then(|column| usize::try_from(column).ok());
            let location_matches = line.is_some_and(|line| match column {
                Some(column) => self.valid_locations.contains(&(line, column)),
                None => self
                    .valid_locations
                    .iter()
                    .any(|(valid_line, _)| *valid_line == line),
            });
            let verified = path_matches
                && line.is_some_and(|line| line > 0)
                && column.map_or(true, |column| column > 0)
                && location_matches;
            if verified {
                self.breakpoints.push(DebugBreakpoint {
                    line: line.unwrap(),
                    column,
                });
            }
            let mut response = json!({
                "id":index + 1,
                "verified":verified,
                "line":line.filter(|line| *line > 0).unwrap_or(1),
            });
            if let Some(column) = column {
                response["column"] = json!(column);
            }
            if !path_matches {
                response["message"] =
                    json!("breakpoint source path does not match the launched file");
            } else if !verified {
                response["message"] =
                    json!("breakpoint line and optional column must be positive integers");
            }
            responses.push(response);
        }
        self.output
            .response(request, true, json!({"breakpoints":responses}), None)
    }

    fn handle_paused_request(
        &mut self,
        request: &Value,
        snapshot: &DebugSnapshot,
    ) -> io::Result<Option<DebugCommand>> {
        let command = request.get("command").and_then(Value::as_str).unwrap_or("");
        match command {
            "threads" => self.output.response(
                request,
                true,
                json!({"threads":[{"id":1,"name":"Star source interpreter"}]}),
                None,
            )?,
            "stackTrace" => self.output.response(
                request,
                true,
                json!({"stackFrames":dap_stack_frames(snapshot),"totalFrames":snapshot.call_stack.len()}),
                None,
            )?,
            "scopes" => {
                let frame_id = request
                    .get("arguments")
                    .and_then(|value| value.get("frameId"))
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let frame_is_valid = frame_id > 0 && frame_id <= snapshot.call_stack.len() as u64;
                let scopes = if frame_id == 1 {
                    vec![
                        json!({"name":"Locals","variablesReference":1,"expensive":false,"namedVariables":snapshot.locals.len()}),
                        json!({"name":"Globals","variablesReference":2,"expensive":false,"namedVariables":snapshot.globals.len()}),
                        json!({"name":"Operand Stack","variablesReference":3,"expensive":false,"namedVariables":snapshot.operand_stack.len()}),
                    ]
                } else if frame_is_valid {
                    vec![json!({"name":"Globals","variablesReference":2,"expensive":false,"namedVariables":snapshot.globals.len()})]
                } else {
                    Vec::new()
                };
                self.output
                    .response(request, true, json!({"scopes":scopes}), None)?;
            }
            "variables" => {
                let reference = request
                    .get("arguments")
                    .and_then(|value| value.get("variablesReference"))
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let variables = dap_variables(snapshot, reference);
                self.output
                    .response(request, true, json!({"variables":variables}), None)?;
            }
            "continue" => {
                self.output.response(
                    request,
                    true,
                    json!({"allThreadsContinued":true}),
                    None,
                )?;
                return Ok(Some(DebugCommand::Continue));
            }
            "next" => {
                self.output.response(request, true, json!({}), None)?;
                return Ok(Some(DebugCommand::StepOver));
            }
            "stepIn" => {
                self.output.response(request, true, json!({}), None)?;
                return Ok(Some(DebugCommand::StepInto));
            }
            "stepOut" => {
                self.output.response(request, true, json!({}), None)?;
                return Ok(Some(DebugCommand::StepOut));
            }
            "disconnect" | "terminate" => {
                self.output.response(request, true, json!({}), None)?;
                return Ok(Some(DebugCommand::Quit));
            }
            "setBreakpoints" => self.output.response(
                request,
                false,
                json!({}),
                Some("breakpoints are fixed after configurationDone"),
            )?,
            other => self.output.response(
                request,
                false,
                json!({}),
                Some(&format!("unsupported DAP request `{other}`")),
            )?,
        }
        Ok(None)
    }
}

impl DebugController for DapController {
    fn on_stop(&mut self, reason: DebugStopReason, snapshot: &DebugSnapshot) -> DebugCommand {
        let reason_name = match reason {
            DebugStopReason::Entry => "entry",
            DebugStopReason::Breakpoint => "breakpoint",
            DebugStopReason::Step => "step",
        };
        if self
            .output
            .event(
                "stopped",
                json!({
                    "reason":reason_name,
                    "threadId":1,
                    "allThreadsStopped":true,
                    "text":format!("{}:{}:{}", snapshot.source_name, snapshot.location.start.line, snapshot.location.start.column),
                }),
            )
            .is_err()
        {
            return DebugCommand::Quit;
        }
        loop {
            let request = match read_message(&mut self.stdin) {
                Ok(Some(request)) => request,
                Ok(None) | Err(_) => return DebugCommand::Quit,
            };
            if request.get("type").and_then(Value::as_str) != Some("request") {
                continue;
            }
            match self.handle_paused_request(&request, snapshot) {
                Ok(Some(command)) => return command,
                Ok(None) => {}
                Err(_) => return DebugCommand::Quit,
            }
        }
    }
}

fn read_message(reader: &mut impl BufRead) -> io::Result<Option<Value>> {
    let mut content_length = None;
    let mut header_bytes = 0usize;
    loop {
        let Some(line) = read_header_line(reader, &mut header_bytes)? else {
            return Ok(None);
        };
        if line == "\r\n" || line == "\n" {
            break;
        }
        if let Some((name, value)) = line.trim_end_matches(['\r', '\n']).split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                if content_length.is_some() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "duplicate DAP Content-Length",
                    ));
                }
                content_length = Some(value.trim().parse::<usize>().map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidData, "invalid DAP Content-Length")
                })?);
            }
        }
    }
    let length = content_length.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "DAP message has no Content-Length",
        )
    })?;
    if length > MAX_DAP_MESSAGE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "DAP message exceeds 1 MiB",
        ));
    }
    let mut payload = vec![0; length];
    reader.read_exact(&mut payload)?;
    serde_json::from_slice(&payload)
        .map(Some)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn read_header_line(reader: &mut impl BufRead, total: &mut usize) -> io::Result<Option<String>> {
    let mut line = Vec::new();
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return if line.is_empty() && *total == 0 {
                Ok(None)
            } else {
                Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "truncated DAP header",
                ))
            };
        }
        let take = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |index| index + 1);
        let next_total = total
            .checked_add(take)
            .filter(|length| *length <= MAX_DAP_HEADER_BYTES)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "DAP headers exceed 8 KiB")
            })?;
        line.extend_from_slice(&available[..take]);
        reader.consume(take);
        *total = next_total;
        if line.last() == Some(&b'\n') {
            return String::from_utf8(line)
                .map(Some)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error));
        }
    }
}

fn dap_stack_frames(snapshot: &DebugSnapshot) -> Vec<Value> {
    let mut frames = Vec::with_capacity(snapshot.call_stack.len());
    for (dap_index, frame_index) in (0..snapshot.call_stack.len()).rev().enumerate() {
        let frame = &snapshot.call_stack[frame_index];
        let location = if dap_index == 0 {
            Some(snapshot.location)
        } else {
            snapshot.call_stack[frame_index + 1].call_site
        }
        .unwrap_or(snapshot.location);
        let name = frame
            .function_id
            .as_ref()
            .map(|id| format!("function {id}"))
            .unwrap_or_else(|| "top-level".to_owned());
        frames.push(json!({
            "id":dap_index + 1,
            "name":name,
            "line":location.start.line,
            "column":location.start.column,
            "source":{"name":snapshot.source_name,"path":snapshot.source_name},
        }));
    }
    frames
}

fn dap_variables(snapshot: &DebugSnapshot, reference: u64) -> Vec<Value> {
    match reference {
        1 => snapshot.locals.iter().map(binding_variable).collect(),
        2 => snapshot.globals.iter().map(binding_variable).collect(),
        3 => snapshot
            .operand_stack
            .iter()
            .enumerate()
            .map(|(index, value)| {
                json!({"name":index.to_string(),"value":value.preview,"type":value.type_name,"variablesReference":0})
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn binding_variable(binding: &DebugBinding) -> Value {
    json!({
        "name":binding.id,
        "value":binding.value.preview,
        "type":binding.value.type_name,
        "variablesReference":0,
    })
}
