use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;

use walker_core::{scan, ScanOptions};

const HELP: &str = "\
walker-scan - Panopticon M0 read-only scanner

USAGE:
    walker-scan [OPTIONS] [PATH]...

ARGS:
    PATH                    Directory or file to scan. Defaults to $HOME.

OPTIONS:
    -e, --exclude PATTERN   Add an exclusion rule (repeatable).
                            Absolute rules match path prefixes; other rules
                            match slash-separated path components.
        --no-default-excludes
                            Disable /proc, /sys, node_modules, .git/objects.
        --follow-links      Follow directory symlinks with inode cycle detection.
        --threads N         Restrict the rayon worker pool to N threads.
    -o, --output FILE       Write the JSON report to FILE instead of stdout.
    -h, --help              Show this help.
";

struct Cli {
    roots: Vec<PathBuf>,
    extra_excludes: Vec<String>,
    use_default_excludes: bool,
    follow_links: bool,
    threads: Option<usize>,
    output: Option<PathBuf>,
    help: bool,
}

fn main() {
    if let Err(message) = run() {
        eprintln!("walker-scan: {message}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let cli = parse_args()?;
    if cli.help {
        print!("{HELP}");
        return Ok(());
    }

    let options = ScanOptions {
        roots: cli.roots,
        extra_excludes: cli.extra_excludes,
        use_default_excludes: cli.use_default_excludes,
        follow_links: cli.follow_links,
        threads: cli.threads,
        ..ScanOptions::default()
    };

    let report = scan(&options);
    let json = serde_json::to_string_pretty(&report).map_err(|error| error.to_string())?;

    match cli.output {
        Some(path) => fs::write(&path, json)
            .map_err(|error| format!("cannot write report to {}: {error}", path.display())),
        None => {
            println!("{json}");
            Ok(())
        }
    }
}

fn parse_args() -> Result<Cli, String> {
    let mut roots = Vec::new();
    let mut extra_excludes = Vec::new();
    let mut use_default_excludes = true;
    let mut follow_links = false;
    let mut threads = None;
    let mut output = None;
    let mut help = false;
    let mut positional_only = false;

    let mut args = env::args_os().skip(1);
    while let Some(argument) = args.next() {
        if positional_only {
            roots.push(PathBuf::from(argument));
            continue;
        }
        let text = argument.to_string_lossy();
        match text.as_ref() {
            "-h" | "--help" => help = true,
            "--no-default-excludes" => use_default_excludes = false,
            "--follow-links" => follow_links = true,
            "--" => positional_only = true,
            "-e" | "--exclude" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--exclude requires a PATTERN".to_string())?;
                extra_excludes.push(value.to_string_lossy().into_owned());
            }
            "-o" | "--output" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--output requires a FILE".to_string())?;
                output = Some(PathBuf::from(value));
            }
            "--threads" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--threads requires a number".to_string())?;
                threads = Some(parse_threads(&value)?);
            }
            _ => {
                if let Some(value) = text.strip_prefix("--exclude=") {
                    extra_excludes.push(value.to_string());
                } else if let Some(value) = text.strip_prefix("--threads=") {
                    threads = Some(parse_threads(&OsString::from(value))?);
                } else if let Some(value) = text.strip_prefix("--output=") {
                    output = Some(PathBuf::from(value));
                } else if text.starts_with('-') && text != "-" {
                    return Err(format!("unknown option: {text}"));
                } else {
                    roots.push(PathBuf::from(argument));
                }
            }
        }
    }

    if roots.is_empty() && !help {
        roots.push(default_root());
    }

    Ok(Cli {
        roots,
        extra_excludes,
        use_default_excludes,
        follow_links,
        threads,
        output,
        help,
    })
}

fn parse_threads(value: &OsString) -> Result<usize, String> {
    let parsed = value
        .to_string_lossy()
        .parse::<usize>()
        .map_err(|_| format!("invalid thread count: {}", value.to_string_lossy()))?;
    if parsed == 0 {
        return Err("thread count must be at least 1".to_string());
    }
    Ok(parsed)
}

fn default_root() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(|| env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."))
}
