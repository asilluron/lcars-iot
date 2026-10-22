//! File modes: score a clip, record the mic chain, or tag a wake log.

use std::io::{self, Write};
use std::path::Path;
use std::process::Command;

use voice_proto::PREROLL_SAMPLES;

use crate::config::VoiceConfig;
use crate::trigger::Trigger;
use crate::verifier::Verifier;
use crate::wake_log::write_tag;
use crate::wav;

pub fn calibrate(config: &VoiceConfig, path: &Path) -> Result<(), String> {
    let samples = wav::read_wav(path).map_err(|error| error.to_string())?;
    let mut trigger = Trigger::open(&config.trigger.model, 0.0, config.trigger.sliding_window)?;
    let verifier = Verifier::open(
        &config.verifier.melspectrogram,
        &config.verifier.embedding,
        &config.verifier.classifier,
    );
    let mut block = Vec::with_capacity(160);
    let mut frame = 0_u64;
    let mut seen = 0_usize;
    for sample in &samples {
        block.push(*sample);
        seen += 1;
        if block.len() < 160 {
            continue;
        }
        frame += 1;
        if let Some(score) = trigger.push(&block)? {
            println!("frame {frame} trigger {score:.3}");
            if score > config.trigger.threshold {
                print_verifier(&verifier, &samples, seen);
            }
        }
        block.clear();
    }
    println!("clip verifier");
    print_verifier(&verifier, &samples, samples.len());
    Ok(())
}

fn print_verifier(verifier: &Verifier, samples: &[i16], end: usize) {
    let start = end.saturating_sub(PREROLL_SAMPLES);
    match verifier.score(&samples[start..end]) {
        Some(score) => println!("verifier {score:.3}"),
        None => println!("verifier absent"),
    }
}

pub fn record(config: &VoiceConfig, path: &Path) -> Result<(), String> {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (config, path);
        Err("record needs the linux capture device".into())
    }
    #[cfg(target_os = "linux")]
    {
        let (producer, mut consumer) = rtrb::RingBuffer::<i16>::new(32_000);
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();
        crate::capture::spawn(config.capture.device.clone(), producer, cmd_tx);
        let stop = install_interrupt()?;
        println!("recording {} — interrupt to stop", path.display());
        let mut samples = Vec::new();
        loop {
            let _ = cmd_rx.try_recv();
            match consumer.pop() {
                Ok(sample) => samples.push(sample),
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(20)),
            }
            if samples.len().is_multiple_of(16_000) && !samples.is_empty() {
                print!("\r{} s", samples.len() / 16_000);
                let _ = io::stdout().flush();
            }
            if stop.try_recv().is_ok() && samples.len() > 160 {
                break;
            }
        }
        println!();
        wav::write_wav(path, &samples).map_err(|error| error.to_string())?;
        println!("wrote {}", path.display());
        Ok(())
    }
}

pub fn review(config: &VoiceConfig) -> Result<(), String> {
    let dir = &config.log.dir;
    let mut clips = Vec::new();
    let entries = std::fs::read_dir(dir).map_err(|error| error.to_string())?;
    for entry in entries {
        let entry = entry.map_err(|error| error.to_string())?;
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "wav") {
            clips.push(path);
        }
    }
    clips.sort();
    if clips.is_empty() {
        println!("no clips in {}", dir.display());
        return Ok(());
    }
    for wav_path in clips {
        println!("{}", wav_path.display());
        play(&wav_path);
        print!("positive? [y/n/q] ");
        let _ = io::stdout().flush();
        let mut line = String::new();
        io::stdin()
            .read_line(&mut line)
            .map_err(|error| error.to_string())?;
        let json = wav_path.with_extension("json");
        match line.trim() {
            "y" | "Y" => write_tag(&json, true).map_err(|error| error.to_string())?,
            "n" | "N" => write_tag(&json, false).map_err(|error| error.to_string())?,
            "q" | "Q" => break,
            _ => {}
        }
    }
    Ok(())
}

fn play(path: &Path) {
    let program = if cfg!(target_os = "macos") {
        "afplay"
    } else {
        "aplay"
    };
    let _ = Command::new(program).arg(path).status();
}

#[cfg(target_os = "linux")]
fn install_interrupt() -> Result<std::sync::mpsc::Receiver<()>, String> {
    let (tx, rx) = std::sync::mpsc::channel();
    let mut signals = signal_hook::iterator::Signals::new([
        signal_hook::consts::SIGINT,
        signal_hook::consts::SIGTERM,
    ])
    .map_err(|error| error.to_string())?;
    std::thread::spawn(move || {
        if signals.forever().next().is_some() {
            let _ = tx.send(());
        }
    });
    Ok(rx)
}
