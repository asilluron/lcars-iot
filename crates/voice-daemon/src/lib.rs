//! On-device "Computer" wake word. The daemon detects locally and streams PCM
//! on the same NATS hub the panel already uses.

mod capture;
mod cmd;
mod config;
mod device;
mod frame;
mod gate;
mod hash;
mod link;
mod listen;
mod modes;
mod net;
mod outbound;
mod session;
mod training;
mod trigger;
mod vad;
mod verifier;
mod wake_log;
mod wav;

use std::path::PathBuf;

use config::resolve_config_path;

pub use config::VoiceConfig;

#[derive(Debug)]
pub enum Mode {
    Listen,
    Calibrate(PathBuf),
    Record(PathBuf),
    Review,
}

pub struct Args {
    pub config: Option<PathBuf>,
    pub mode: Mode,
}

pub fn run(args: Args) -> Result<(), String> {
    let path = resolve_config_path(args.config.as_deref())
        .ok_or_else(|| "set --config or VOICE_CONFIG".to_string())?;
    let config = config::load(&path).map_err(|error| error.to_string())?;
    match args.mode {
        Mode::Listen => {
            let hub = link::Hub::from_env()?;
            listen::listen(config, hub, &path)
        }
        Mode::Calibrate(wav) => modes::calibrate(&config, &wav),
        Mode::Record(wav) => modes::record(&config, &wav),
        Mode::Review => modes::review(&config),
    }
}
