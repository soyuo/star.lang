#![no_main]

use std::collections::BTreeSet;

use libfuzzer_sys::fuzz_target;
use star_compiler::{verify_bytecode, VerificationLimits, VerificationPolicy};

fuzz_target!(|data: &[u8]| {
    let policy = VerificationPolicy {
        limits: VerificationLimits {
            max_file_bytes: 1 << 20,
            max_table_entries: 4096,
            max_instructions: 100_000,
            max_stack_depth: 4096,
        },
        granted_capabilities: BTreeSet::new(),
    };
    let _ = verify_bytecode(data, &policy);
});
