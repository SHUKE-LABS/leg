use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use leg_ui_client::SessionCatalogConfig;
use leg_web::{DEFAULT_PORT, Host, HostConfig};

const SAFETY_WARNING: &str = "Leg can run shell commands and modify files as your OS user. The workspace is its working directory, not a sandbox.";

#[derive(Default)]
struct Args {
    bind: Option<SocketAddr>,
    state_dir: Option<PathBuf>,
    leg_bin: Option<PathBuf>,
    supervisor_bin: Option<PathBuf>,
    event_buffer: Option<usize>,
    receipt_limit: Option<usize>,
    no_open: bool,
    help: bool,
}

#[tokio::main]
async fn main() {
    if let Err(error) = entry().await {
        eprintln!("leg-web: {error}");
        std::process::exit(1);
    }
}

async fn entry() -> Result<(), Box<dyn std::error::Error>> {
    let args = parse_args()?;
    if args.help {
        print_help();
        return Ok(());
    }
    let defaults = HostConfig::default();
    let config = HostConfig {
        bind_addr: args.bind.unwrap_or(defaults.bind_addr),
        catalog: SessionCatalogConfig {
            state_dir: args.state_dir,
            leg_bin: args.leg_bin,
            supervisor_bin: args.supervisor_bin,
        },
        event_buffer: args.event_buffer.unwrap_or(defaults.event_buffer),
        receipt_limit: args.receipt_limit.unwrap_or(defaults.receipt_limit),
    };
    let host = Host::open(config)?.bind().await?;
    let url = host.launch_url();
    println!("Leg Web is listening on http://{}", host.local_addr());
    println!("Open this one-time launch URL: {url}");
    if !args.no_open
        && let Err(error) = open_browser(&url)
    {
        eprintln!("leg-web: could not open a browser ({error}); use the printed URL");
    }
    host.serve(async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await?;
    Ok(())
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args::default();
    let mut values = std::env::args_os().skip(1);
    while let Some(argument) = values.next() {
        match argument.to_str() {
            Some("--help" | "-h") => args.help = true,
            Some("--no-open") => args.no_open = true,
            Some("--bind") => {
                let value = values.next().ok_or("--bind requires an address")?;
                args.bind = Some(value.to_string_lossy().parse().map_err(
                    |_| "--bind must be 127.0.0.1:<port>; use port 0 for any free port",
                )?);
            }
            Some("--state-dir") => {
                args.state_dir = Some(PathBuf::from(
                    values.next().ok_or("--state-dir requires a path")?,
                ));
            }
            Some("--leg-bin") => {
                args.leg_bin = Some(PathBuf::from(
                    values.next().ok_or("--leg-bin requires a path")?,
                ));
            }
            Some("--supervisor-bin") => {
                args.supervisor_bin = Some(PathBuf::from(
                    values.next().ok_or("--supervisor-bin requires a path")?,
                ));
            }
            Some("--event-buffer") => {
                args.event_buffer = Some(parse_positive(
                    "--event-buffer",
                    values.next().ok_or("--event-buffer requires a count")?,
                )?);
            }
            Some("--receipt-limit") => {
                args.receipt_limit = Some(parse_positive(
                    "--receipt-limit",
                    values.next().ok_or("--receipt-limit requires a count")?,
                )?);
            }
            _ => return Err(format!("unknown argument {argument:?}; use --help")),
        }
    }
    Ok(args)
}

fn print_help() {
    println!(
        "leg-web — authenticated loopback host for leg companion sessions\n\n\
         Usage: leg-web [--no-open] [--bind 127.0.0.1:PORT] [--state-dir PATH]\n\
         [--leg-bin PATH] [--supervisor-bin PATH] [--event-buffer N]\n\
         [--receipt-limit N]\n\n\
         Listens on 127.0.0.1:{DEFAULT_PORT} by default; --bind 127.0.0.1:0 picks\n\
         any free port.\n\n\
         The launch token is printed once in a URL fragment, then moved into\n\
         per-tab session storage by the embedded page.\n\n\
         {SAFETY_WARNING}"
    );
}

fn parse_positive(name: &str, value: std::ffi::OsString) -> Result<usize, String> {
    let value = value
        .to_str()
        .ok_or_else(|| format!("{name} must be a positive integer"))?;
    let parsed = value
        .parse::<usize>()
        .map_err(|_| format!("{name} must be a positive integer"))?;
    if parsed == 0 {
        return Err(format!("{name} must be a positive integer"));
    }
    Ok(parsed)
}

fn open_browser(url: &str) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    let mut command = Command::new("open");
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut command = Command::new("cmd");
        command.args(["/C", "start", ""]);
        command
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut command = Command::new("xdg-open");
    command
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::SAFETY_WARNING;

    #[test]
    fn help_warning_matches_the_workspace_contract() {
        assert_eq!(
            SAFETY_WARNING,
            "Leg can run shell commands and modify files as your OS user. The workspace is its working directory, not a sandbox."
        );
    }
}
