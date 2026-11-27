use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::io::{self, BufReader};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use star_compiler::{VerificationLimits, VerificationPolicy};
use star_interpreter::runtime::{
    BytecodeVm, ExecutionLimits, Interpreter, RuntimeError, RuntimeErrorKind,
};
use star_lexer::{normalize_source, LexMode};
use star_parser::parse;

const EXIT_OK: u8 = 0;
const EXIT_USAGE: u8 = 2;
const EXIT_IO: u8 = 3;
const EXIT_SOURCE: u8 = 4;
const EXIT_RUNTIME: u8 = 5;
const EXIT_LIMIT: u8 = 6;
const DEFAULT_FILE_BYTES: usize = 64 * 1024 * 1024;

const HELP: &str = "\
Star command line

Usage:
  star run <file.star|file.starc> [options]
  star help

Run options:
  --strict                    Reject formatting whitespace in source
  --stats                     Write execution time and guest-memory estimate to stderr
  --max-memory <bytes|K|M|G>  Guest memory ceiling (default: 256M)
  --max-stack <count>         Operand stack ceiling (default: 65536)
  --max-call-depth <count>    Call depth ceiling (default: 512)
  --max-instructions <count>  Instruction ceiling (default: 10000000)
  --timeout <duration>        Wall-clock limit, e.g. 30s or 500ms (default: 30s)

Standard input and output are streamed without tokenization by the CLI. The
program controls input reads; output bytes and whitespace are preserved.

Stable exit codes:
  0 success; 2 usage/options; 3 file I/O; 4 source or bytecode diagnostics;
  5 runtime failure; 6 configured resource limit.
";

struct RunOptions {
    path: PathBuf,
    strict: bool,
    stats: bool,
    limits: ExecutionLimits,
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
        "run" => match parse_run_options(&args[1..]) {
            Ok(options) => run_file(options),
            Err(message) => {
                eprintln!("star: {message}\n\n{HELP}");
                EXIT_USAGE
            }
        },
        other => {
            eprintln!("star: unknown command `{other}`\n\n{HELP}");
            EXIT_USAGE
        }
    }
}

fn parse_run_options(arguments: &[String]) -> Result<RunOptions, String> {
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
            stats = true;
            index += 1;
            continue;
        }
        let option = argument.as_str();
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
            return Err(format!("unknown run option `{argument}`"));
        }
        if path.replace(PathBuf::from(argument)).is_some() {
            return Err("`star run` accepts exactly one input file".to_owned());
        }
        index += 1;
    }
    Ok(RunOptions {
        path: path.ok_or_else(|| "missing input file for `star run`".to_owned())?,
        strict,
        stats,
        limits,
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

fn run_file(options: RunOptions) -> u8 {
    let metadata = match fs::metadata(&options.path) {
        Ok(metadata) => metadata,
        Err(error) => {
            eprintln!("{}: {error}", options.path.display());
            return EXIT_IO;
        }
    };
    let max_file_bytes = options.limits.max_memory_bytes.min(DEFAULT_FILE_BYTES);
    if metadata.len() > max_file_bytes as u64 {
        eprintln!(
            "{}: input file exceeds the {max_file_bytes}-byte CLI file limit",
            options.path.display()
        );
        return EXIT_LIMIT;
    }
    let bytes = match fs::read(&options.path) {
        Ok(bytes) => bytes,
        Err(error) => {
            eprintln!("{}: {error}", options.path.display());
            return EXIT_IO;
        }
    };
    let extension = options
        .path
        .extension()
        .and_then(|extension| extension.to_str());
    if extension.is_some_and(|extension| extension.eq_ignore_ascii_case("starc")) {
        if options.strict {
            eprintln!("`--strict` applies only to `.star` source files");
            return EXIT_USAGE;
        }
        run_bytecode(&options.path, &bytes, options.limits, options.stats)
    } else if extension.is_some_and(|extension| extension.eq_ignore_ascii_case("star")) {
        run_source(
            &options.path,
            &bytes,
            options.strict,
            options.limits,
            options.stats,
        )
    } else {
        eprintln!(
            "{}: expected a `.star` or `.starc` file",
            options.path.display()
        );
        EXIT_USAGE
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
