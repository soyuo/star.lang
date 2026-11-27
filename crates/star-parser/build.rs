use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    let manifest_dir =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("manifest directory"));
    let catalog_path = manifest_dir.join("../../spec/opcodes.toml");
    println!("cargo:rerun-if-changed={}", catalog_path.display());

    let catalog = fs::read_to_string(&catalog_path)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", catalog_path.display()));
    let mut basic = parse_instructions(&catalog, "basic", "ordinal", false);
    let mut extended = parse_instructions(&catalog, "extended", "opcode", true);
    sort_and_check(&mut basic, "basic ordinal");
    sort_and_check(&mut extended, "extended opcode");

    let basic_mnemonics = format_mapping(&basic, false);
    let extended_values = extended
        .iter()
        .map(|(opcode, _)| format!("0x{opcode:02X}"))
        .collect::<Vec<_>>()
        .join(", ");
    let extended_mnemonics = format_mapping(&extended, true);
    let generated = format!(
        "pub const ASSIGNED_EXTENDED_OPCODES: &[u8] = &[{extended_values}];\npub const BASIC_OPCODE_MNEMONICS: &[(u8, &str)] = &[{basic_mnemonics}];\npub const EXTENDED_OPCODE_MNEMONICS: &[(u8, &str)] = &[{extended_mnemonics}];\n"
    );
    let output_path = PathBuf::from(env::var_os("OUT_DIR").expect("output directory"))
        .join("assigned_opcodes.rs");
    fs::write(&output_path, generated)
        .unwrap_or_else(|error| panic!("failed to write {}: {error}", output_path.display()));
}

fn parse_instructions(
    catalog: &str,
    table: &str,
    number_key: &str,
    hexadecimal: bool,
) -> Vec<(u8, String)> {
    let table_header = format!("[[{table}]]");
    let mut active = false;
    let mut number = None;
    let mut mnemonic = None;
    let mut instructions = Vec::new();

    for line in catalog.lines().map(str::trim) {
        if line.starts_with("[[") {
            if active {
                push_record(&mut instructions, number.take(), mnemonic.take(), table);
            }
            active = line == table_header;
            continue;
        }
        if line.starts_with('[') {
            if active {
                push_record(&mut instructions, number.take(), mnemonic.take(), table);
            }
            active = false;
            continue;
        }
        if !active {
            continue;
        }
        if let Some(value) = line.strip_prefix(&format!("{number_key} =")) {
            let value = value.trim();
            let parsed = if hexadecimal {
                value
                    .strip_prefix("0x")
                    .and_then(|digits| u8::from_str_radix(digits, 16).ok())
            } else {
                value.parse::<u8>().ok()
            };
            number =
                Some(parsed.unwrap_or_else(|| panic!("invalid {number_key} in {table}: {value}")));
        } else if let Some(value) = line.strip_prefix("mnemonic =") {
            mnemonic = Some(value.trim().trim_matches('"').to_owned());
        }
    }
    if active {
        push_record(&mut instructions, number, mnemonic, table);
    }
    instructions
}

fn push_record(
    instructions: &mut Vec<(u8, String)>,
    number: Option<u8>,
    mnemonic: Option<String>,
    table: &str,
) {
    let number = number.unwrap_or_else(|| panic!("{table} opcode record has no numeric value"));
    let mnemonic = mnemonic.unwrap_or_else(|| panic!("{table} opcode {number} has no mnemonic"));
    instructions.push((number, mnemonic));
}

fn sort_and_check(instructions: &mut Vec<(u8, String)>, label: &str) {
    assert!(!instructions.is_empty(), "opcode catalog has no {label}s");
    instructions.sort_unstable_by_key(|(number, _)| *number);
    let original_count = instructions.len();
    instructions.dedup_by_key(|(number, _)| *number);
    assert_eq!(
        instructions.len(),
        original_count,
        "opcode catalog contains duplicate {label} values"
    );
}

fn format_mapping(instructions: &[(u8, String)], hexadecimal: bool) -> String {
    instructions
        .iter()
        .map(|(number, mnemonic)| {
            let number = if hexadecimal {
                format!("0x{number:02X}")
            } else {
                number.to_string()
            };
            format!("({number}, \"{mnemonic}\")")
        })
        .collect::<Vec<_>>()
        .join(", ")
}
