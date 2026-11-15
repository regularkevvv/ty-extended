//! Compatibility entry point. The application itself is the adjacent `tyx` binary.
use std::process::{Command, ExitCode};

fn run() -> std::io::Result<()> {
    let executable = std::env::current_exe()?;
    let target = executable.with_file_name(if cfg!(windows) { "tyx.exe" } else { "tyx" });
    let mut command = Command::new(target);
    command.args(std::env::args_os().skip(1));

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        Err(command.exec())
    }
    #[cfg(not(unix))]
    {
        // Let the child handle Ctrl+C while the launcher waits for its exit code.
        ctrlc::set_handler(|| {}).map_err(std::io::Error::other)?;
        let status = command.status()?;
        std::process::exit(status.code().unwrap_or(2));
    }
}

fn main() -> ExitCode {
    if std::env::args_os()
        .nth(1)
        .is_some_and(|arg| arg == "--version" || arg == "-V")
    {
        println!("ty-extended {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    if let Err(error) = run() {
        eprintln!("ty-extended: could not start adjacent tyx executable: {error}");
    }
    ExitCode::from(2)
}
