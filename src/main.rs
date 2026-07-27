fn main() {
    if let Err(error) = vanity::app::run() {
        eprintln!("\nError: {error:#}");
        std::process::exit(1);
    }
}
