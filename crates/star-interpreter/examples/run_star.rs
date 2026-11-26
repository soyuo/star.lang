use std::error::Error;
use std::fs;
use std::io;

use star_interpreter::runtime::Interpreter;
use star_lexer::{normalize_source, LexMode};
use star_parser::parse;

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let path = std::env::args_os().nth(1).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: cargo run -p star-interpreter --example run_star -- <source.star>",
        )
    })?;
    let source = fs::read(path)?;
    let normalized = normalize_source(&source, LexMode::Normal)?;
    let program = parse(&normalized)?;
    let stdin = io::stdin();
    let stdout = io::stdout();
    Interpreter::new(stdin.lock(), stdout.lock()).execute(&program)?;
    Ok(())
}
