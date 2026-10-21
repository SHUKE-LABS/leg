fn main() {
    let mut args = std::env::args_os();
    let _ = args.next();
    let first = args.next();
    if first.as_deref() == Some(std::ffi::OsStr::new("--version"))
        || first.as_deref() == Some(std::ffi::OsStr::new("-V"))
    {
        println!("leg-ui-supervisor {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    if first.as_deref() != Some(std::ffi::OsStr::new("--internal-supervisor"))
        || args.next().is_some()
    {
        eprintln!("leg-ui-supervisor: internal helper; launch it through leg-ui-client");
        std::process::exit(2);
    }
    let code = match leg_ui_client::supervisor::run_from_stdin() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("leg-ui-supervisor: {error}");
            1
        }
    };
    std::process::exit(code.clamp(0, 255));
}
