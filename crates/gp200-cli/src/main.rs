fn main() -> std::process::ExitCode {
    gp200_cli::main_entry(std::env::args_os().skip(1).collect())
}
