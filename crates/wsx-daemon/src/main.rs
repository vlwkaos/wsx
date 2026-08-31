fn main() -> std::process::ExitCode {
    match wsx_daemon::run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("wsxd: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}
