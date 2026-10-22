//! Detection loop. Capture never waits on NATS, and a full publish queue drops audio.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::mpsc::Receiver;
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use rtrb::Consumer;
use tokio::sync::mpsc::Sender;
use voice_proto::v1::{Phase, SessionStart, VoiceState};
use voice_proto::{FRAME_SAMPLES, PREROLL_SAMPLES};

use crate::cmd::Cmd;
use crate::config::VoiceConfig;
use crate::frame::{pack_frame, preroll_frames};
use crate::gate::silence_before_onset;
use crate::hash::file_hash;
use crate::link::Hub;
use crate::outbound::{AudioFrame, Outbound, QUEUE_DEPTH, offer};
use crate::session::{ActiveSession, Limits, Stop};
use crate::trigger::Trigger;
use crate::vad::{self, Vad};
use crate::verifier::Verifier;
use crate::wake_log::{WakeMeta, enforce_budget, write_wake};

const HISTORY: usize = 32_000;
const SPEECH_MEMORY_MS: u64 = 2_000;

struct OpenSession {
    id: String,
    active: ActiveSession,
    streaming: bool,
    seq: u32,
}

pub fn listen(mut config: VoiceConfig, hub: Hub, config_path: &Path) -> Result<(), String> {
    apply_env(&mut config)?;
    let trigger_hash = file_hash(&config.trigger.model);
    let verifier_hash = file_hash(&config.verifier.classifier);
    tracing::info!(
        trigger = %trigger_hash,
        verifier = %verifier_hash,
        stream_all = config.training.stream_all,
        "models"
    );

    let trigger = Trigger::open(
        &config.trigger.model,
        config.trigger.threshold,
        config.trigger.sliding_window,
    )?;
    let vad = Vad::open(&config.vad.model, config.vad.threshold)?;
    let verifier = Verifier::open(
        &config.verifier.melspectrogram,
        &config.verifier.embedding,
        &config.verifier.classifier,
    );
    if !verifier.available() {
        tracing::warn!("verifier is closed; confirmed sessions will not start");
    }

    let (producer, consumer) = rtrb::RingBuffer::<i16>::new(HISTORY);
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();
    let (out_tx, out_rx) = tokio::sync::mpsc::channel(QUEUE_DEPTH);
    let (train_tx, train_rx) = tokio::sync::mpsc::channel(crate::training::QUEUE_DEPTH);

    let capture_cmd = cmd_tx.clone();
    let device = config.capture.device.clone();
    crate::capture::spawn(device, producer, capture_cmd);

    let reload_cmd = cmd_tx.clone();
    thread::spawn(move || watch_signals(reload_cmd));

    let net_cmd = cmd_tx;
    let screen_id = hub.screen_id.clone();
    thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("nats runtime");
        runtime.block_on(crate::net::run(hub, screen_id, out_rx, train_rx, net_cmd));
    });

    detect(
        config,
        config_path.to_path_buf(),
        consumer,
        cmd_rx,
        out_tx,
        train_tx,
        trigger,
        vad,
        verifier,
        trigger_hash,
        verifier_hash,
    )
}

#[allow(clippy::too_many_arguments)]
fn detect(
    mut config: VoiceConfig,
    config_path: std::path::PathBuf,
    mut consumer: Consumer<i16>,
    cmd_rx: Receiver<Cmd>,
    out_tx: Sender<Outbound>,
    train_tx: Sender<Vec<u8>>,
    mut trigger: Trigger,
    mut vad: Vad,
    verifier: Verifier,
    trigger_hash: String,
    verifier_hash: String,
) -> Result<(), String> {
    let screen = std::env::var("SCREEN_ID").unwrap_or_else(|_| "dev".into());
    let mut history = VecDeque::with_capacity(HISTORY);
    let mut speech: VecDeque<(u64, bool)> = VecDeque::new();
    let mut vad_pending = Vec::new();
    let mut trigger_pending = Vec::new();
    let mut live_pending = Vec::new();
    let mut session: Option<OpenSession> = None;
    let mut samples_seen: u64 = 0;
    let mut cooldown_until = 0_u64;
    let mut idle_pending = false;
    let mut mic_online = false;
    let mut endpoint = false;
    let mut cancel = false;
    let mut dropped = 0_u64;
    let mut training_dropped = 0_u64;
    let mut training = crate::training::Batch::default();

    loop {
        while let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                Cmd::Endpoint { session_id } => {
                    if session_matches(session.as_ref().map(|open| open.id.as_str()), &session_id) {
                        endpoint = true;
                    }
                }
                Cmd::Cancel { session_id } => {
                    if session_matches(session.as_ref().map(|open| open.id.as_str()), &session_id) {
                        cancel = true;
                    }
                }
                Cmd::Reload => {
                    if let Err(error) = reload(&config_path, &mut config, &mut trigger, &mut vad) {
                        tracing::warn!(%error, "config reload failed");
                    }
                }
                Cmd::MicOnline => {
                    mic_online = true;
                    publish_state(&out_tx, &screen, Phase::Idle, "");
                }
                Cmd::MicOffline => {
                    mic_online = false;
                    session = None;
                    idle_pending = false;
                    live_pending.clear();
                    training.clear();
                    publish_state(&out_tx, &screen, Phase::MicOffline, "");
                }
            }
        }

        let mut chunk = Vec::new();
        while chunk.len() < 1600 {
            match consumer.pop() {
                Ok(sample) => chunk.push(sample),
                Err(_) => break,
            }
        }
        if chunk.is_empty() {
            thread::sleep(std::time::Duration::from_millis(2));
            continue;
        }

        for sample in chunk {
            samples_seen += 1;
            if let Some(pcm) = training.push(sample, config.training.stream_all)
                && train_tx.try_send(pcm).is_err()
            {
                training_dropped += 1;
                if training_dropped == 1 || training_dropped.is_multiple_of(60) {
                    tracing::warn!(
                        training_dropped,
                        "dropped training audio; the hub is behind"
                    );
                }
            }
            let now_ms = samples_seen * 1000 / u64::from(voice_proto::SAMPLE_RATE);
            push_history(&mut history, sample);
            vad_pending.push(sample);
            if session.is_some() {
                live_pending.push(sample);
            } else if now_ms >= cooldown_until && mic_online {
                trigger_pending.push(sample);
            }

            let window = vad::window_samples();
            if vad_pending.len() >= window {
                let block: Vec<i16> = vad_pending.drain(..window).collect();
                let heard = vad.push(&block);
                speech.push_back((now_ms, heard));
                while speech
                    .front()
                    .is_some_and(|(time, _)| now_ms.saturating_sub(*time) > SPEECH_MEMORY_MS)
                {
                    speech.pop_front();
                }
                if let Some(open) = session.as_mut()
                    && heard
                {
                    open.active.last_speech_ms = now_ms;
                    if !open.streaming {
                        open.streaming = true;
                        publish_state(&out_tx, &screen, Phase::Streaming, &open.id);
                    }
                }
            }

            if session.is_none() && trigger_pending.len() >= 160 {
                let block: Vec<i16> = trigger_pending.drain(..160).collect();
                if let Some(score) = trigger.push(&block)? {
                    on_trigger(
                        &TriggerContext {
                            config: &config,
                            history: &history,
                            speech: &speech,
                            verifier: &verifier,
                            trigger_hash: &trigger_hash,
                            verifier_hash: &verifier_hash,
                            now_ms,
                            score,
                        },
                        &out_tx,
                        &screen,
                        &mut session,
                    );
                    if session.is_some() {
                        trigger_pending.clear();
                        let _ = trigger.reset();
                    }
                }
            }

            while session.is_some() && live_pending.len() >= FRAME_SAMPLES {
                let block: Vec<i16> = live_pending.drain(..FRAME_SAMPLES).collect();
                if let Some(open) = session.as_mut() {
                    let frame = AudioFrame {
                        session_id: open.id.clone(),
                        seq: open.seq,
                        pcm: pack_frame(&block),
                    };
                    open.seq += 1;
                    if !offer(&out_tx, Outbound::Frame(frame)) {
                        dropped += 1;
                        if dropped == 1 || dropped.is_multiple_of(50) {
                            tracing::warn!(dropped, "dropped pcm; the hub is behind");
                        }
                    }
                }
            }

            if let Some(open) = &session {
                let limits = Limits {
                    silence_ms: config.session.silence_end_ms,
                    hard_cap_ms: config.session.hard_cap_ms,
                };
                if let Some(reason) = open.active.stop(now_ms, limits, endpoint, cancel) {
                    tracing::info!(?reason, session = %open.id, "session ended");
                    let id = open.id.clone();
                    session = None;
                    endpoint = false;
                    cancel = false;
                    live_pending.clear();
                    cooldown_until = now_ms + config.session.cooldown_ms;
                    idle_pending = true;
                    publish_state(&out_tx, &screen, Phase::Processing, &id);
                    let _ = trigger.reset();
                }
            }

            if idle_pending && session.is_none() && now_ms >= cooldown_until && mic_online {
                idle_pending = false;
                publish_state(&out_tx, &screen, Phase::Idle, "");
            }
        }
    }
}

struct TriggerContext<'a> {
    config: &'a VoiceConfig,
    history: &'a VecDeque<i16>,
    speech: &'a VecDeque<(u64, bool)>,
    verifier: &'a Verifier,
    trigger_hash: &'a str,
    verifier_hash: &'a str,
    now_ms: u64,
    score: f32,
}

fn on_trigger(
    hit: &TriggerContext<'_>,
    out_tx: &Sender<Outbound>,
    screen: &str,
    session: &mut Option<OpenSession>,
) {
    let samples: Vec<i16> = hit.history.iter().copied().collect();
    let marks: Vec<(u64, bool)> = hit.speech.iter().copied().collect();
    let gate_open = silence_before_onset(
        &marks,
        hit.now_ms,
        hit.config.gate.onset_ms,
        hit.config.gate.silence_ms,
    );
    let clip = tail(&samples, PREROLL_SAMPLES);
    let verifier_score = hit.verifier.score(clip);
    let confirmed =
        gate_open && verifier_score.is_some_and(|value| value > hit.config.verifier.threshold);
    let meta = WakeMeta {
        confirmed,
        trigger_score: hit.score,
        verifier_score,
        gate_open,
        unix_ms: unix_ms(),
        tag: None,
    };
    match write_wake(&hit.config.log.dir, &samples, &meta) {
        Ok(path) => {
            if let Err(error) = enforce_budget(&hit.config.log.dir, hit.config.log.max_bytes) {
                tracing::warn!(%error, "wake log budget");
            }
            tracing::info!(
                ?path,
                confirmed,
                gate_open,
                trigger = hit.score,
                verifier = ?verifier_score,
                "wake"
            );
        }
        Err(error) => tracing::warn!(%error, "wake log"),
    }
    if !confirmed {
        return;
    }
    let id = uuid::Uuid::now_v7().to_string();
    publish_state(out_tx, screen, Phase::Listening, &id);
    let start = SessionStart {
        session_id: id.clone(),
        screen_id: screen.to_string(),
        unix_ms: unix_ms(),
        trigger_model_hash: hit.trigger_hash.to_string(),
        verifier_model_hash: hit.verifier_hash.to_string(),
        trigger_threshold: hit.config.trigger.threshold,
        verifier_threshold: hit.config.verifier.threshold,
        trigger_score: hit.score,
        verifier_score: verifier_score.unwrap_or(0.0),
    };
    if !offer(out_tx, Outbound::Start(start)) {
        tracing::warn!("dropped session start");
    }
    let frames = preroll_frames(clip);
    let seq = frames.len() as u32;
    if !offer(
        out_tx,
        Outbound::Preroll {
            session_id: id.clone(),
            frames,
        },
    ) {
        tracing::warn!("dropped preroll");
    }
    *session = Some(OpenSession {
        id,
        active: ActiveSession {
            started_ms: hit.now_ms,
            last_speech_ms: hit.now_ms,
        },
        streaming: false,
        seq,
    });
}

fn reload(
    path: &Path,
    config: &mut VoiceConfig,
    trigger: &mut Trigger,
    vad: &mut Vad,
) -> Result<(), String> {
    let fresh = crate::config::load(path).map_err(|error| error.to_string())?;
    config.apply_live(&fresh);
    apply_env(config)?;
    trigger.set_threshold(config.trigger.threshold)?;
    vad.set_threshold(config.vad.threshold)?;
    tracing::info!(
        trigger = config.trigger.threshold,
        verifier = config.verifier.threshold,
        onset_ms = config.gate.onset_ms,
        silence_ms = config.gate.silence_ms,
        vad = config.vad.threshold,
        stream_all = config.training.stream_all,
        "config reloaded"
    );
    Ok(())
}

fn publish_state(tx: &Sender<Outbound>, screen: &str, phase: Phase, session_id: &str) {
    let state = VoiceState {
        phase: phase as i32,
        session_id: session_id.to_string(),
        unix_ms: unix_ms(),
        screen_id: screen.to_string(),
    };
    if !offer(tx, Outbound::State(state)) {
        tracing::warn!(?phase, "dropped voice state");
    }
}

fn push_history(history: &mut VecDeque<i16>, sample: i16) {
    if history.len() == HISTORY {
        history.pop_front();
    }
    history.push_back(sample);
}

fn tail(samples: &[i16], count: usize) -> &[i16] {
    if samples.len() > count {
        &samples[samples.len() - count..]
    } else {
        samples
    }
}

fn session_matches(open: Option<&str>, requested: &str) -> bool {
    requested.is_empty() || open == Some(requested)
}

fn unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

fn apply_env(config: &mut VoiceConfig) -> Result<(), String> {
    if let Some(dir) = std::env::var_os("VOICE_LOG_DIR") {
        config.log.dir = std::path::PathBuf::from(dir);
    }
    if let Some(device) = std::env::var_os("VOICE_ALSA_DEVICE") {
        let device = device.to_string_lossy();
        if !device.is_empty() {
            config.capture.device = device.into_owned();
        }
    }
    crate::config::apply_stream_all_env(config)
}

fn watch_signals(cmd: std::sync::mpsc::Sender<Cmd>) {
    let Ok(mut signals) = signal_hook::iterator::Signals::new([signal_hook::consts::SIGHUP]) else {
        tracing::warn!("sighup handler was not installed");
        return;
    };
    for _ in &mut signals {
        let _ = cmd.send(Cmd::Reload);
    }
}

#[allow(dead_code)]
fn _stop(stop: Stop) -> Stop {
    stop
}
