fn main() {
    if let Err(error) = vanity_create2::app::run() {
        eprintln!("\nError: {error:#}");
        std::process::exit(1);
    }
}
