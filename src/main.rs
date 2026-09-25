use std::process::ExitCode;

fn main() -> ExitCode {
    match leg::cli::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(leg::error::LegError::SessionNotFound(session_id)) => {
            eprintln!("leg: no session found: {session_id}");
            ExitCode::FAILURE
        }
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}
