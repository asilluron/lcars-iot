//! `voice-daemon` listens for "Computer" and streams the utterance.

use std::path::PathBuf;
use std::process::ExitCode;

use voice_daemon::{Args, Mode};

fn main() -> ExitCode {
    let args = match parse(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(error) => {
            eprintln!("{error}");
            eprintln!(
                "usage: voice-daemon [--config FILE] [--calibrate WAV | --record WAV | review]"
            );
            return ExitCode::from(2);
        }
    };
    let filter = std::env::var("LOG_LEVEL").unwrap_or_else(|_| "info".into());
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(filter))
        .init();
    if let Err(error) = voice_daemon::run(args) {
        tracing::error!(%error, "voice daemon stopped");
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}

fn parse(args: impl IntoIterator<Item = String>) -> Result<Args, String> {
    let mut config = None;
    let mut mode = Mode::Listen;
    let mut tokens = args.into_iter();
    while let Some(token) = tokens.next() {
        match token.as_str() {
            "--config" => {
                config = Some(PathBuf::from(tokens.next().ok_or("--config needs a path")?));
            }
            "--calibrate" => {
                mode = Mode::Calibrate(PathBuf::from(
                    tokens.next().ok_or("--calibrate needs a wav")?,
                ));
            }
            "--record" => {
                mode = Mode::Record(PathBuf::from(tokens.next().ok_or("--record needs a wav")?));
            }
            "review" => mode = Mode::Review,
            "--help" | "-h" => return Err("help".into()),
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(Args { config, mode })
}
