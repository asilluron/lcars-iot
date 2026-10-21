//! Button cues from `assets/sounds`.
//!
//! Clips are decoded once at startup and mixed into an output stream that stays
//! open. A press only enqueues samples, so it does not pay for a new `afplay` or
//! `aplay` process. The stream asks for a 5 ms period. On the Pi, cpal talks to
//! ALSA and prefers the panel's USB audio device, then the connected HDMI port.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::OnceLock;

use cpal::traits::{DeviceTrait, HostTrait};
use cpal::{BufferSize, SupportedBufferSize};
use rodio::buffer::SamplesBuffer;
use rodio::mixer::Mixer;
use rodio::{OutputStream, OutputStreamBuilder};

const CUE_FILES: &[&str] = &[
    "button-push.wav",
    "button-push-alt.wav",
    "button-quick.wav",
    "button-alt-2.wav",
    "button-alt-3.wav",
    "button-confirm.wav",
    "button-double-click.wav",
    "long-press-button.wav",
    "scanning-button.wav",
    "setting-changed.wav",
    "chirp.wav",
    "affirmative.wav",
    "access-denied.wav",
];

#[must_use]
pub fn playback_volume(enabled: bool, volume: u8, quiet: bool) -> u8 {
    if !enabled {
        return 0;
    }
    if quiet { volume / 2 } else { volume }
}

/// Open the output stream before the first press so device setup is not part of the tap.
pub fn warm(enabled: bool) {
    if enabled {
        let _ = engine();
    }
}

pub fn cue(name: &str, volume: u8) {
    if volume == 0 {
        return;
    }
    let Some(file) = cue_file(name) else {
        tracing::debug!(sound = name, "unknown cue");
        return;
    };
    let Some(engine) = engine() else {
        return;
    };
    let Some(clip) = engine.clips.get(file) else {
        tracing::debug!(file, "cue was not loaded");
        return;
    };
    engine.mixer.add(SamplesBuffer::new(
        clip.channels,
        clip.rate,
        scale_samples(&clip.samples, volume),
    ));
}

fn cue_file(name: &str) -> Option<&'static str> {
    Some(match name {
        "push" | "tap" => "button-push.wav",
        "push-alt" => "button-push-alt.wav",
        "quick" => "button-quick.wav",
        "alt-2" => "button-alt-2.wav",
        "alt-3" => "button-alt-3.wav",
        "confirm" => "button-confirm.wav",
        "double" => "button-double-click.wav",
        "long" => "long-press-button.wav",
        "scan" => "scanning-button.wav",
        "setting" => "setting-changed.wav",
        "chirp" => "chirp.wav",
        "affirmative" => "affirmative.wav",
        "denied" | "error" => "access-denied.wav",
        _ => return None,
    })
}

fn scale_samples(samples: &[f32], volume: u8) -> Vec<f32> {
    let gain = f32::from(volume) / 100.0;
    if gain >= 1.0 {
        return samples.to_vec();
    }
    samples.iter().map(|sample| sample * gain).collect()
}

struct Clip {
    channels: u16,
    rate: u32,
    samples: Vec<f32>,
}

struct Engine {
    mixer: Mixer,
    clips: HashMap<&'static str, Clip>,
}

fn engine() -> Option<&'static Engine> {
    static ENGINE: OnceLock<Option<Engine>> = OnceLock::new();
    ENGINE.get_or_init(Engine::start).as_ref()
}

impl Engine {
    fn start() -> Option<Self> {
        let host = cpal::default_host();
        let chosen = pick_device(&host);
        let sink = match chosen.and_then(|device| open_sink(device).ok()) {
            Some(sink) => sink,
            None => {
                let device = host.default_output_device()?;
                match open_sink(device) {
                    Ok(sink) => sink,
                    Err(error) => {
                        tracing::warn!(%error, "audio output unavailable");
                        return None;
                    }
                }
            }
        };
        tracing::info!(config = ?sink.config(), "audio output ready");
        let mixer = sink.mixer().clone();
        let clips = load_clips();
        // CoreAudio's stream is not Sync, so it cannot sit in the static. Leaving
        // it undropped keeps the output callback running for the life of the process.
        std::mem::forget(sink);
        Some(Self { mixer, clips })
    }
}

fn open_sink(device: cpal::Device) -> Result<OutputStream, String> {
    let label = device_name(&device);
    let frames = output_period(&device);
    let config = device
        .default_output_config()
        .map_err(|error| error.to_string())?;
    let opened = OutputStreamBuilder::default()
        .with_device(device.clone())
        .with_supported_config(&config)
        .with_buffer_size(BufferSize::Fixed(frames))
        .with_error_callback(|error| tracing::warn!(%error, "audio output"))
        .open_stream();
    let mut sink = match opened {
        Ok(sink) => sink,
        Err(error) => {
            tracing::warn!(%error, frames, device = %label, "short audio period was rejected");
            OutputStreamBuilder::from_device(device)
                .map_err(|error| error.to_string())?
                .with_error_callback(|error| tracing::warn!(%error, "audio output"))
                .open_stream()
                .map_err(|error| error.to_string())?
        }
    };
    sink.log_on_drop(false);
    tracing::info!(device = %label, frames, "audio stream open");
    Ok(sink)
}

fn pick_device(host: &cpal::Host) -> Option<cpal::Device> {
    let devices: Vec<cpal::Device> = host.output_devices().ok()?.collect();
    let names: Vec<String> = devices.iter().map(device_name).collect();
    let labels: Vec<&str> = names.iter().map(String::as_str).collect();
    let cards = std::fs::read_to_string("/proc/asound/cards")
        .map(|text| parse_cards(&text))
        .unwrap_or_default();
    let explicit = std::env::var("AUDIO_DEVICE").ok();
    let spec = explicit
        .as_deref()
        .map(str::trim)
        .filter(|spec| !spec.is_empty())
        .map_or_else(
            || choose_device(&cards, &connected_hdmi_ports(), None),
            ToOwned::to_owned,
        );
    let index = resolve_device_index(&spec, &cards, &labels)?;
    devices.into_iter().nth(index)
}

fn device_name(device: &cpal::Device) -> String {
    device.name().unwrap_or_else(|_| "audio".to_string())
}

fn output_period(device: &cpal::Device) -> u32 {
    let Ok(default) = device.default_output_config() else {
        return 256;
    };
    let rate = default.sample_rate().0;
    let range = device
        .supported_output_configs()
        .ok()
        .and_then(|mut configs| {
            configs.find(|config| {
                config.channels() == default.channels()
                    && config.sample_format() == default.sample_format()
                    && config.min_sample_rate() <= default.sample_rate()
                    && config.max_sample_rate() >= default.sample_rate()
            })
        });
    match range
        .as_ref()
        .map(cpal::SupportedStreamConfigRange::buffer_size)
    {
        Some(SupportedBufferSize::Range { min, max }) => period_frames(rate, *min, *max),
        _ => period_frames(rate, 32, rate.max(32)),
    }
}

fn period_frames(rate: u32, min: u32, max: u32) -> u32 {
    let want = (rate / 200).max(32);
    if min > max {
        return want.max(1);
    }
    want.clamp(min, max).max(1)
}

fn load_clips() -> HashMap<&'static str, Clip> {
    let mut clips = HashMap::new();
    for file in CUE_FILES {
        let path = assets_dir().join("sounds").join(file);
        match std::fs::read(&path) {
            Ok(bytes) => match decode_wav(&bytes) {
                Some(clip) => {
                    clips.insert(*file, clip);
                }
                None => tracing::warn!(path = %path.display(), "cue is not a pcm wav"),
            },
            Err(error) => tracing::warn!(path = %path.display(), %error, "cue file missing"),
        }
    }
    clips
}

fn assets_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("LCARS_ASSETS") {
        return PathBuf::from(dir);
    }
    let packaged = PathBuf::from("/opt/lcars/assets");
    if packaged.join("sounds").is_dir() {
        return packaged;
    }
    let local = PathBuf::from("assets");
    if local.join("sounds").is_dir() {
        return local;
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../assets")
}

fn decode_wav(bytes: &[u8]) -> Option<Clip> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return None;
    }
    let mut offset = 12usize;
    let mut channels = None;
    let mut rate = None;
    let mut bits = None;
    let mut data = None;
    while offset + 8 <= bytes.len() {
        let id = &bytes[offset..offset + 4];
        let size = u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().ok()?) as usize;
        let start = offset + 8;
        let end = start.checked_add(size)?;
        if end > bytes.len() {
            return None;
        }
        if id == b"fmt " && size >= 16 {
            let format = u16::from_le_bytes([bytes[start], bytes[start + 1]]);
            if format == 1 {
                channels = Some(u16::from_le_bytes([bytes[start + 2], bytes[start + 3]]));
                rate = Some(u32::from_le_bytes(
                    bytes[start + 4..start + 8].try_into().ok()?,
                ));
                bits = Some(u16::from_le_bytes([bytes[start + 14], bytes[start + 15]]));
            }
        } else if id == b"data" {
            data = Some(start..end);
        }
        offset = end + (size % 2);
    }
    let (channels, rate, bits, range) = (channels?, rate?, bits?, data?);
    if channels == 0 || rate == 0 {
        return None;
    }
    let samples = match bits {
        8 => bytes[range]
            .iter()
            .map(|sample| (f32::from(*sample) - 128.0) / 128.0)
            .collect(),
        16 => bytes[range]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|chunk| f32::from(i16::from_le_bytes(*chunk)) / 32_768.0)
            .collect(),
        _ => return None,
    };
    Some(Clip {
        channels,
        rate,
        samples,
    })
}

fn connected_hdmi_ports() -> Vec<u8> {
    let Ok(entries) = std::fs::read_dir("/sys/class/drm") else {
        return Vec::new();
    };
    let mut ports = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let port = if name.contains("HDMI-A-1") {
            1
        } else if name.contains("HDMI-A-2") {
            2
        } else {
            continue;
        };
        let status = std::fs::read_to_string(entry.path().join("status")).unwrap_or_default();
        if status.trim() == "connected" {
            ports.push(port);
        }
    }
    ports
}

#[derive(Debug, PartialEq, Eq)]
struct SoundCard {
    index: u32,
    id: String,
    kind: String,
}

fn parse_cards(text: &str) -> Vec<SoundCard> {
    let mut cards = Vec::new();
    for line in text.lines() {
        let Some(open) = line.find('[') else {
            continue;
        };
        let Some(close) = line.find(']') else {
            continue;
        };
        let Ok(index) = line[..open].trim().parse::<u32>() else {
            continue;
        };
        let id = line[open + 1..close].trim().to_string();
        let kind = line[close + 1..]
            .trim()
            .trim_start_matches(':')
            .trim()
            .to_string();
        cards.push(SoundCard { index, id, kind });
    }
    cards
}

fn choose_device(cards: &[SoundCard], connected_hdmi: &[u8], explicit: Option<&str>) -> String {
    if let Some(device) = explicit.map(str::trim).filter(|device| !device.is_empty()) {
        return device.to_string();
    }
    if let Some(card) = cards.iter().find(|card| is_usb(card)) {
        return format!("plughw:{},0", card.index);
    }
    let hdmi: Vec<&SoundCard> = cards
        .iter()
        .filter(|card| hdmi_port(card).is_some())
        .collect();
    if let Some(card) = hdmi
        .iter()
        .find(|card| hdmi_port(card).is_some_and(|port| connected_hdmi.contains(&port)))
    {
        return format!("plughw:{},0", card.index);
    }
    if let Some(card) = hdmi.first() {
        return format!("plughw:{},0", card.index);
    }
    "default".to_string()
}

fn resolve_device_index(spec: &str, cards: &[SoundCard], names: &[&str]) -> Option<usize> {
    let spec = spec.trim();
    if spec.is_empty() || spec.eq_ignore_ascii_case("default") {
        return None;
    }
    if let Some(index) = names
        .iter()
        .position(|name| name.eq_ignore_ascii_case(spec))
    {
        return Some(index);
    }
    if let Some(card) =
        alsa_card_index(spec).and_then(|index| cards.iter().find(|card| card.index == index))
    {
        return device_for_card(names, &card.id);
    }
    names.iter().position(|name| {
        name.to_ascii_lowercase()
            .contains(&spec.to_ascii_lowercase())
    })
}

fn device_for_card(names: &[&str], card_id: &str) -> Option<usize> {
    let id = card_id.to_ascii_lowercase();
    let hits: Vec<usize> = names
        .iter()
        .enumerate()
        .filter_map(|(index, name)| name.to_ascii_lowercase().contains(&id).then_some(index))
        .collect();
    hits.iter()
        .copied()
        .find(|index| {
            let name = names[*index].to_ascii_lowercase();
            name.contains("plughw") || name.contains("hdmi:")
        })
        .or_else(|| hits.first().copied())
}

fn alsa_card_index(spec: &str) -> Option<u32> {
    let rest = spec
        .strip_prefix("plughw:")
        .or_else(|| spec.strip_prefix("hw:"))?;
    rest.split([',', ':']).next()?.trim().parse().ok()
}

fn is_usb(card: &SoundCard) -> bool {
    card_blob(card).contains("usb")
}

fn hdmi_port(card: &SoundCard) -> Option<u8> {
    let blob = card_blob(card);
    if blob.contains("hdmi0") || blob.contains("hdmi-0") {
        Some(1)
    } else if blob.contains("hdmi1") || blob.contains("hdmi-1") {
        Some(2)
    } else if blob.contains("hdmi") {
        Some(1)
    } else {
        None
    }
}

fn card_blob(card: &SoundCard) -> String {
    format!("{} {}", card.id, card.kind).to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::{
        choose_device, cue_file, decode_wav, parse_cards, period_frames, playback_volume,
        resolve_device_index, scale_samples,
    };

    #[test]
    fn quiet_hours_halve_the_volume() {
        assert_eq!(playback_volume(true, 60, false), 60);
        assert_eq!(playback_volume(true, 60, true), 30);
        assert_eq!(playback_volume(false, 60, false), 0);
    }

    #[test]
    fn button_names_map_to_button_files() {
        assert_eq!(cue_file("push"), Some("button-push.wav"));
        assert_eq!(cue_file("tap"), Some("button-push.wav"));
        assert_eq!(cue_file("setting"), Some("setting-changed.wav"));
        assert_eq!(cue_file("scan"), Some("scanning-button.wav"));
        assert_eq!(cue_file("error"), Some("access-denied.wav"));
        assert_eq!(cue_file("nope"), None);
    }

    #[test]
    fn usb_audio_wins_over_hdmi_and_a_connected_port_wins_otherwise() {
        let cards = sample_cards();
        assert_eq!(cards.len(), 3);
        assert_eq!(choose_device(&cards, &[1], None), "plughw:2,0");
        assert_eq!(choose_device(&cards[..2], &[2], None), "plughw:1,0");
        assert_eq!(choose_device(&cards[..2], &[], None), "plughw:0,0");
        assert_eq!(
            choose_device(&cards, &[1], Some("plughw:9,0")),
            "plughw:9,0"
        );
        assert_eq!(choose_device(&[], &[], None), "default");
    }

    #[test]
    fn alsa_card_maps_onto_the_coreaudio_or_alsa_device_name() {
        let cards = sample_cards();
        let names = [
            "hdmi:CARD=vc4hdmi0,DEV=0",
            "hdmi:CARD=vc4hdmi1,DEV=0",
            "hw:CARD=UAC2,DEV=0",
            "plughw:CARD=UAC2,DEV=0",
        ];
        assert_eq!(resolve_device_index("default", &cards, &names), None);
        assert_eq!(resolve_device_index("plughw:2,0", &cards, &names), Some(3));
        assert_eq!(resolve_device_index("plughw:1,0", &cards, &names), Some(1));
        assert_eq!(
            resolve_device_index("MacBook Pro Speakers", &cards, &["MacBook Pro Speakers"]),
            Some(0)
        );
    }

    #[test]
    fn period_targets_five_milliseconds() {
        assert_eq!(period_frames(48_000, 32, 4_096), 240);
        assert_eq!(period_frames(48_000, 256, 4_096), 256);
        assert_eq!(period_frames(44_100, 15, 4_096), 220);
    }

    #[test]
    fn volume_scales_pcm_samples() {
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&36u32.to_le_bytes());
        wav.extend_from_slice(b"WAVE");
        wav.extend_from_slice(b"fmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&22050u32.to_le_bytes());
        wav.extend_from_slice(&44100u32.to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&2u32.to_le_bytes());
        wav.extend_from_slice(&1000i16.to_le_bytes());
        let clip = decode_wav(&wav).expect("pcm");
        assert_eq!(clip.channels, 1);
        assert_eq!(clip.rate, 22_050);
        let scaled = scale_samples(&clip.samples, 50);
        let expected = (1000.0 / 32_768.0) * 0.5;
        assert!((scaled[0] - expected).abs() < 0.000_1);
    }

    fn sample_cards() -> Vec<super::SoundCard> {
        parse_cards(
            " 0 [vc4hdmi0       ]: vc4-hdmi - vc4-hdmi-0\n                      vc4-hdmi-0\n 1 [vc4hdmi1       ]: vc4-hdmi - vc4-hdmi-1\n 2 [UAC2          ]: USB-Audio - USB Audio Device\n",
        )
    }
}
