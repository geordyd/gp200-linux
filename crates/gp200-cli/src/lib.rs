//! Supported headless adapter. Offline commands do not construct MIDI services.
mod device;
mod files;
mod offline;
mod support;
use gp200_domain::{ErrorKind, ServiceError};
use serde_json::{Value, json};
use std::{collections::BTreeMap, ffi::OsString, io::Write, path::PathBuf, process::ExitCode};

pub const HELP: &str = include_str!("help.txt");
#[derive(Debug)]
pub(crate) struct Options {
    pub command: String,
    pub action: String,
    values: BTreeMap<String, OsString>,
    pub inclusions: Vec<(String, PathBuf)>,
}
impl Options {
    pub fn has(&self, key: &str) -> bool {
        self.values.contains_key(key)
    }
    pub fn text(&self, key: &str) -> Result<Option<&str>, ServiceError> {
        self.values
            .get(key)
            .map(|value| {
                value
                    .to_str()
                    .ok_or_else(|| ServiceError::invalid(format!("{key} requires UTF-8 text")))
            })
            .transpose()
    }
    pub fn required_text(&self, key: &str) -> Result<&str, ServiceError> {
        self.text(key)?
            .ok_or_else(|| ServiceError::invalid(format!("Missing {key}")))
    }
    pub fn path(&self, key: &str) -> Result<PathBuf, ServiceError> {
        self.values
            .get(key)
            .map(PathBuf::from)
            .ok_or_else(|| ServiceError::invalid(format!("Missing {key}")))
    }
    pub fn optional_path(&self, key: &str) -> Option<PathBuf> {
        self.values.get(key).map(PathBuf::from)
    }
    pub fn number(&self, key: &str, default: u64, min: u64, max: u64) -> Result<u64, ServiceError> {
        let value = match self.text(key)? {
            Some(text) => text
                .parse::<u64>()
                .map_err(|_| ServiceError::invalid(format!("{key} requires an integer")))?,
            None => default,
        };
        if !(min..=max).contains(&value) {
            return Err(ServiceError::invalid(format!("{key} must be {min}..{max}")));
        }
        Ok(value)
    }
}
fn parse(arguments: Vec<OsString>) -> Result<Options, ServiceError> {
    let mut args = arguments.into_iter();
    let command = args
        .next()
        .and_then(|s| s.into_string().ok())
        .ok_or_else(|| ServiceError::invalid("A command is required; use --help"))?;
    let action = args
        .next()
        .and_then(|s| s.into_string().ok())
        .ok_or_else(|| ServiceError::invalid("An action is required; use --help"))?;
    let (allowed, flags): (&[&str], &[&str]) = match (command.as_str(), action.as_str()) {
        ("inspect", "nam") => (&["--input", "--nam-size"], &[]),
        ("inspect", "prst" | "wav" | "clo" | "firmware" | "backup" | "bundle") => {
            (&["--input"], &[])
        }
        ("library", "list") => (&["--store", "--text"], &[]),
        ("library", "backup-list") => (&["--store"], &[]),
        ("library", "import" | "bundle-import" | "backup-import") => (&["--store", "--input"], &[]),
        ("library", "export") => (
            &["--store", "--version", "--output", "--preset-only"],
            &["--preset-only"],
        ),
        ("library", "bundle-export") => (
            &["--store", "--version", "--output", "--include-assets"],
            &["--include-assets"],
        ),
        ("library", "backup-export") => (&["--store", "--backup-id", "--output"], &[]),
        ("asset", "prepare") => (
            &[
                "--kind",
                "--input",
                "--store",
                "--output",
                "--sample-rate",
                "--channel",
                "--normalize-peak",
                "--truncate-frames",
                "--input-wav",
                "--input-channel",
                "--output-format",
                "--activation",
                "--nam-size",
                "--resample-input",
                "--max-operations",
                "--calibration-timeout-seconds",
            ],
            &["--resample-input"],
        ),
        ("asset", "ir-pack") => (&["--input", "--output", "--truncate"], &["--truncate"]),
        ("trace", "inspect" | "replay") => (&["--input"], &[]),
        ("support", "export") => (
            &[
                "--output",
                "--session-summary",
                "--include-preset",
                "--include-audio",
                "--include-midi",
            ],
            &[],
        ),
        ("device", "ports") => (&[], &[]),
        ("device", "identity" | "current" | "globals" | "doctor") => (
            &[
                "--unique",
                "--input-port",
                "--output-port",
                "--timeout-seconds",
                "--output",
            ],
            &["--unique"],
        ),
        ("device", "stored" | "ir-metadata") => (
            &[
                "--unique",
                "--input-port",
                "--output-port",
                "--timeout-seconds",
                "--output",
                "--slot",
            ],
            &["--unique"],
        ),
        ("device", "bank") => (
            &[
                "--unique",
                "--input-port",
                "--output-port",
                "--timeout-seconds",
                "--store",
            ],
            &["--unique"],
        ),
        _ => return Err(ServiceError::invalid("Unknown command/action; use --help")),
    };
    let mut result = Options {
        command,
        action,
        values: BTreeMap::new(),
        inclusions: Vec::new(),
    };
    while let Some(argument) = args.next() {
        let key = argument
            .into_string()
            .map_err(|_| ServiceError::invalid("Option names require UTF-8"))?;
        if !allowed.contains(&key.as_str()) {
            return Err(ServiceError::invalid(format!(
                "Unknown option {key} for this command"
            )));
        }
        let value = if flags.contains(&key.as_str()) {
            OsString::from("true")
        } else {
            let value = args
                .next()
                .ok_or_else(|| ServiceError::invalid(format!("Missing value for {key}")))?;
            if value.is_empty() || value.to_string_lossy().starts_with("--") {
                return Err(ServiceError::invalid(format!("Missing value for {key}")));
            }
            value
        };
        if key.starts_with("--include-") && result.command == "support" {
            if result.inclusions.len() >= support::MAX_ATTACHMENTS {
                return Err(ServiceError::invalid("Too many support attachments"));
            }
            result.inclusions.push((
                key.trim_start_matches("--include-").to_owned(),
                PathBuf::from(value),
            ));
        } else if result.values.insert(key.clone(), value).is_some() {
            return Err(ServiceError::invalid(format!("Repeated option {key}")));
        }
    }
    Ok(result)
}
pub fn exit_code(kind: &ErrorKind) -> u8 {
    match kind {
        ErrorKind::InvalidData => 2,
        ErrorKind::Unsupported | ErrorKind::Unverified => 3,
        ErrorKind::Busy | ErrorKind::Conflict => 4,
        ErrorKind::Disconnected | ErrorKind::Timeout => 5,
        ErrorKind::Io | ErrorKind::Protocol => 6,
        ErrorKind::Cancelled => 7,
        ErrorKind::Indeterminate => 8,
    }
}
pub fn main_entry(arguments: Vec<OsString>) -> ExitCode {
    if arguments.is_empty() || arguments.iter().any(|arg| arg == "--help" || arg == "-h") {
        print!("{HELP}");
        return ExitCode::SUCCESS;
    }
    if arguments.len() == 1 && arguments[0] == "--version" {
        println!("GP200 Linux CLI {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    if arguments.len() == 1 && arguments[0] == "--calibration-helper-info" {
        println!(
            "{}",
            json!({"available": gp200_assets::calibration_helper_available(), "sha256": gp200_assets::calibration_helper_sha256(), "mode": "offline; no MIDI or network"})
        );
        return ExitCode::SUCCESS;
    }
    let result = parse(arguments).and_then(|options| {
        let command = format!("{} {}", options.command, options.action);
        let data = run(&options)?;
        Ok(json!({"schema": 1, "ok": true, "command": command, "data": data}))
    });
    match result {
        Ok(value) => {
            let written = serde_json::to_writer(std::io::stdout().lock(), &value).and_then(|()| {
                std::io::stdout()
                    .write_all(b"\n")
                    .map_err(serde_json::Error::io)
            });
            if written.is_ok() {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(6)
            }
        }
        Err(error) => {
            eprintln!(
                "{}",
                json!({"schema": 1, "ok": false, "error": error, "exit_code": exit_code(&error.kind)})
            );
            ExitCode::from(exit_code(&error.kind))
        }
    }
}
fn run(options: &Options) -> Result<Value, ServiceError> {
    match options.command.as_str() {
        "device" => device::run(options),
        "support" => support::run(options),
        _ => offline::run(options),
    }
}
