use std::env;
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut args = env::args().skip(1);
    if let Some(arg) = args.next()
        && (arg == "--version" || arg == "-V")
    {
        println!("leg {}", env!("CARGO_PKG_VERSION"));
    }
    ExitCode::SUCCESS
}
