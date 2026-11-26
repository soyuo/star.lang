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
    let mut in_extended_section = false;
    let mut opcodes = Vec::new();

    for line in catalog.lines().map(str::trim) {
        if line.starts_with("[[") {
            in_extended_section = line == "[[extended]]";
            continue;
        }

        if in_extended_section {
            if let Some(value) = line.strip_prefix("opcode =") {
                let value = value.trim();
                let hex = value
                    .strip_prefix("0x")
                    .unwrap_or_else(|| panic!("invalid extended opcode value: {value}"));
                let opcode = u8::from_str_radix(hex, 16)
                    .unwrap_or_else(|_| panic!("invalid extended opcode value: {value}"));
                opcodes.push(opcode);
            }
        }
    }

    assert!(
        !opcodes.is_empty(),
        "opcode catalog has no extended instructions"
    );
    opcodes.sort_unstable();
    let original_count = opcodes.len();
    opcodes.dedup();
    assert_eq!(
        opcodes.len(),
        original_count,
        "opcode catalog contains duplicate extended opcode values"
    );

    let values = opcodes
        .iter()
        .map(|opcode| format!("0x{opcode:02X}"))
        .collect::<Vec<_>>()
        .join(", ");
    let generated = format!("pub const ASSIGNED_EXTENDED_OPCODES: &[u8] = &[{values}];\n");
    let output_path = PathBuf::from(env::var_os("OUT_DIR").expect("output directory"))
        .join("assigned_opcodes.rs");
    fs::write(&output_path, generated)
        .unwrap_or_else(|error| panic!("failed to write {}: {error}", output_path.display()));
}
