use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use num_bigint::{BigInt, BigUint, Sign};
use num_traits::{ToPrimitive, Zero};
use star_lexer::{normalize_source, LexMode};
use star_parser::{format_program, DataLiteral, InstructionOpcode, Program, Statement};

use super::{
    parse_source, read_limited_file, DEFAULT_FILE_BYTES, EXIT_IO, EXIT_LIMIT, EXIT_OK, EXIT_SOURCE,
    EXIT_USAGE,
};

type ToolResult<T> = Result<T, (u8, String)>;

pub fn format_command(arguments: &[String]) -> u8 {
    let (path, write) = match parse_path_options(arguments, "fmt") {
        Ok(options) => options,
        Err(message) => return report_error(EXIT_USAGE, message),
    };
    let source = match read_limited_file(&path, DEFAULT_FILE_BYTES) {
        Ok(source) => source,
        Err(code) => return code,
    };
    let program = match parse_source(&path, &source, false) {
        Ok(program) => program,
        Err(code) => return code,
    };
    let formatted = match format_program(&program) {
        Ok(formatted) => formatted,
        Err(error) => return report_error(EXIT_SOURCE, error.to_string()),
    };
    output_source(&path, write, formatted.as_bytes())
}

pub fn minify_command(arguments: &[String]) -> u8 {
    let (path, write) = match parse_path_options(arguments, "minify") {
        Ok(options) => options,
        Err(message) => return report_error(EXIT_USAGE, message),
    };
    let source = match read_limited_file(&path, DEFAULT_FILE_BYTES) {
        Ok(source) => source,
        Err(code) => return code,
    };
    if let Err(code) = parse_source(&path, &source, false) {
        return code;
    }
    let normalized = match normalize_source(&source, LexMode::Normal) {
        Ok(normalized) => normalized,
        Err(error) => return report_error(EXIT_SOURCE, format!("{}: {error}", path.display())),
    };
    output_source(&path, write, normalized.as_bytes())
}

pub fn encode_command(arguments: &[String]) -> u8 {
    match encode(arguments) {
        Ok(source) => match io::stdout().write_all(source.as_bytes()) {
            Ok(()) => EXIT_OK,
            Err(error) => report_error(EXIT_IO, format!("cannot write encoded source: {error}")),
        },
        Err((code, message)) => report_error(code, message),
    }
}

pub fn decode_command(arguments: &[String]) -> u8 {
    let path = match arguments {
        [path] if !path.starts_with('-') => PathBuf::from(path),
        _ => return report_error(EXIT_USAGE, "usage: star decode <source-file>".to_owned()),
    };
    let source = match read_limited_file(&path, DEFAULT_FILE_BYTES) {
        Ok(source) => source,
        Err(code) => return code,
    };
    let program = match parse_source(&path, &source, false) {
        Ok(program) => program,
        Err(code) => return code,
    };
    match decode_program(&program) {
        Ok(value) => {
            println!("{value}");
            EXIT_OK
        }
        Err(message) => report_error(EXIT_SOURCE, message),
    }
}

fn parse_path_options(arguments: &[String], command: &str) -> Result<(PathBuf, bool), String> {
    let mut path = None;
    let mut write = false;
    for argument in arguments {
        if argument == "--write" {
            if write {
                return Err(format!(
                    "`star {command}` received `--write` more than once"
                ));
            }
            write = true;
        } else if argument.starts_with('-') || path.is_some() {
            return Err(format!("usage: star {command} <source-file> [--write]"));
        } else {
            path = Some(PathBuf::from(argument));
        }
    }
    path.map(|path| (path, write))
        .ok_or_else(|| format!("usage: star {command} <source-file> [--write]"))
}

fn output_source(path: &Path, write: bool, source: &[u8]) -> u8 {
    if write {
        return match fs::write(path, source) {
            Ok(()) => {
                eprintln!("{}: source updated", path.display());
                EXIT_OK
            }
            Err(error) => report_error(EXIT_IO, format!("{}: {error}", path.display())),
        };
    }
    match io::stdout().write_all(source) {
        Ok(()) => EXIT_OK,
        Err(error) => report_error(EXIT_IO, format!("cannot write source output: {error}")),
    }
}

fn encode(arguments: &[String]) -> ToolResult<String> {
    let [mode, value] = arguments else {
        return Err((
            EXIT_USAGE,
            "usage: star encode (--int N | --float N | --string TEXT | --file PATH | --opcode NAME)"
                .to_owned(),
        ));
    };
    let source = match mode.as_str() {
        "--int" => {
            let integer = BigInt::from_str(value)
                .map_err(|_| (EXIT_USAGE, format!("invalid decimal integer `{value}`")))?;
            let payload_bits = usize::try_from(integer.magnitude().bits())
                .unwrap_or(usize::MAX)
                .max(1);
            let suffix_bytes = if integer.sign() == Sign::Minus { 16 } else { 1 };
            if payload_bits.saturating_add(suffix_bytes + 2) > DEFAULT_FILE_BYTES {
                return Err((
                    EXIT_LIMIT,
                    "encoded source would exceed the CLI source-size limit".to_owned(),
                ));
            }
            let mut tokens = vec![encode_data_literal(integer.magnitude(), 0)];
            if integer.sign() == Sign::Minus {
                tokens.push(opcode_spelling("NEG")?);
            }
            join_tokens(tokens)
        }
        "--float" => {
            let number = value
                .parse::<f64>()
                .map_err(|_| (EXIT_USAGE, format!("invalid decimal float `{value}`")))?;
            if !number.is_finite() {
                return Err((
                    EXIT_USAGE,
                    "`star encode --float` requires a finite decimal value".to_owned(),
                ));
            }
            let bits = BigUint::from(number.to_bits());
            join_tokens(vec![
                encode_data_literal(&bits, 64),
                opcode_spelling("FLOAT_FROM_BITS")?,
            ])
        }
        "--string" => encode_bytes(value.as_bytes(), true)?,
        "--file" => {
            let path = PathBuf::from(value);
            let bytes = match read_limited_file(&path, DEFAULT_FILE_BYTES) {
                Ok(bytes) => bytes,
                Err(code) => return Err((code, String::new())),
            };
            encode_bytes(&bytes, false)?
        }
        "--opcode" => {
            let opcode = InstructionOpcode::from_mnemonic(value).ok_or_else(|| {
                (
                    EXIT_USAGE,
                    format!("unknown assigned source opcode `{value}`"),
                )
            })?;
            let mnemonic = opcode.mnemonic().unwrap_or_default();
            if is_structural_mnemonic(mnemonic) {
                return Err((
                    EXIT_USAGE,
                    format!("structural opcode `{mnemonic}` must be encoded as part of a block"),
                ));
            }
            format!(
                "{}\n",
                opcode
                    .source_spelling()
                    .expect("assigned opcode has spelling")
            )
        }
        _ => {
            return Err((
                EXIT_USAGE,
                "usage: star encode (--int N | --float N | --string TEXT | --file PATH | --opcode NAME)"
                    .to_owned(),
            ));
        }
    };
    Ok(source)
}

fn encode_bytes(bytes: &[u8], decode_utf8: bool) -> ToolResult<String> {
    let payload_bits = significant_payload_bits(bytes);
    let length_bits = (usize::BITS as usize - bytes.len().leading_zeros() as usize).max(1);
    let output_bound = payload_bits.saturating_add(length_bits).saturating_add(64);
    if output_bound > DEFAULT_FILE_BYTES {
        return Err((
            EXIT_LIMIT,
            "encoded source would exceed the CLI source-size limit".to_owned(),
        ));
    }

    let payload = BigUint::from_bytes_be(bytes);
    let length = BigUint::from(bytes.len());
    let mut tokens = vec![
        encode_data_literal(&payload, 0),
        encode_data_literal(&length, 0),
    ];
    tokens.push(opcode_spelling("INT_TO_BYTES")?);
    if decode_utf8 {
        tokens.push(opcode_spelling("UTF8_DECODE")?);
    }
    Ok(join_tokens(tokens))
}

fn significant_payload_bits(bytes: &[u8]) -> usize {
    let Some(first_nonzero) = bytes.iter().position(|byte| *byte != 0) else {
        return 1;
    };
    (bytes.len() - first_nonzero - 1) * 8
        + (u8::BITS as usize - bytes[first_nonzero].leading_zeros() as usize)
}

fn encode_data_literal(value: &BigUint, minimum_width: usize) -> String {
    let bits = value.to_str_radix(2);
    let width = bits.len().max(minimum_width);
    let mut payload = String::with_capacity(width);
    payload.extend(std::iter::repeat('\'').take(width - bits.len()));
    payload.extend(bits.chars().map(|bit| if bit == '1' { '*' } else { '\'' }));
    format!("\"{payload}\"")
}

fn opcode_spelling(mnemonic: &str) -> ToolResult<String> {
    let opcode = InstructionOpcode::from_mnemonic(mnemonic).ok_or_else(|| {
        (
            EXIT_SOURCE,
            format!("opcode catalog is missing `{mnemonic}`"),
        )
    })?;
    opcode.source_spelling().ok_or_else(|| {
        (
            EXIT_SOURCE,
            format!("opcode catalog has no spelling for `{mnemonic}`"),
        )
    })
}

fn join_tokens(tokens: Vec<String>) -> String {
    let mut source = tokens.join("\n");
    source.push('\n');
    source
}

fn is_structural_mnemonic(mnemonic: &str) -> bool {
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
}

fn decode_program(program: &Program) -> Result<String, String> {
    let statements = &program.statements;
    if let [Statement::DataLiteral(literal)] = statements.as_slice() {
        return Ok(format!("Int({})", bits_to_biguint(literal)));
    }
    if let [Statement::DataLiteral(literal), instruction] = statements.as_slice() {
        if instruction_is(instruction, "NEG") {
            let value = bits_to_biguint(literal);
            let signed = BigInt::from_biguint(Sign::Minus, value);
            return Ok(format!("Int({signed})"));
        }
        if instruction_is(instruction, "FLOAT_FROM_BITS") {
            if literal.bits.len() != 64 {
                return Err(
                    "FLOAT_FROM_BITS encoding must contain exactly 64 payload bits".to_owned(),
                );
            }
            let raw = bits_to_biguint(literal)
                .to_u64()
                .ok_or_else(|| "float payload does not fit 64 bits".to_owned())?;
            let value = f64::from_bits(raw);
            return Ok(format!("Float(bits=0x{raw:016X}, value={value})"));
        }
    }
    if let [Statement::DataLiteral(payload), Statement::DataLiteral(length), instruction]
    | [Statement::DataLiteral(payload), Statement::DataLiteral(length), instruction, _] =
        statements.as_slice()
    {
        if instruction_is(instruction, "INT_TO_BYTES") {
            let bytes = decode_bytes(payload, length)?;
            if statements.len() == 3 {
                return Ok(format!("Bytes({})", format_hex(&bytes)));
            }
            if let [_, _, _, decode] = statements.as_slice() {
                if instruction_is(decode, "UTF8_DECODE") {
                    let text = std::str::from_utf8(&bytes)
                        .map_err(|error| format!("byte payload is not valid UTF-8: {error}"))?;
                    return Ok(format!("String({text:?})"));
                }
            }
        }
    }
    if let [statement @ Statement::Instruction(_)] = statements.as_slice() {
        if let Statement::Instruction(instruction) = statement {
            return instruction
                .opcode
                .mnemonic()
                .map(|mnemonic| format!("Opcode({mnemonic})"))
                .ok_or_else(|| "source opcode has no assigned mnemonic".to_owned());
        }
    }
    Err("decode accepts one encoded integer, float, byte/string value, or opcode token".to_owned())
}

fn bits_to_biguint(literal: &DataLiteral) -> BigUint {
    literal.bits.iter().fold(BigUint::zero(), |value, bit| {
        (value << 1usize) + BigUint::from(u8::from(*bit == 1))
    })
}

fn decode_bytes(payload: &DataLiteral, length: &DataLiteral) -> Result<Vec<u8>, String> {
    let value = bits_to_biguint(payload);
    let length = bits_to_biguint(length)
        .to_usize()
        .ok_or_else(|| "byte length does not fit this platform".to_owned())?;
    if length > DEFAULT_FILE_BYTES {
        return Err("decoded byte payload exceeds the CLI file-size limit".to_owned());
    }
    if length == 0 {
        return if value.is_zero() {
            Ok(Vec::new())
        } else {
            Err("a zero-length byte payload must have value zero".to_owned())
        };
    }
    let encoded = value.to_bytes_be();
    if encoded.len() > length {
        return Err("byte payload does not fit the encoded length".to_owned());
    }
    let mut bytes = vec![0; length];
    bytes[length - encoded.len()..].copy_from_slice(&encoded);
    Ok(bytes)
}

fn format_hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn instruction_is(statement: &Statement, mnemonic: &str) -> bool {
    matches!(statement, Statement::Instruction(instruction) if instruction.opcode.mnemonic() == Some(mnemonic))
}

fn report_error(code: u8, message: String) -> u8 {
    if !message.is_empty() {
        eprintln!("star: {message}");
    }
    code
}
