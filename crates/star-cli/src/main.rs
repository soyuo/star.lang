use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::io::{self, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

mod source_lowering;
mod source_tools;

use star_compiler::{
    compile, disassemble_bytecode, verify_bytecode, Capability, VerificationLimits,
    VerificationPolicy,
};
use star_interpreter::{
    runtime::{
        stdio::InputError, BytecodeVm, ExecutionLimits, Interpreter, RuntimeError, RuntimeErrorKind,
    },
    semantic::analyze,
};
use star_lexer::{normalize_source, LexMode};
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
  star check [<file.star|file.starc>] [options]
  star build [<file.star>] [--output <file.starc>] [options]
  star run [<file.star|file.starc>] [options]
  star test --suite oj [--solutions <directory>]
  star bench [<file.star|file.starc>] --iterations <count> [--input <file>]
  star fmt <file.star> [--write]
  star minify <file.star> [--write]
  star encode (--int N | --float N | --string TEXT | --file PATH | --opcode NAME)
  star decode <encoded-source.star>
  star disasm <file.starc>
  star repl
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
`build` compiles supported `.star` source to `.starc`; TRY/CATCH lowering is not
yet supported. `test --suite oj` requires paired solutions and Python 3.11+.
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
    strict: bool,
    limits: ExecutionLimits,
}

struct TestOptions {
    solutions: Option<PathBuf>,
}

struct BenchOptions {
    execution: RunOptions,
    iterations: usize,
    input: Option<PathBuf>,
}

fn main() -> ExitCode {
    ExitCode::from(run(env::args().skip(1).collect()))
}

fn run(args: Vec<String>) -> u8 {
    let Some(command) = args.first().map(String::as_str) else {
        eprint!("{HELP}");
        return EXIT_USAGE;
    };
    match command {
        "help" | "--help" | "-h" => {
            print!("{HELP}");
            EXIT_OK
        }
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
        "run" => match parse_run_options(&args[1..]) {
            Ok(options) => run_file(options),
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
    let manifest = directory.join("Star.toml");
    let source_dir = directory.join("src");
    let source = source_dir.join("main.star");
    let gitignore = directory.join(".gitignore");
    for path in [&manifest, &source, &gitignore] {
        if path.exists() {
            eprintln!("{}: project file already exists", path.display());
            return EXIT_IO;
        }
    }
    if create_directory && directory.exists() {
        eprintln!("{}: project directory already exists", directory.display());
        return EXIT_IO;
    }
    if let Err(error) = fs::create_dir_all(&source_dir) {
        eprintln!("{}: {error}", source_dir.display());
        return EXIT_IO;
    }
    let source_bits = "010010000110010101101100011011000110111100101100001000000101001101110100011000010111001000100001";
    let source_text = format!("\"{source_bits}\"******'\n");
    let manifest_text = format!(
        "schema_version = 1\n\n[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nentry = \"src/main.star\"\n"
    );
    for (path, contents) in [
        (&manifest, manifest_text.as_bytes()),
        (&source, source_text.as_bytes()),
        (&gitignore, b"/target/\n/src/*.starc\n".as_slice()),
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
    Ok(BuildOptions {
        path: match path {
            Some(path) => path,
            None => resolve_project_entry()
                .map_err(|error| format!("missing source file for `star build`: {error}"))?,
        },
        output,
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
            let program = match parse_source(&options.path, &bytes, options.strict) {
                Ok(program) => program,
                Err(code) => return code,
            };
            match compile_source_program(&options.path, &program, options.limits) {
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
    let bytes = match read_limited_file(&options.path, options.limits.max_memory_bytes) {
        Ok(bytes) => bytes,
        Err(code) => return code,
    };
    let program = match parse_source(&options.path, &bytes, options.strict) {
        Ok(program) => program,
        Err(code) => return code,
    };
    let bytecode = match compile_source_program(&options.path, &program, options.limits) {
        Ok(bytecode) => bytecode,
        Err(error) => {
            eprintln!("{}: {error}", options.path.display());
            return EXIT_SOURCE;
        }
    };
    let output = options.output.unwrap_or_else(|| {
        let mut output = options.path.clone();
        output.set_extension("starc");
        output
    });
    if let Err(error) = fs::write(&output, bytecode) {
        eprintln!("{}: {error}", output.display());
        return EXIT_IO;
    }
    println!("built {}", output.display());
    EXIT_OK
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
        .find(|directory| directory.join("Star.toml").is_file())
        .ok_or_else(|| {
            "no `Star.toml` project found; supply a source file or run `star init`".to_owned()
        })?;
    let manifest_path = root.join("Star.toml");
    let manifest = fs::read_to_string(&manifest_path)
        .map_err(|error| format!("cannot read {}: {error}", manifest_path.display()))?;
    let entry = manifest
        .lines()
        .map(str::trim)
        .find_map(|line| line.strip_prefix("entry ="))
        .map(str::trim)
        .map(|value| value.trim_matches('"'))
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("{} has no nonempty `entry` field", manifest_path.display()))?;
    let entry = PathBuf::from(entry);
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
    let max_stack_depth = u32::try_from(limits.max_stack_values).unwrap_or(u32::MAX);
    let policy = VerificationPolicy {
        limits: VerificationLimits {
            max_file_bytes: limits.max_memory_bytes.min(DEFAULT_FILE_BYTES),
            max_table_entries: 65_536,
            max_instructions: limits.max_instructions,
            max_stack_depth,
        },
        granted_capabilities: BTreeSet::new(),
    };
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
