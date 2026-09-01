fn main() -> std::process::ExitCode {
    let mut arguments = std::env::args_os();
    let _program = arguments.next();
    let result = match arguments.next() {
        Some(argument) if argument == wsx_daemon::RESUME_SUPERVISOR_ARG => {
            wsx_daemon::run_resume_supervisor(arguments)
        }
        Some(_) => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "unexpected wsxd argument",
        )),
        None => wsx_daemon::run(),
    };
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("wsxd: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}
