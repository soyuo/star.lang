use std::collections::BTreeSet;
use std::collections::HashSet;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, BufReader, Cursor, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

mod dap;
mod package_commands;
mod source_lowering;
mod source_tools;
mod standalone;

use star_compiler::{
    compile, compile_modules, disassemble_bytecode, verify_bytecode, Capability, CompileModule,
    ModuleIr, VerificationLimits, VerificationPolicy,
};
use star_interpreter::{
    runtime::{
        source_breakpoint_locations, stdio::InputError, BytecodeVm, DebugBinding, DebugBreakpoint,
        DebugCommand, DebugController, DebugExecution, DebugOptions, DebugSnapshot,
        DebugStopReason, ExecutionLimits, Interpreter, RuntimeError, RuntimeErrorKind,
    },
    semantic::analyze,
};
use star_lexer::{normalize_source, LexMode};
use star_package::{locate_manifest, Manifest, Permissions, ResolvedPackage};
use star_parser::{parse, ParseErrorKind};

const EXIT_OK: u8 = 0;
const EXIT_USAGE: u8 = 2;
const EXIT_IO: u8 = 3;
const EXIT_SOURCE: u8 = 4;
const EXIT_RUNTIME: u8 = 5;
const EXIT_LIMIT: u8 = 6;
const EXIT_TEST_FAILURE: u8 = 7;
const DEFAULT_FILE_BYTES: usize = 64 * 1024 * 1024;
const DEFAULT_REPL_LINE_BYTES: usize = 1024 * 1024;

const HELP: &str = "\
Star command line

Usage:
  star new <directory>
  star init [package-name]
  star add <name> [--version <requirement>|--path <directory>|--git <url> --rev <commit>]
  star remove <name>
  star install | update | search <query> | package | publish | audit
  star check [<file.star|file.starc>] [options]
  star build [<file.star>] [--target bytecode|native] [--release] [--output <file>] [options]
  star run [<file.star|file.starc>] [options]
  star debug <file.star> [--strict] [--break LINE[:COLUMN]]... [--input <file>] [--dap]
  star test --suite oj [--solutions <directory>]
  star bench [<file.star|file.starc>] --iterations <count> [--input <file>]
  star fmt <file.star> [--write]
  star minify <file.star> [--write]
  star encode (--int N | --float N | --string TEXT | --file PATH | --opcode NAME)
  star decode <encoded-source.star>
  star disasm <file.starc>
  star lsp
  star repl
  star --version
  star help

Execution options:
  --strict                    Reject formatting whitespace in source
  --max-memory <bytes|K|M|G>  Guest memory ceiling (default: 256M)
  --max-stack <count>         Operand stack ceiling (default: 65536)
  --max-call-depth <count>    Run/bench call depth ceiling (default: 512)
  --max-instructions <count>  Instruction ceiling (default: 10000000)
  --timeout <duration>        Run/bench deadline, e.g. 30s or 500ms (default: 30s)
  --stats                     Run only: write timing and guest-memory estimate to stderr

`check` validates source semantics and bytecode generation, or verifies `.starc`.
`build` defaults to `.starc`; `--target native --release` bundles source bytecode
with the release executable for the current host platform. Source
`TRY/CATCH/FINALLY` runs through `star run` but is not yet lowered to bytecode,
so `build` reports a lowering diagnostic for it. `test --suite oj` requires
paired solutions and Python 3.11+.
`bench` launches a fresh process per iteration and suppresses guest stdout.
`fmt` and `minify` print validated source transformations; `--write` replaces
the input file. `encode` emits valid source and `decode` reads one encoded value.

Standard input and output are streamed without tokenization by the CLI. The
program controls input reads; output bytes and whitespace are preserved.

Stable exit codes:
  0 success; 2 usage/options; 3 file I/O; 4 source or bytecode diagnostics;
  5 runtime failure; 6 configured resource limit; 7 OJ fixture failure.
";

struct RunOptions {
    path: PathBuf,
    strict: bool,
    stats: bool,
    limits: ExecutionLimits,
}

struct BuildOptions {
    path: PathBuf,
    output: Option<PathBuf>,
    target: BuildTarget,
    strict: bool,
    limits: ExecutionLimits,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BuildTarget {
    Bytecode,
    Native,
}

struct TestOptions {
    solutions: Option<PathBuf>,
}

struct BenchOptions {
    execution: RunOptions,
    iterations: usize,
    input: Option<PathBuf>,
}

struct DebugFileOptions {
    path: PathBuf,
    input: Option<PathBuf>,
    strict: bool,
    breakpoints: Vec<DebugBreakpoint>,
    dap: bool,
}

fn main() -> ExitCode {
    match standalone::read_current_executable() {
        Ok(Some(application)) => ExitCode::from(run_embedded_application(application)),
        Ok(None) => ExitCode::from(run(env::args().skip(1).collect())),
        Err(error) => {
            eprintln!("star: cannot read standalone application: {error}");
            ExitCode::from(EXIT_IO)
        }
    }
}

fn run(args: Vec<String>) -> u8 {
    let Some(command) = args.first().map(String::as_str) else {
        eprint!("{HELP}");
        return EXIT_USAGE;
    };
    match command {
        "--version" | "-V" | "version" if args.len() == 1 => {
            println!(
                "star {} (language 1.0, .starc 1.0)",
                env!("CARGO_PKG_VERSION")
            );
            EXIT_OK
        }
        "help" | "--help" | "-h" => {
            print!("{HELP}");
            EXIT_OK
        }
        "lsp" if args.len() == 1 => match star_lsp::run() {
            Ok(()) => EXIT_OK,
            Err(error) => {
                eprintln!("star: language server failed: {error}");
                EXIT_IO
            }
        },
        "new" => match parse_new_options(&args[1..]) {
            Ok((directory, name)) => create_project(&directory, &name, true),
            Err(message) => {
                eprintln!("star: {message}\n\n{HELP}");
                EXIT_USAGE
            }
        },
        "init" => match parse_init_options(&args[1..]) {
            Ok(name) => match env::current_dir() {
                Ok(directory) => create_project(&directory, &name, false),
                Err(error) => {
                    eprintln!("star: cannot determine current directory: {error}");
                    EXIT_IO
                }
            },
            Err(message) => {
                eprintln!("star: {message}\n\n{HELP}");
                EXIT_USAGE
            }
        },
        "add" | "remove" | "install" | "update" | "search" | "package" | "publish" | "audit"
        | "login" => package_commands::run(command, &args[1..]),
        "run" => match parse_run_options(&args[1..]) {
            Ok(options) => run_file(options),
            Err(message) => {
                eprintln!("star: {message}\n\n{HELP}");
                EXIT_USAGE
            }
        },
        "debug" => match parse_debug_options(&args[1..]) {
            Ok(options) if options.dap => debug_file_dap(options),
            Ok(options) => debug_file(options),
            Err(message) => {
                eprintln!("star: {message}\n\n{HELP}");
                EXIT_USAGE
            }
        },
        "check" => match parse_execution_options(&args[1..], "check", false) {
            Ok(options) => check_file(options),
            Err(message) => {
                eprintln!("star: {message}\n\n{HELP}");
                EXIT_USAGE
            }
        },
        "build" => match parse_build_options(&args[1..]) {
            Ok(options) => build_file(options),
            Err(message) => {
                eprintln!("star: {message}\n\n{HELP}");
                EXIT_USAGE
            }
        },
        "test" => match parse_test_options(&args[1..]) {
            Ok(options) => run_oj_suite(options),
            Err(message) => {
                eprintln!("star: {message}\n\n{HELP}");
                EXIT_USAGE
            }
        },
        "bench" => match parse_bench_options(&args[1..]) {
            Ok(options) => benchmark(options),
            Err(message) => {
                eprintln!("star: {message}\n\n{HELP}");
                EXIT_USAGE
            }
        },
        "fmt" => source_tools::format_command(&args[1..]),
        "minify" => source_tools::minify_command(&args[1..]),
        "encode" => source_tools::encode_command(&args[1..]),
        "decode" => source_tools::decode_command(&args[1..]),
        "disasm" => disassemble_file(&args[1..]),
        "repl" => {
            if args.len() != 1 {
                eprintln!("star: `repl` does not accept arguments");
                EXIT_USAGE
            } else {
                run_repl()
            }
        }
        other => {
            eprintln!("star: unknown command `{other}`\n\n{HELP}");
            EXIT_USAGE
        }
    }
}

fn disassemble_file(arguments: &[String]) -> u8 {
    if arguments.len() != 1 {
        eprintln!("star: `disasm` requires exactly one `.starc` file");
        return EXIT_USAGE;
    }
    let path = PathBuf::from(&arguments[0]);
    let bytes = match read_limited_file(&path, DEFAULT_FILE_BYTES) {
        Ok(bytes) => bytes,
        Err(code) => return code,
    };
    let mut policy = verification_policy(&ExecutionLimits::default());
    // Disassembly verifies structure but never executes privileged code.
    policy.granted_capabilities = [
        Capability::FsRead,
        Capability::FsWrite,
        Capability::NetConnect,
        Capability::NetListen,
        Capability::ProcessSpawn,
        Capability::EnvRead,
        Capability::Ffi,
        Capability::Clock,
        Capability::Random,
    ]
    .into_iter()
    .collect();
    match disassemble_bytecode(&bytes, &policy) {
        Ok(listing) => {
            print!("{listing}");
            if let Err(error) = io::stdout().flush() {
                eprintln!("{}: {error}", path.display());
                EXIT_IO
            } else {
                EXIT_OK
            }
        }
        Err(error) => {
            eprintln!("{}: {error}", path.display());
            EXIT_SOURCE
        }
    }
}

fn run_repl() -> u8 {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut interpreter = Interpreter::new(BufReader::new(stdin.lock()), stdout.lock());
    let mut pending_source = Vec::new();

    loop {
        if pending_source.is_empty() {
            eprint!("star> ");
        } else {
            eprint!("...> ");
        }
        if let Err(error) = io::stderr().flush() {
            eprintln!("star: cannot write prompt: {error}");
            return EXIT_IO;
        }

        let line = match interpreter.read_repl_line(DEFAULT_REPL_LINE_BYTES) {
            Ok(Some(line)) => line,
            Ok(None) => {
                if pending_source.is_empty() {
                    return EXIT_OK;
                }
                let code = report_pending_repl_error(&pending_source);
                return code;
            }
            Err(error) => {
                eprintln!("star: REPL input failed: {error}");
                return if matches!(error, InputError::LimitExceeded) {
                    EXIT_LIMIT
                } else {
                    EXIT_IO
                };
            }
        };

        if pending_source.is_empty() {
            match trim_ascii(&line) {
                b"exit" | b"quit" => return EXIT_OK,
                b"help" => {
                    eprintln!("Enter Star source; incomplete instructions and blocks continue on the next line.");
                    eprintln!("Commands: help, reset, exit");
                    continue;
                }
                b"reset" => {
                    match interpreter.reset_repl_session() {
                        Ok(()) => eprintln!("star: session reset"),
                        Err(error) => eprintln!(
                            "star: session state reset; garbage collection deferred: {error}"
                        ),
                    }
                    continue;
                }
                _ => {}
            }
        }

        if pending_source
            .len()
            .saturating_add(line.len())
            .saturating_add(1)
            > DEFAULT_FILE_BYTES
        {
            eprintln!("star: REPL submission exceeds the {DEFAULT_FILE_BYTES}-byte limit");
            pending_source.clear();
            continue;
        }
        pending_source.extend_from_slice(&line);
        pending_source.push(b'\n');

        let normalized = match normalize_source(&pending_source, LexMode::Normal) {
            Ok(source) => source,
            Err(error) => {
                eprintln!("star: {error}");
                pending_source.clear();
                continue;
            }
        };
        match parse(&normalized) {
            Ok(program) if program.statements.is_empty() => pending_source.clear(),
            Ok(program) => {
                let previous_stack_len = interpreter.stack().len();
                match interpreter.execute_repl(&program) {
                    Ok(()) => {
                        if let Some(value) = interpreter.stack().get(previous_stack_len) {
                            eprintln!("=> {value}");
                        }
                    }
                    Err(error) => eprintln!("star: {error}"),
                }
                pending_source.clear();
            }
            Err(error) if is_incomplete_repl_input(&error.kind) => {}
            Err(error) => {
                report_repl_parse_error(&error);
                pending_source.clear();
            }
        }
    }
}

fn trim_ascii(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .map_or(start, |index| index + 1);
    &bytes[start..end]
}

fn is_incomplete_repl_input(kind: &ParseErrorKind) -> bool {
    matches!(
        kind,
        ParseErrorKind::IncompleteBasicInstruction
            | ParseErrorKind::IncompleteExtendedPayload { .. }
            | ParseErrorKind::MissingExtendedTerminator
            | ParseErrorKind::UnterminatedDataLiteral
            | ParseErrorKind::UnclosedBlock { .. }
    )
}

fn report_repl_parse_error(error: &star_parser::ParseError) {
    eprintln!(
        "star: {} at {}:{}",
        error.kind, error.span.start.line, error.span.start.column
    );
}

fn report_pending_repl_error(source: &[u8]) -> u8 {
    match normalize_source(source, LexMode::Normal) {
        Ok(normalized) => match parse(&normalized) {
            Ok(_) => EXIT_OK,
            Err(error) => {
                report_repl_parse_error(&error);
                EXIT_SOURCE
            }
        },
        Err(error) => {
            eprintln!("star: {error}");
            EXIT_SOURCE
        }
    }
}

fn parse_run_options(arguments: &[String]) -> Result<RunOptions, String> {
    parse_execution_options(arguments, "run", true)
}

fn parse_debug_options(arguments: &[String]) -> Result<DebugFileOptions, String> {
    let mut path = None;
    let mut input = None;
    let mut strict = false;
    let mut breakpoints = Vec::new();
    let mut dap = false;
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--strict" => {
                strict = true;
                index += 1;
            }
            "--dap" => {
                dap = true;
                index += 1;
            }
            "--input" => {
                if input.is_some() {
                    return Err("`--input` may only be specified once".to_owned());
                }
                let value = arguments
                    .get(index + 1)
                    .ok_or_else(|| "missing value for `--input`".to_owned())?;
                input = Some(PathBuf::from(value));
                index += 2;
            }
            "--break" => {
                let value = arguments
                    .get(index + 1)
                    .ok_or_else(|| "missing value for `--break`".to_owned())?;
                breakpoints.push(parse_debug_breakpoint(value)?);
                index += 2;
            }
            option if option.starts_with('-') => {
                return Err(format!("unknown debugger option `{option}`"));
            }
            _ => {
                if path.is_some() {
                    return Err("`star debug` accepts exactly one source file".to_owned());
                }
                path = Some(PathBuf::from(&arguments[index]));
                index += 1;
            }
        }
    }
    Ok(DebugFileOptions {
        path: path.ok_or_else(|| "`star debug` requires a source file".to_owned())?,
        input,
        strict,
        breakpoints,
        dap,
    })
}

fn parse_debug_breakpoint(value: &str) -> Result<DebugBreakpoint, String> {
    let (line, column) = match value.split_once(':') {
        Some((line, column)) => (
            line.parse::<usize>()
                .map_err(|_| format!("invalid breakpoint line `{line}`"))?,
            Some(
                column
                    .parse::<usize>()
                    .map_err(|_| format!("invalid breakpoint column `{column}`"))?,
            ),
        ),
        None => (
            value
                .parse::<usize>()
                .map_err(|_| format!("invalid breakpoint line `{value}`"))?,
            None,
        ),
    };
    if line == 0 || column == Some(0) {
        return Err("breakpoint lines and columns are one-based".to_owned());
    }
    Ok(DebugBreakpoint { line, column })
}

fn debug_file(options: DebugFileOptions) -> u8 {
    let source = match read_limited_file(&options.path, DEFAULT_FILE_BYTES) {
        Ok(source) => source,
        Err(code) => return code,
    };
    let mode = if options.strict {
        LexMode::Strict
    } else {
        LexMode::Normal
    };
    let normalized = match normalize_source(&source, mode) {
        Ok(normalized) => normalized,
        Err(error) => {
            eprintln!("{}: {error}", options.path.display());
            return EXIT_SOURCE;
        }
    };
    let program = match parse(&normalized) {
        Ok(program) => program,
        Err(error) => {
            eprintln!("{}: {error}", options.path.display());
            return EXIT_SOURCE;
        }
    };
    let guest_input = match options.input {
        Some(path) => match read_limited_file(&path, DEFAULT_FILE_BYTES) {
            Ok(input) => input,
            Err(code) => return code,
        },
        None => Vec::new(),
    };
    let stdout = io::stdout();
    let mut interpreter = Interpreter::new(BufReader::new(Cursor::new(guest_input)), stdout.lock());
    let debug_options = DebugOptions {
        pause_on_entry: true,
        breakpoints: options.breakpoints,
    };
    match interpreter.execute_debug(
        &program,
        options.path.to_string_lossy(),
        debug_options,
        Box::new(InteractiveDebugger),
    ) {
        Ok(DebugExecution::Completed | DebugExecution::Quit) => EXIT_OK,
        Err(error) => report_runtime_error(&options.path, error),
    }
}

fn debug_file_dap(options: DebugFileOptions) -> u8 {
    let source = match read_limited_file(&options.path, DEFAULT_FILE_BYTES) {
        Ok(source) => source,
        Err(code) => return code,
    };
    let mode = if options.strict {
        LexMode::Strict
    } else {
        LexMode::Normal
    };
    let normalized = match normalize_source(&source, mode) {
        Ok(normalized) => normalized,
        Err(error) => {
            eprintln!("{}: {error}", options.path.display());
            return EXIT_SOURCE;
        }
    };
    let program = match parse(&normalized) {
        Ok(program) => program,
        Err(error) => {
            eprintln!("{}: {error}", options.path.display());
            return EXIT_SOURCE;
        }
    };
    let valid_locations = source_breakpoint_locations(&program)
        .into_iter()
        .filter_map(|location| location.column.map(|column| (location.line, column)))
        .collect();
    let guest_input = match options.input {
        Some(path) => match read_limited_file(&path, DEFAULT_FILE_BYTES) {
            Ok(input) => input,
            Err(code) => return code,
        },
        None => Vec::new(),
    };
    let cli_breakpoints = options.breakpoints;
    let dap_output = dap::DapOutput::new();
    let mut controller =
        match dap::DapController::new(&options.path, dap_output.clone(), valid_locations) {
            Ok(controller) => controller,
            Err(error) => {
                eprintln!(
                    "{}: cannot initialize DAP source: {error}",
                    options.path.display()
                );
                return EXIT_IO;
            }
        };
    let mut breakpoints = match controller.configure() {
        Ok(breakpoints) => breakpoints,
        Err(error) => {
            if error.kind() != io::ErrorKind::Interrupted {
                eprintln!("star: DAP configuration failed: {error}");
            }
            return if error.kind() == io::ErrorKind::Interrupted {
                EXIT_OK
            } else {
                EXIT_IO
            };
        }
    };
    breakpoints.extend(cli_breakpoints);
    let mut interpreter = Interpreter::new(
        BufReader::new(Cursor::new(guest_input)),
        dap::DapGuestOutput::new(dap_output.clone()),
    );
    let result = interpreter.execute_debug(
        &program,
        options.path.to_string_lossy(),
        DebugOptions {
            pause_on_entry: true,
            breakpoints,
        },
        Box::new(controller),
    );
    match result {
        Ok(DebugExecution::Completed | DebugExecution::Quit) => {
            let _ = dap_output.finish(0);
            EXIT_OK
        }
        Err(error) => {
            let exit = report_runtime_error(&options.path, error);
            let _ = dap_output.finish(i32::from(exit));
            exit
        }
    }
}

struct InteractiveDebugger;

impl DebugController for InteractiveDebugger {
    fn on_stop(&mut self, reason: DebugStopReason, snapshot: &DebugSnapshot) -> DebugCommand {
        eprintln!(
            "paused ({reason:?}) at {}:{}:{}",
            snapshot.source_name, snapshot.location.start.line, snapshot.location.start.column
        );
        let stdin = io::stdin();
        loop {
            eprint!("(star-debug) ");
            if let Err(error) = io::stderr().flush() {
                eprintln!("star: cannot write debugger prompt: {error}");
                return DebugCommand::Quit;
            }
            let mut command = String::new();
            match stdin.read_line(&mut command) {
                Ok(0) => return DebugCommand::Quit,
                Ok(_) => {}
                Err(error) => {
                    eprintln!("star: debugger input failed: {error}");
                    return DebugCommand::Quit;
                }
            }
            match command.trim() {
                "continue" | "c" => return DebugCommand::Continue,
                "step" | "s" => return DebugCommand::StepInto,
                "next" | "n" => return DebugCommand::StepOver,
                "finish" | "out" => return DebugCommand::StepOut,
                "quit" | "q" => return DebugCommand::Quit,
                "where" => eprintln!(
                    "{}:{}:{}",
                    snapshot.source_name,
                    snapshot.location.start.line,
                    snapshot.location.start.column
                ),
                "stack" => print_debug_stack(snapshot),
                "locals" => print_debug_bindings("locals", &snapshot.locals),
                "globals" => print_debug_bindings("globals", &snapshot.globals),
                "bt" => print_debug_backtrace(snapshot),
                "help" | "h" => {
                    eprintln!("Commands: continue, step, next, finish, where, bt, stack, locals, globals, quit");
                }
                "" => {}
                other => eprintln!("unknown debugger command `{other}`; enter `help`"),
            }
        }
    }
}

fn print_debug_stack(snapshot: &DebugSnapshot) {
    for (index, value) in snapshot.operand_stack.iter().enumerate() {
        eprintln!("  [{index}] {} {}", value.type_name, value.preview);
    }
    if snapshot.operand_stack_truncated {
        eprintln!("  ... older stack values omitted");
    }
}

fn print_debug_bindings(label: &str, bindings: &[DebugBinding]) {
    eprintln!("{label}:");
    for binding in bindings {
        let mutability = if binding.mutable { "mut" } else { "const" };
        eprintln!(
            "  [{}] scope={} {mutability} {} {}",
            binding.id, binding.scope_depth, binding.value.type_name, binding.value.preview
        );
    }
}

fn print_debug_backtrace(snapshot: &DebugSnapshot) {
    for (index, frame) in snapshot.call_stack.iter().enumerate().rev() {
        match (&frame.function_id, frame.call_site) {
            (Some(function_id), Some(call_site)) => eprintln!(
                "  #{index} function {function_id} called at {}:{}:{}",
                snapshot.source_name, call_site.start.line, call_site.start.column
            ),
            _ => eprintln!("  #{index} top-level"),
        }
    }
    if snapshot.call_stack_truncated {
        eprintln!("  ... outer call frames omitted");
    }
}

fn parse_new_options(arguments: &[String]) -> Result<(PathBuf, String), String> {
    if arguments.len() != 1 {
        return Err("`star new` requires exactly one project directory".to_owned());
    }
    let directory = PathBuf::from(&arguments[0]);
    let name = directory
        .file_name()
        .and_then(|component| component.to_str())
        .filter(|name| !name.is_empty())
        .ok_or_else(|| "project directory must have a final path component".to_owned())?
        .to_owned();
    validate_project_name(&name)?;
    Ok((directory, name))
}

fn parse_init_options(arguments: &[String]) -> Result<String, String> {
    if arguments.len() > 1 {
        return Err("`star init` accepts at most one package name".to_owned());
    }
    let name = if let Some(name) = arguments.first() {
        name.clone()
    } else {
        env::current_dir()
            .ok()
            .and_then(|directory| {
                directory
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
            .ok_or_else(|| {
                "supply a package name when the current directory has no name".to_owned()
            })?
    };
    validate_project_name(&name)?;
    Ok(name)
}

fn validate_project_name(name: &str) -> Result<(), String> {
    let mut characters = name.chars();
    if !characters
        .next()
        .is_some_and(|character| character.is_ascii_alphabetic() || character == '_')
        || !characters
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
    {
        return Err(
            "package names must start with an ASCII letter or underscore and contain only ASCII letters, digits, `_`, or `-`".to_owned(),
        );
    }
    Ok(())
}

fn create_project(directory: &Path, name: &str, create_directory: bool) -> u8 {
    let manifest = directory.join(star_package::MANIFEST_FILE);
    let source_dir = directory.join("src");
    let source = source_dir.join("main.star");
    let library = source_dir.join("lib.star");
    let example_dir = directory.join("examples");
    let example = example_dir.join("hello.star");
    let gitignore = directory.join(".gitignore");
    for path in [&manifest, &source, &library, &example, &gitignore] {
        if path.exists() {
            eprintln!("{}: project file already exists", path.display());
            return EXIT_IO;
        }
    }
    if directory.join("Star.toml").exists() {
        eprintln!(
            "{}: project manifest already exists",
            directory.join("Star.toml").display()
        );
        return EXIT_IO;
    }
    if create_directory && directory.exists() {
        eprintln!("{}: project directory already exists", directory.display());
        return EXIT_IO;
    }
    if let Err(error) =
        fs::create_dir_all(&source_dir).and_then(|()| fs::create_dir_all(&example_dir))
    {
        eprintln!("{}: {error}", source_dir.display());
        return EXIT_IO;
    }
    let source_bits = "010010000110010101101100011011000110111100101100001000000101001101110100011000010111001000100001";
    let source_text = format!("\"{source_bits}\"******'\n");
    let manifest_text = format!(
        "schema_version = 1\n\n[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"1\"\ndescription = \"A Star language project\"\n\n[build]\nentry = \"src/main.star\"\ntarget = \"bytecode\"\noptimization = 0\n\n[dependencies]\n\n[permissions]\nfilesystem = false\nnetwork = false\nprocess = false\nffi = false\nenvironment = false\n"
    );
    for (path, contents) in [
        (&manifest, manifest_text.as_bytes()),
        (&source, source_text.as_bytes()),
        (&library, b"".as_slice()),
        (&example, source_text.as_bytes()),
        (
            &gitignore,
            b"/target/\n/deps/\n/.star/\n/src/*.starc\n".as_slice(),
        ),
    ] {
        if let Err(error) = fs::write(path, contents) {
            eprintln!("{}: {error}", path.display());
            return EXIT_IO;
        }
    }
    println!("created Star project `{name}` at {}", directory.display());
    EXIT_OK
}

fn parse_execution_options(
    arguments: &[String],
    command: &str,
    allow_stats: bool,
) -> Result<RunOptions, String> {
    let mut path = None;
    let mut strict = false;
    let mut stats = false;
    let mut limits = ExecutionLimits::default();
    let mut index = 0;
    while index < arguments.len() {
        let argument = &arguments[index];
        if argument == "--strict" {
            strict = true;
            index += 1;
            continue;
        }
        if argument == "--stats" {
            if !allow_stats {
                return Err(format!("`--stats` is not supported by `star {command}`"));
            }
            stats = true;
            index += 1;
            continue;
        }
        let option = argument.as_str();
        if option == "--timeout" && !matches!(command, "run" | "bench") {
            return Err(format!("`--timeout` is not supported by `star {command}`"));
        }
        if option == "--max-call-depth" && !matches!(command, "run" | "bench") {
            return Err(format!(
                "`--max-call-depth` is not supported by `star {command}`"
            ));
        }
        if matches!(
            option,
            "--max-memory"
                | "--max-stack"
                | "--max-call-depth"
                | "--max-instructions"
                | "--timeout"
        ) {
            let value = arguments
                .get(index + 1)
                .ok_or_else(|| format!("missing value for `{option}`"))?;
            match option {
                "--max-memory" => limits.max_memory_bytes = parse_byte_size(value)?,
                "--max-stack" => limits.max_stack_values = parse_positive_usize(option, value)?,
                "--max-call-depth" => limits.max_call_depth = parse_positive_usize(option, value)?,
                "--max-instructions" => {
                    limits.max_instructions = value
                        .parse::<u64>()
                        .ok()
                        .filter(|count| *count > 0)
                        .ok_or_else(|| format!("`{option}` must be a positive integer"))?;
                }
                "--timeout" => limits.max_execution_time = parse_duration(value)?,
                _ => unreachable!(),
            }
            index += 2;
            continue;
        }
        if argument.starts_with('-') {
            return Err(format!("unknown {command} option `{argument}`"));
        }
        if path.replace(PathBuf::from(argument)).is_some() {
            return Err(format!("`star {command}` accepts exactly one input file"));
        }
        index += 1;
    }
    Ok(RunOptions {
        path: match path {
            Some(path) => path,
            None => resolve_project_entry()
                .map_err(|error| format!("missing input file for `star {command}`: {error}"))?,
        },
        strict,
        stats,
        limits,
    })
}

fn parse_build_options(arguments: &[String]) -> Result<BuildOptions, String> {
    let mut path = None;
    let mut output = None;
    let mut target = BuildTarget::Bytecode;
    let mut target_seen = false;
    let mut release = false;
    let mut strict = false;
    let mut limits = ExecutionLimits::default();
    let mut index = 0;
    while index < arguments.len() {
        let argument = &arguments[index];
        if argument == "--strict" {
            strict = true;
            index += 1;
            continue;
        }
        if argument == "--output" {
            let value = arguments
                .get(index + 1)
                .ok_or_else(|| "missing value for `--output`".to_owned())?;
            if output.replace(PathBuf::from(value)).is_some() {
                return Err("`star build` accepts `--output` once".to_owned());
            }
            index += 2;
            continue;
        }
        if argument == "--target" {
            let value = arguments
                .get(index + 1)
                .ok_or_else(|| "missing value for `--target`".to_owned())?;
            if target_seen {
                return Err("`star build` accepts `--target` once".to_owned());
            }
            target = match value.as_str() {
                "bytecode" => BuildTarget::Bytecode,
                "native" => BuildTarget::Native,
                _ => return Err("`--target` must be `bytecode` or `native`".to_owned()),
            };
            target_seen = true;
            index += 2;
            continue;
        }
        if argument == "--release" {
            if release {
                return Err("`star build` accepts `--release` once".to_owned());
            }
            release = true;
            index += 1;
            continue;
        }
        let option = argument.as_str();
        if matches!(
            option,
            "--max-memory" | "--max-stack" | "--max-instructions"
        ) {
            let value = arguments
                .get(index + 1)
                .ok_or_else(|| format!("missing value for `{option}`"))?;
            match option {
                "--max-memory" => limits.max_memory_bytes = parse_byte_size(value)?,
                "--max-stack" => limits.max_stack_values = parse_positive_usize(option, value)?,
                "--max-instructions" => {
                    limits.max_instructions = value
                        .parse::<u64>()
                        .ok()
                        .filter(|count| *count > 0)
                        .ok_or_else(|| format!("`{option}` must be a positive integer"))?;
                }
                _ => unreachable!(),
            }
            index += 2;
            continue;
        }
        if argument.starts_with('-') {
            return Err(format!("unknown build option `{argument}`"));
        }
        if path.replace(PathBuf::from(argument)).is_some() {
            return Err("`star build` accepts exactly one source file".to_owned());
        }
        index += 1;
    }
    if release && target != BuildTarget::Native {
        return Err("`--release` is only supported with `--target native`".to_owned());
    }
    if target == BuildTarget::Native && !release {
        return Err("`--target native` requires `--release`".to_owned());
    }
    Ok(BuildOptions {
        path: match path {
            Some(path) => path,
            None => resolve_project_entry()
                .map_err(|error| format!("missing source file for `star build`: {error}"))?,
        },
        output,
        target,
        strict,
        limits,
    })
}

fn parse_test_options(arguments: &[String]) -> Result<TestOptions, String> {
    let mut suite = None;
    let mut solutions = None;
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--suite" => {
                let value = arguments
                    .get(index + 1)
                    .ok_or_else(|| "missing value for `--suite`".to_owned())?;
                if suite.replace(value.as_str()).is_some() {
                    return Err("`star test` accepts `--suite` once".to_owned());
                }
                index += 2;
            }
            "--solutions" => {
                let value = arguments
                    .get(index + 1)
                    .ok_or_else(|| "missing value for `--solutions`".to_owned())?;
                if solutions.replace(PathBuf::from(value)).is_some() {
                    return Err("`star test` accepts `--solutions` once".to_owned());
                }
                index += 2;
            }
            option if option.starts_with('-') => {
                return Err(format!("unknown test option `{option}`"));
            }
            other => return Err(format!("unexpected `star test` argument `{other}`")),
        }
    }
    if suite != Some("oj") {
        return Err("`star test` currently requires `--suite oj`".to_owned());
    }
    Ok(TestOptions { solutions })
}

fn parse_bench_options(arguments: &[String]) -> Result<BenchOptions, String> {
    let mut iterations = None;
    let mut input = None;
    let mut run_arguments = Vec::new();
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--iterations" => {
                let value = arguments
                    .get(index + 1)
                    .ok_or_else(|| "missing value for `--iterations`".to_owned())?;
                if iterations.is_some() {
                    return Err("`star bench` accepts `--iterations` once".to_owned());
                }
                iterations = Some(parse_positive_usize("--iterations", value)?);
                index += 2;
            }
            "--input" => {
                let value = arguments
                    .get(index + 1)
                    .ok_or_else(|| "missing value for `--input`".to_owned())?;
                if input.replace(PathBuf::from(value)).is_some() {
                    return Err("`star bench` accepts `--input` once".to_owned());
                }
                index += 2;
            }
            argument => {
                run_arguments.push(argument.to_owned());
                index += 1;
            }
        }
    }
    let execution = parse_execution_options(&run_arguments, "bench", false)?;
    if execution.stats {
        return Err("`--stats` is not supported by `star bench`".to_owned());
    }
    Ok(BenchOptions {
        execution,
        iterations: iterations
            .ok_or_else(|| "missing `--iterations` for `star bench`".to_owned())?,
        input,
    })
}

fn parse_positive_usize(option: &str, value: &str) -> Result<usize, String> {
    value
        .parse::<usize>()
        .ok()
        .filter(|count| *count > 0)
        .ok_or_else(|| format!("`{option}` must be a positive integer"))
}

fn parse_byte_size(value: &str) -> Result<usize, String> {
    let split = value
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(value.len());
    let (digits, suffix) = value.split_at(split);
    let amount = digits
        .parse::<usize>()
        .ok()
        .filter(|amount| *amount > 0)
        .ok_or_else(|| "`--max-memory` must be a positive byte size".to_owned())?;
    let multiplier = match suffix.to_ascii_lowercase().as_str() {
        "" | "b" => 1usize,
        "k" | "kb" | "kib" => 1024,
        "m" | "mb" | "mib" => 1024usize.pow(2),
        "g" | "gb" | "gib" => 1024usize.pow(3),
        _ => return Err(format!("unsupported memory-size suffix `{suffix}`")),
    };
    amount
        .checked_mul(multiplier)
        .ok_or_else(|| "`--max-memory` is too large".to_owned())
}

fn parse_duration(value: &str) -> Result<Duration, String> {
    let (digits, multiplier) = if let Some(digits) = value.strip_suffix("ms") {
        (digits, 1u32)
    } else if let Some(digits) = value.strip_suffix('s') {
        (digits, 1_000u32)
    } else {
        return Err("`--timeout` must use `ms` or `s`, for example `500ms` or `30s`".to_owned());
    };
    let amount = digits
        .parse::<u64>()
        .ok()
        .filter(|amount| *amount > 0)
        .ok_or_else(|| "`--timeout` must be a positive duration".to_owned())?;
    let milliseconds = amount
        .checked_mul(u64::from(multiplier))
        .ok_or_else(|| "`--timeout` is too large".to_owned())?;
    Ok(Duration::from_millis(milliseconds))
}

fn check_file(options: RunOptions) -> u8 {
    let bytes = match read_limited_file(&options.path, options.limits.max_memory_bytes) {
        Ok(bytes) => bytes,
        Err(code) => return code,
    };
    match extension(&options.path).as_deref() {
        Some("starc") => {
            if options.strict {
                eprintln!("`--strict` applies only to `.star` source files");
                return EXIT_USAGE;
            }
            match verify_bytecode(&bytes, &verification_policy(&options.limits)) {
                Ok(_) => {
                    println!("{}: valid bytecode", options.path.display());
                    EXIT_OK
                }
                Err(error) => {
                    eprintln!("{}: {error}", options.path.display());
                    EXIT_SOURCE
                }
            }
        }
        Some("star") => {
            let compilation = if package_root_for(&options.path).is_some() {
                compile_project_source(&options.path, options.strict, options.limits.clone())
                    .map(|(bytecode, _)| bytecode)
            } else {
                let program = match parse_source(&options.path, &bytes, options.strict) {
                    Ok(program) => program,
                    Err(code) => return code,
                };
                compile_source_program(&options.path, &program, options.limits.clone())
            };
            match compilation {
                Ok(_) => {
                    println!("{}: valid source", options.path.display());
                    EXIT_OK
                }
                Err(error) => {
                    eprintln!("{}: {error}", options.path.display());
                    EXIT_SOURCE
                }
            }
        }
        _ => {
            eprintln!(
                "{}: expected a `.star` or `.starc` file",
                options.path.display()
            );
            EXIT_USAGE
        }
    }
}

fn build_file(options: BuildOptions) -> u8 {
    if !extension(&options.path).is_some_and(|extension| extension == "star") {
        eprintln!(
            "{}: `star build` requires a `.star` source file",
            options.path.display()
        );
        return EXIT_USAGE;
    }
    if options.target == BuildTarget::Native && cfg!(debug_assertions) {
        eprintln!(
            "star: native app bundling requires a release-compiled `star` executable; rebuild `star-cli` with `--release`"
        );
        return EXIT_USAGE;
    }
    let bytecode_result: Result<(Vec<u8>, VerificationPolicy), String> =
        if package_root_for(&options.path).is_some() {
            compile_project_source(&options.path, options.strict, options.limits.clone())
        } else {
            let bytes = match read_limited_file(&options.path, options.limits.max_memory_bytes) {
                Ok(bytes) => bytes,
                Err(code) => return code,
            };
            let program = match parse_source(&options.path, &bytes, options.strict) {
                Ok(program) => program,
                Err(code) => return code,
            };
            let policy = verification_policy(&options.limits);
            compile_source_program(&options.path, &program, options.limits.clone())
                .map(|bytecode| (bytecode, policy))
        };
    let (bytecode, policy) = match bytecode_result {
        Ok(compiled) => compiled,
        Err(error) => {
            eprintln!("{}: {error}", options.path.display());
            return EXIT_SOURCE;
        }
    };
    let output = options.output.unwrap_or_else(|| match options.target {
        BuildTarget::Bytecode => {
            let mut output = options.path.clone();
            output.set_extension("starc");
            output
        }
        BuildTarget::Native => default_native_output(&options.path),
    });
    let write_result = match options.target {
        BuildTarget::Bytecode => fs::write(&output, bytecode),
        BuildTarget::Native => {
            if let (Ok(source_path), Ok(output_path)) =
                (fs::canonicalize(&options.path), fs::canonicalize(&output))
            {
                if source_path == output_path {
                    eprintln!(
                        "{}: output path would overwrite the source file",
                        output.display()
                    );
                    return EXIT_USAGE;
                }
            }
            let runtime_path = match env::current_exe() {
                Ok(path) => path,
                Err(error) => {
                    eprintln!("star: cannot locate the running Star executable: {error}");
                    return EXIT_IO;
                }
            };
            standalone::write_application(
                &runtime_path,
                &output,
                &bytecode,
                capability_bits(&policy),
            )
        }
    };
    if let Err(error) = write_result {
        eprintln!("{}: {error}", output.display());
        return EXIT_IO;
    }
    match options.target {
        BuildTarget::Bytecode => println!("built {}", output.display()),
        BuildTarget::Native => println!("bundled {}", output.display()),
    }
    EXIT_OK
}

fn default_native_output(source: &Path) -> PathBuf {
    let mut name: OsString = source
        .file_stem()
        .unwrap_or_else(|| OsStr::new("star-app"))
        .to_os_string();
    if cfg!(windows) {
        name.push(".exe");
    }
    source.parent().unwrap_or_else(|| Path::new(".")).join(name)
}

fn run_embedded_application(application: standalone::EmbeddedApplication) -> u8 {
    let limits = ExecutionLimits::default();
    let mut policy = verification_policy(&limits);
    policy.granted_capabilities = capabilities_from_bits(application.capability_bits);
    run_bytecode_with_policy(
        Path::new("<embedded application>"),
        &application.bytecode,
        limits,
        false,
        policy,
    )
}

const STANDALONE_CAPABILITIES: [Capability; 9] = [
    Capability::FsRead,
    Capability::FsWrite,
    Capability::NetConnect,
    Capability::NetListen,
    Capability::ProcessSpawn,
    Capability::EnvRead,
    Capability::Ffi,
    Capability::Clock,
    Capability::Random,
];

fn capability_bits(policy: &VerificationPolicy) -> u64 {
    STANDALONE_CAPABILITIES
        .iter()
        .enumerate()
        .filter(|(_, capability)| policy.granted_capabilities.contains(*capability))
        .fold(0_u64, |bits, (index, _)| bits | (1 << index))
}

fn capabilities_from_bits(bits: u64) -> BTreeSet<Capability> {
    STANDALONE_CAPABILITIES
        .iter()
        .enumerate()
        .filter(|(index, _)| bits & (1_u64 << *index) != 0)
        .map(|(_, capability)| *capability)
        .collect()
}

fn compile_source_program(
    path: &Path,
    program: &star_parser::Program,
    limits: ExecutionLimits,
) -> Result<Vec<u8>, String> {
    analyze(program).map_err(|error| error.to_string())?;
    let max_stack = u32::try_from(limits.max_stack_values).unwrap_or(u32::MAX);
    let mut module =
        source_lowering::lower(program, max_stack).map_err(|error| error.to_string())?;
    module
        .functions
        .sort_by(|left, right| left.id.cmp(&right.id));
    let module_name = path
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or("main");
    let source_file = path.to_string_lossy();
    let provisional =
        compile(&module, module_name, &source_file).map_err(|error| error.to_string())?;
    let provisional_verification = verify_bytecode(&provisional, &verification_policy(&limits))
        .map_err(|error| error.to_string())?;
    if provisional_verification.function_stack_depths.len() != module.functions.len() + 1 {
        return Err("verified function table does not match the source module".to_owned());
    }
    module.initializer.max_stack = provisional_verification.function_stack_depths[0];
    for (function, depth) in module.functions.iter_mut().zip(
        provisional_verification
            .function_stack_depths
            .iter()
            .skip(1),
    ) {
        function.max_stack = *depth;
    }
    let bytecode =
        compile(&module, module_name, &source_file).map_err(|error| error.to_string())?;
    verify_bytecode(&bytecode, &verification_policy(&limits)).map_err(|error| error.to_string())?;
    Ok(bytecode)
}

struct ProjectModuleBuild {
    ir: ModuleIr,
    name: String,
    source_file: String,
}

fn resolve_project_packages(
    root: &Path,
    manifest: &Manifest,
) -> Result<Vec<ResolvedPackage>, String> {
    if manifest.dependencies.is_empty() {
        return Ok(Vec::new());
    }
    let lock = star_package::read_lock(root)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "star.lock is missing; run `star install`".to_owned())?;
    let (resolved_lock, mut packages) = star_package::resolve_dependencies_from_lock(
        root,
        &package_commands::local_registry(),
        &lock,
    )
    .map_err(|error| error.to_string())?;
    if resolved_lock != lock {
        return Err("star.lock does not match the manifest; run `star update`".to_owned());
    }
    for package in &mut packages {
        let installed = if package.locked.source.starts_with("path+") {
            package.root.clone()
        } else {
            root.join("deps")
                .join(&package.locked.name)
                .join(&package.locked.version)
        };
        let installed = installed.canonicalize().map_err(|error| {
            format!(
                "dependency `{}` is not installed; run `star install`: {error}",
                package.locked.name
            )
        })?;
        if !installed.starts_with(root) {
            return Err(format!(
                "dependency `{}` escapes the project root",
                package.locked.name
            ));
        }
        let checksum =
            star_package::source_checksum(&installed).map_err(|error| error.to_string())?;
        if checksum != package.locked.checksum {
            return Err(format!(
                "installed dependency `{}` checksum does not match star.lock; run `star install`",
                package.locked.name
            ));
        }
        let installed_manifest = Manifest::read(&installed).map_err(|error| error.to_string())?;
        if installed_manifest.package.name != package.locked.name
            || installed_manifest.package.version != package.locked.version
        {
            return Err(format!(
                "installed dependency `{}` has a mismatched manifest identity",
                package.locked.name
            ));
        }
        package.root = installed;
    }
    Ok(packages)
}

fn compile_project_source(
    entry_path: &Path,
    strict: bool,
    limits: ExecutionLimits,
) -> Result<(Vec<u8>, VerificationPolicy), String> {
    let root = package_root_for(entry_path)
        .ok_or_else(|| "source file is not inside a Star project".to_owned())?;
    let root = root
        .canonicalize()
        .map_err(|error| format!("cannot resolve project root: {error}"))?;
    let entry = entry_path
        .canonicalize()
        .map_err(|error| format!("cannot resolve entry source: {error}"))?;
    if !entry.starts_with(&root) {
        return Err("entry source escapes its project root".to_owned());
    }
    let manifest = Manifest::read(&root).map_err(|error| error.to_string())?;
    let resolved_packages = resolve_project_packages(&root, &manifest)?;
    let mut named_paths = Vec::new();
    let mut seen_paths = HashSet::new();
    let mut project_paths = Vec::new();
    collect_star_sources(&root.join("src"), &mut project_paths)
        .map_err(|error| error.to_string())?;
    project_paths.push(entry.clone());
    for path in project_paths {
        let path = path
            .canonicalize()
            .map_err(|error| format!("cannot resolve source {}: {error}", path.display()))?;
        if !path.starts_with(&root) {
            return Err(format!("module {} escapes project root", path.display()));
        }
        if seen_paths.insert(path.clone()) {
            let relative = path
                .strip_prefix(&root)
                .expect("path is under project root");
            let name = relative.to_string_lossy().replace('\\', "/");
            named_paths.push((path, name));
        }
    }
    for package in &resolved_packages {
        let source_root = package.root.join("src");
        let source_root = match source_root.canonicalize() {
            Ok(path) => path,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.to_string()),
        };
        let mut package_paths = Vec::new();
        collect_star_sources(&source_root, &mut package_paths)
            .map_err(|error| error.to_string())?;
        for path in package_paths {
            let path = path.canonicalize().map_err(|error| error.to_string())?;
            if !path.starts_with(&source_root) {
                return Err(format!(
                    "module {} escapes package source root",
                    path.display()
                ));
            }
            if seen_paths.insert(path.clone()) {
                let relative = path
                    .strip_prefix(&source_root)
                    .expect("path is under package source root");
                let name = format!(
                    "deps/{}/{}/src/{}",
                    package.locked.name,
                    package.locked.version,
                    relative.to_string_lossy().replace('\\', "/")
                );
                named_paths.push((path, name));
            }
        }
    }
    named_paths.sort_by_key(|(path, _)| (path != &entry, path.clone()));
    if manifest.permissions.environment && manifest.permissions.filesystem {
        if let Some(star_path) = env::var_os("STAR_PATH") {
            for (root_index, search_root) in env::split_paths(&star_path).enumerate() {
                let canonical_search_root = match search_root.canonicalize() {
                    Ok(path) => path,
                    Err(_) => continue,
                };
                let mut external_paths = Vec::new();
                collect_star_sources(&canonical_search_root, &mut external_paths)
                    .map_err(|error| error.to_string())?;
                for path in external_paths {
                    let path = path.canonicalize().map_err(|error| error.to_string())?;
                    let relative = path
                        .strip_prefix(&canonical_search_root)
                        .map_err(|_| "STAR_PATH module escapes its search root".to_owned())?;
                    let name = relative.to_string_lossy().replace('\\', "/");
                    if seen_paths.insert(path.clone()) {
                        named_paths.push((path, format!("starpath/{root_index}/{name}")));
                    }
                }
            }
        }
    }

    let max_stack = u32::try_from(limits.max_stack_values).unwrap_or(u32::MAX);
    let mut modules = Vec::with_capacity(named_paths.len());
    let mut entry_index = None;
    let mut total_source_bytes = 0usize;
    for (path, name) in named_paths {
        if path == entry {
            entry_index = Some(
                u32::try_from(modules.len())
                    .map_err(|_| "project has too many modules".to_owned())?,
            );
        }
        let source_limit = limits.max_memory_bytes.min(DEFAULT_FILE_BYTES);
        let metadata =
            fs::metadata(&path).map_err(|error| format!("{}: {error}", path.display()))?;
        let metadata_len = usize::try_from(metadata.len()).unwrap_or(usize::MAX);
        if metadata_len > source_limit
            || total_source_bytes.saturating_add(metadata_len) > source_limit
        {
            return Err("project source exceeds configured aggregate file limit".to_owned());
        }
        let bytes = fs::read(&path).map_err(|error| format!("{}: {error}", path.display()))?;
        total_source_bytes = total_source_bytes.saturating_add(bytes.len());
        if total_source_bytes > source_limit {
            return Err("project source exceeds configured aggregate file limit".to_owned());
        }
        let mode = if strict {
            LexMode::Strict
        } else {
            LexMode::Normal
        };
        let normalized = normalize_source(&bytes, mode)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        let program = parse(&normalized).map_err(|error| format!("{}: {error}", path.display()))?;
        analyze(&program).map_err(|error| format!("{}: {error}", path.display()))?;
        let mut ir = source_lowering::lower(&program, max_stack)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        ir.functions.sort_by(|left, right| left.id.cmp(&right.id));
        let source_file = name.clone();
        modules.push(ProjectModuleBuild {
            ir,
            name,
            source_file,
        });
    }
    let entry_index = entry_index.ok_or_else(|| "project entry module is missing".to_owned())?;
    let module_views: Vec<_> = modules
        .iter()
        .map(|module| CompileModule {
            ir: &module.ir,
            name: &module.name,
            source_file: &module.source_file,
        })
        .collect();
    let policy = policy_with_permissions(&limits, &manifest.permissions);
    let provisional =
        compile_modules(&module_views, entry_index).map_err(|error| error.to_string())?;
    let verified = verify_bytecode(&provisional, &policy).map_err(|error| error.to_string())?;
    if verified.function_stack_depths.len()
        != modules
            .iter()
            .map(|module| module.ir.functions.len() + 1)
            .sum::<usize>()
    {
        return Err("verified function table does not match project modules".to_owned());
    }
    let mut function_index = 0;
    for module in &mut modules {
        module.ir.initializer.max_stack = verified.function_stack_depths[function_index];
        function_index += 1;
        for function in &mut module.ir.functions {
            function.max_stack = verified.function_stack_depths[function_index];
            function_index += 1;
        }
    }
    let module_views: Vec<_> = modules
        .iter()
        .map(|module| CompileModule {
            ir: &module.ir,
            name: &module.name,
            source_file: &module.source_file,
        })
        .collect();
    let bytecode =
        compile_modules(&module_views, entry_index).map_err(|error| error.to_string())?;
    verify_bytecode(&bytecode, &policy).map_err(|error| error.to_string())?;
    Ok((bytecode, policy))
}

fn package_root_for(path: &Path) -> Option<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir().ok()?.join(path)
    };
    let start = absolute.canonicalize().ok()?;
    let start = if start.is_dir() {
        start
    } else {
        start.parent()?.to_path_buf()
    };
    start
        .ancestors()
        .find(|ancestor| locate_manifest(ancestor).is_ok())
        .map(Path::to_path_buf)
}

fn collect_star_sources(root: &Path, output: &mut Vec<PathBuf>) -> io::Result<()> {
    if !root.is_dir() {
        return Ok(());
    }
    let mut entries: Vec<_> = fs::read_dir(root)?.collect::<Result<_, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let file_type = entry.file_type()?;
        let path = entry.path();
        if file_type.is_dir() {
            collect_star_sources(&path, output)?;
        } else if file_type.is_file() && extension(&path).as_deref() == Some("star") {
            output.push(path);
        }
    }
    Ok(())
}

fn policy_with_permissions(
    limits: &ExecutionLimits,
    permissions: &Permissions,
) -> VerificationPolicy {
    let mut policy = verification_policy(limits);
    let mut capabilities = BTreeSet::new();
    if permissions.filesystem {
        capabilities.insert(Capability::FsRead);
        capabilities.insert(Capability::FsWrite);
    }
    if permissions.network {
        capabilities.insert(Capability::NetConnect);
        capabilities.insert(Capability::NetListen);
    }
    if permissions.process {
        capabilities.insert(Capability::ProcessSpawn);
    }
    if permissions.ffi {
        capabilities.insert(Capability::Ffi);
    }
    if permissions.environment {
        capabilities.insert(Capability::EnvRead);
    }
    policy.granted_capabilities = capabilities;
    policy
}

fn parse_source(path: &Path, bytes: &[u8], strict: bool) -> Result<star_parser::Program, u8> {
    let mode = if strict {
        LexMode::Strict
    } else {
        LexMode::Normal
    };
    let normalized = normalize_source(bytes, mode).map_err(|error| {
        eprintln!("{}: {error}", path.display());
        EXIT_SOURCE
    })?;
    parse(&normalized).map_err(|error| {
        eprintln!("{}: {error}", path.display());
        EXIT_SOURCE
    })
}

fn read_limited_file(path: &Path, memory_limit: usize) -> Result<Vec<u8>, u8> {
    let metadata = fs::metadata(path).map_err(|error| {
        eprintln!("{}: {error}", path.display());
        EXIT_IO
    })?;
    let max_file_bytes = memory_limit.min(DEFAULT_FILE_BYTES);
    if metadata.len() > max_file_bytes as u64 {
        eprintln!(
            "{}: input file exceeds the {max_file_bytes}-byte CLI file limit",
            path.display()
        );
        return Err(EXIT_LIMIT);
    }
    fs::read(path).map_err(|error| {
        eprintln!("{}: {error}", path.display());
        EXIT_IO
    })
}

fn extension(path: &Path) -> Option<String> {
    path.extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
}

fn verification_policy(limits: &ExecutionLimits) -> VerificationPolicy {
    VerificationPolicy {
        limits: VerificationLimits {
            max_file_bytes: limits.max_memory_bytes.min(DEFAULT_FILE_BYTES),
            max_table_entries: 65_536,
            max_instructions: limits.max_instructions,
            max_stack_depth: u32::try_from(limits.max_stack_values).unwrap_or(u32::MAX),
        },
        granted_capabilities: BTreeSet::new(),
    }
}

fn run_oj_suite(options: TestOptions) -> u8 {
    let root = match find_project_root() {
        Some(root) => root,
        None => {
            eprintln!(
                "star: cannot find `tests/oj/run.py`; run `star test` from a Star repository"
            );
            return EXIT_IO;
        }
    };
    let runner = root.join("tests").join("oj").join("run.py");
    let original_dir = match env::current_dir() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("star: cannot determine current directory: {error}");
            return EXIT_IO;
        }
    };
    let solutions = match options.solutions {
        Some(path) if path.is_absolute() => path,
        Some(path) => original_dir.join(path),
        None => root.join("tests").join("oj").join("solutions"),
    };
    let star_cli = match env::current_exe() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("star: cannot locate this CLI executable: {error}");
            return EXIT_IO;
        }
    };
    let args = [
        runner.as_os_str().to_owned(),
        "--solutions".into(),
        solutions.into_os_string(),
        "--star-cli".into(),
        star_cli.into_os_string(),
    ];
    let mut python = Command::new("python");
    python.args(&args).current_dir(&root);
    let status = match python.status() {
        Ok(status) => status,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let mut python3 = Command::new("python3");
            python3.args(&args).current_dir(&root);
            match python3.status() {
                Ok(status) => status,
                Err(error) => {
                    eprintln!("star: unable to start Python 3.11 or newer: {error}");
                    return EXIT_IO;
                }
            }
        }
        Err(error) => {
            eprintln!("star: unable to start the OJ fixture runner: {error}");
            return EXIT_IO;
        }
    };
    match status.code() {
        Some(0) => EXIT_OK,
        Some(1) => EXIT_TEST_FAILURE,
        _ => EXIT_IO,
    }
}

fn find_project_root() -> Option<PathBuf> {
    env::current_dir()
        .ok()?
        .ancestors()
        .find(|path| path.join("tests").join("oj").join("run.py").is_file())
        .map(Path::to_path_buf)
}

fn resolve_project_entry() -> Result<PathBuf, String> {
    let current = env::current_dir()
        .map_err(|error| format!("cannot determine current directory: {error}"))?;
    let root = current
        .ancestors()
        .find(|directory| locate_manifest(directory).is_ok())
        .ok_or_else(|| {
            "no `star.toml` project found; supply a source file or run `star init`".to_owned()
        })?;
    let manifest = Manifest::read(root).map_err(|error| error.to_string())?;
    let entry = manifest.build.entry;
    if entry.is_absolute() {
        return Err("project entry must be a relative path".to_owned());
    }
    Ok(root.join(entry))
}

fn benchmark(options: BenchOptions) -> u8 {
    let input = match options.input {
        Some(path) => match read_limited_file(&path, options.execution.limits.max_memory_bytes) {
            Ok(input) => input,
            Err(code) => return code,
        },
        None => Vec::new(),
    };
    let executable = match env::current_exe() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("star: cannot locate this CLI executable: {error}");
            return EXIT_IO;
        }
    };
    let limits = &options.execution.limits;
    let mut total = Duration::ZERO;
    for iteration in 0..options.iterations {
        let mut command = Command::new(&executable);
        command
            .arg("run")
            .arg(&options.execution.path)
            .arg("--max-memory")
            .arg(limits.max_memory_bytes.to_string())
            .arg("--max-stack")
            .arg(limits.max_stack_values.to_string())
            .arg("--max-call-depth")
            .arg(limits.max_call_depth.to_string())
            .arg("--max-instructions")
            .arg(limits.max_instructions.to_string())
            .arg("--timeout")
            .arg(format!("{}ms", limits.max_execution_time.as_millis()))
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        if options.execution.strict {
            command.arg("--strict");
        }
        let started = Instant::now();
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                eprintln!("star: unable to start benchmark iteration: {error}");
                return EXIT_IO;
            }
        };
        if let Some(mut stdin) = child.stdin.take() {
            if let Err(error) = stdin.write_all(&input) {
                eprintln!("star: unable to provide benchmark input: {error}");
                let _ = child.kill();
                let _ = child.wait();
                return EXIT_IO;
            }
        }
        let output = match child.wait_with_output() {
            Ok(output) => output,
            Err(error) => {
                eprintln!("star: benchmark iteration failed: {error}");
                return EXIT_IO;
            }
        };
        total += started.elapsed();
        if !output.status.success() {
            eprint!("{}", String::from_utf8_lossy(&output.stderr));
            let code = output.status.code().unwrap_or(i32::from(EXIT_RUNTIME));
            eprintln!(
                "star: benchmark iteration {} failed with exit code {code}",
                iteration + 1
            );
            return u8::try_from(code).unwrap_or(EXIT_RUNTIME);
        }
    }
    let total_ms = total.as_secs_f64() * 1_000.0;
    println!(
        "star: bench_iterations={} total_ms={total_ms:.3} mean_ms={:.3} input_bytes={}",
        options.iterations,
        total_ms / options.iterations as f64,
        input.len()
    );
    EXIT_OK
}

fn run_file(options: RunOptions) -> u8 {
    let bytes = match read_limited_file(&options.path, options.limits.max_memory_bytes) {
        Ok(bytes) => bytes,
        Err(code) => return code,
    };
    match extension(&options.path).as_deref() {
        Some("starc") => {
            if options.strict {
                eprintln!("`--strict` applies only to `.star` source files");
                return EXIT_USAGE;
            }
            run_bytecode(&options.path, &bytes, options.limits, options.stats)
        }
        Some("star") if package_root_for(&options.path).is_some() => {
            match compile_project_source(&options.path, options.strict, options.limits.clone()) {
                Ok((bytecode, policy)) => run_bytecode_with_policy(
                    &options.path,
                    &bytecode,
                    options.limits,
                    options.stats,
                    policy,
                ),
                Err(error) => {
                    eprintln!("{}: {error}", options.path.display());
                    EXIT_SOURCE
                }
            }
        }
        Some("star") => run_source(
            &options.path,
            &bytes,
            options.strict,
            options.limits,
            options.stats,
        ),
        _ => {
            eprintln!(
                "{}: expected a `.star` or `.starc` file",
                options.path.display()
            );
            EXIT_USAGE
        }
    }
}

fn run_source(path: &Path, bytes: &[u8], strict: bool, limits: ExecutionLimits, stats: bool) -> u8 {
    let mode = if strict {
        LexMode::Strict
    } else {
        LexMode::Normal
    };
    let normalized = match normalize_source(bytes, mode) {
        Ok(source) => source,
        Err(error) => {
            eprintln!("{}: {error}", path.display());
            return EXIT_SOURCE;
        }
    };
    let program = match parse(&normalized) {
        Ok(program) => program,
        Err(error) => {
            eprintln!("{}: {error}", path.display());
            return EXIT_SOURCE;
        }
    };
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut interpreter =
        Interpreter::new(BufReader::new(stdin.lock()), stdout.lock()).with_execution_limits(limits);
    let started = Instant::now();
    let result = interpreter.execute(&program);
    if stats {
        report_stats(
            started.elapsed(),
            interpreter.peak_memory_estimate_bytes(),
            interpreter.executed_steps(),
        );
    }
    match result {
        Ok(()) => EXIT_OK,
        Err(error) => report_runtime_error(path, error),
    }
}

fn run_bytecode(path: &Path, bytes: &[u8], limits: ExecutionLimits, stats: bool) -> u8 {
    let policy = verification_policy(&limits);
    run_bytecode_with_policy(path, bytes, limits, stats, policy)
}

fn run_bytecode_with_policy(
    path: &Path,
    bytes: &[u8],
    limits: ExecutionLimits,
    stats: bool,
    policy: VerificationPolicy,
) -> u8 {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut vm = BytecodeVm::new(BufReader::new(stdin.lock()), stdout.lock(), policy)
        .with_execution_limits(limits);
    let started = Instant::now();
    let result = vm.execute(bytes);
    if stats {
        report_stats(
            started.elapsed(),
            vm.peak_memory_estimate_bytes(),
            vm.executed_steps(),
        );
    }
    match result {
        Ok(_) => EXIT_OK,
        Err(error) => report_runtime_error(path, error),
    }
}

fn report_stats(elapsed: Duration, peak_memory_estimate_bytes: usize, executed_steps: u64) {
    eprintln!(
        "star: execution_ms={:.3} peak_guest_memory_estimate_bytes={peak_memory_estimate_bytes} executed_steps={executed_steps}",
        elapsed.as_secs_f64() * 1_000.0
    );
}

fn report_runtime_error(path: &Path, error: RuntimeError) -> u8 {
    eprintln!("{}: {error}", path.display());
    match error.kind {
        RuntimeErrorKind::CompileError(_) => EXIT_SOURCE,
        RuntimeErrorKind::ExecutionLimit
        | RuntimeErrorKind::ResourceLimit(_)
        | RuntimeErrorKind::StackOverflow => EXIT_LIMIT,
        _ => EXIT_RUNTIME,
    }
}
