use std::env;

mod clip;
mod index;
mod r2;

const USAGE: &str = "usage: rolodex <index|clip> [OPTIONS]";

fn next_value(values: &mut impl Iterator<Item = String>, flag: &str) -> Result<String, String> {
    values
        .next()
        .ok_or_else(|| format!("{flag} requires a value"))
}

fn parse_number<T: std::str::FromStr>(
    values: &mut impl Iterator<Item = String>,
    flag: &str,
) -> Result<T, String> {
    next_value(values, flag)?
        .parse()
        .map_err(|_| format!("invalid value for {flag}"))
}

fn run() -> Result<(), String> {
    let mut values = env::args().skip(1);
    match values.next().as_deref() {
        Some("index") => index::run_cli(values),
        Some("clip") => clip::run_cli(values),
        Some("-h" | "--help") => Err(USAGE.into()),
        Some(command) => Err(format!("unknown command {command}\n{USAGE}")),
        None => Err(USAGE.into()),
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("rolodex: {error}");
        std::process::exit(1);
    }
}
