#![no_main]

use libfuzzer_sys::fuzz_target;
use star_lexer::{normalize_source, LexMode};
use star_parser::parse;

fuzz_target!(|data: &[u8]| {
    for mode in [LexMode::Normal, LexMode::Strict] {
        if let Ok(source) = normalize_source(data, mode) {
            let _ = parse(&source);
        }
    }
});
