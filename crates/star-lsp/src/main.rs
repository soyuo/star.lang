fn main() {
    if let Err(error) = star_lsp::run() {
        eprintln!("star-lsp: {error}");
        std::process::exit(1);
    }
}
