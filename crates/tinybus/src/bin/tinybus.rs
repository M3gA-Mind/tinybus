//! The TinyBus CLI uses Unix domain sockets; other platforms fail explicitly.

#[cfg(unix)]
#[path = "tinybus/unix.rs"]
mod unix;

#[cfg(unix)]
fn main() -> std::process::ExitCode {
    unix::main()
}

#[cfg(not(unix))]
fn main() -> std::process::ExitCode {
    eprintln!("tinybus CLI requires Unix domain sockets; this platform is unsupported");
    std::process::ExitCode::FAILURE
}

#[cfg(all(test, not(unix)))]
#[path = "tinybus_platform_tests.rs"]
mod tests;
