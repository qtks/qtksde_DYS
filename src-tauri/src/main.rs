// Prevents an extra console window on Windows in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::process::ExitCode;

const HELP: &str = "\
panopticon - local file index browser

USAGE:
    panopticon              Launch the desktop window.
    panopticon --version    Print the version and exit.
    panopticon --help       Show this help and exit.

The embedded query server starts only when the window launches.
The index defaults to $XDG_DATA_HOME/panopticon/index.db
(~/.local/share/panopticon/index.db); PANOPTICON_DB overrides it.

Populate an empty index with the bundled walker-index binary:
    walker-index scan --db ~/.local/share/panopticon/index.db /path/to/scan
";

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    match args.next() {
        None => {
            panopticon_lib::run();
            ExitCode::SUCCESS
        }
        Some(argument) => match argument.as_str() {
            "--version" | "-V" => {
                println!("panopticon {}", env!("CARGO_PKG_VERSION"));
                ExitCode::SUCCESS
            }
            "--help" | "-h" => {
                print!("{HELP}");
                ExitCode::SUCCESS
            }
            other => {
                eprintln!("panopticon: unknown argument: {other} (try --help)");
                ExitCode::from(2)
            }
        },
    }
}
