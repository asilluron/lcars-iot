//! Wake-word state on the hub connection the calendar already uses.

use std::time::{Duration, Instant};

use futures::StreamExt;
use lcars_ui::AppWindow;
use lcars_ui::slint::Weak;
use voice_proto::prost::Message;
use voice_proto::v1::{Phase, SessionCancel, Transcript, VoiceState};
use voice_proto::{cancel_subject, final_subject, partial_subject, state_subject};

pub const PROCESSING_HOLD: Duration = Duration::from_secs(3);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VoiceSnap {
    pub phase: Phase,
    pub partial: String,
    pub final_text: String,
    pub since_processing: Duration,
    pub awaiting_final: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VoiceView {
    pub band: bool,
    pub offline: bool,
    pub label: String,
    pub text: String,
    pub wake_screen: bool,
}

impl VoiceSnap {
    #[must_use]
    pub fn idle() -> Self {
        Self {
            phase: Phase::Idle,
            partial: String::new(),
            final_text: String::new(),
            since_processing: Duration::ZERO,
            awaiting_final: false,
        }
    }
}

/// What the band should show. Processing stays up for [`PROCESSING_HOLD`] so a
/// missing final, or an early idle from the daemon, does not blank the panel.
#[must_use]
pub fn project(snap: &VoiceSnap) -> VoiceView {
    let held = snap.awaiting_final && snap.since_processing < PROCESSING_HOLD;
    let phase = match snap.phase {
        Phase::MicOffline => Phase::MicOffline,
        Phase::Listening | Phase::Streaming => snap.phase,
        Phase::Processing if !held => Phase::Idle,
        Phase::Processing => Phase::Processing,
        Phase::Idle | Phase::Unspecified => {
            if held {
                Phase::Processing
            } else {
                Phase::Idle
            }
        }
    };
    let band = matches!(
        phase,
        Phase::Listening | Phase::Streaming | Phase::Processing
    );
    let text = if phase == Phase::Processing && !snap.final_text.is_empty() {
        snap.final_text.clone()
    } else if band {
        snap.partial.clone()
    } else {
        String::new()
    };
    VoiceView {
        band,
        offline: phase == Phase::MicOffline,
        label: phase_label(phase).to_string(),
        text,
        wake_screen: band,
    }
}

#[must_use]
pub fn phase_label(phase: Phase) -> &'static str {
    match phase {
        Phase::Listening => "LISTENING",
        Phase::Streaming => "STREAMING",
        Phase::Processing => "PROCESSING",
        Phase::MicOffline => "MIC OFFLINE",
        Phase::Idle | Phase::Unspecified => "",
    }
}

pub fn paint(app: &AppWindow, view: &VoiceView) {
    app.set_voice_band(view.band);
    app.set_voice_phase(view.label.as_str().into());
    app.set_voice_text(view.text.as_str().into());
    app.set_voice_offline(view.offline);
    if view.wake_screen {
        app.set_screen_off(false);
    }
}

pub async fn watch(
    client: async_nats::Client,
    screen_id: String,
    weak: Weak<AppWindow>,
    cancel: tokio::sync::broadcast::Receiver<()>,
) {
    clear_session_state(&weak).await;
    let result = watch_inner(client, screen_id, weak.clone(), cancel).await;
    clear_session_state(&weak).await;
    result
}

async fn clear_session_state(weak: &Weak<AppWindow>) {
    for _ in 0..100 {
        let weak = weak.clone();
        if lcars_ui::slint::invoke_from_event_loop(move || {
            if let Some(app) = weak.upgrade() {
                app.set_voice_band(false);
                app.set_voice_phase("".into());
                app.set_voice_text("".into());
            }
        })
        .is_ok()
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn watch_inner(
    client: async_nats::Client,
    screen_id: String,
    weak: Weak<AppWindow>,
    mut cancel: tokio::sync::broadcast::Receiver<()>,
) {
    let Ok(mut states) = client.subscribe(state_subject(&screen_id)).await else {
        tracing::warn!("voice state subscribe failed");
        return;
    };
    let Ok(mut partials) = client.subscribe(partial_subject(&screen_id)).await else {
        tracing::warn!("partial transcript subscribe failed");
        return;
    };
    let Ok(mut finals) = client.subscribe(final_subject(&screen_id)).await else {
        tracing::warn!("final transcript subscribe failed");
        return;
    };
    let mut snap = VoiceSnap::idle();
    let mut processing_at: Option<Instant> = None;
    let mut tick = tokio::time::interval(Duration::from_millis(200));
    loop {
        tokio::select! {
            message = states.next() => {
                let Some(message) = message else { return };
                let Ok(state) = VoiceState::decode(message.payload) else {
                    continue;
                };
                let phase = Phase::try_from(state.phase).unwrap_or(Phase::Unspecified);
                if phase == Phase::Listening {
                    snap.partial.clear();
                    snap.final_text.clear();
                    snap.awaiting_final = false;
                    processing_at = None;
                }
                if phase == Phase::Processing && processing_at.is_none() {
                    processing_at = Some(Instant::now());
                    snap.awaiting_final = true;
                }
                if phase == Phase::MicOffline {
                    snap.awaiting_final = false;
                    processing_at = None;
                }
                snap.phase = phase;
                snap.since_processing = processing_at.map(|at| at.elapsed()).unwrap_or_default();
                render(&weak, &snap).await;
            }
            message = partials.next() => {
                let Some(message) = message else { return };
                if let Ok(transcript) = Transcript::decode(message.payload) {
                    snap.partial = transcript.text;
                    render(&weak, &snap).await;
                }
            }
            message = finals.next() => {
                let Some(message) = message else { return };
                if let Ok(transcript) = Transcript::decode(message.payload) {
                    snap.final_text = transcript.text;
                    render(&weak, &snap).await;
                }
            }
            result = cancel.recv() => {
                if result.is_err() {
                    continue;
                }
                let cancel = SessionCancel {
                    session_id: String::new(),
                    screen_id: screen_id.clone(),
                    unix_ms: unix_ms(),
                };
                if let Err(error) = client
                    .publish(cancel_subject(&screen_id), cancel.encode_to_vec().into())
                    .await
                {
                    tracing::warn!(%error, "voice cancel was not published");
                }
            }
            _ = tick.tick() => {
                if let Some(at) = processing_at {
                    snap.since_processing = at.elapsed();
                    let view = project(&snap);
                    if !view.band && snap.phase != Phase::MicOffline {
                        snap.awaiting_final = false;
                        processing_at = None;
                        snap.phase = Phase::Idle;
                    }
                    render(&weak, &snap).await;
                }
            }
        }
    }
}

async fn render(weak: &Weak<AppWindow>, snap: &VoiceSnap) {
    let view = project(snap);
    if view_changed(&view) {
        tracing::info!(
            band = view.band,
            offline = view.offline,
            label = %view.label,
            text = %view.text,
            "voice band"
        );
    }
    for _ in 0..100 {
        let weak = weak.clone();
        let view = view.clone();
        if lcars_ui::slint::invoke_from_event_loop(move || {
            if let Some(app) = weak.upgrade() {
                paint(&app, &view);
            }
        })
        .is_ok()
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn view_changed(view: &VoiceView) -> bool {
    use std::sync::Mutex;
    static LAST: Mutex<Option<VoiceView>> = Mutex::new(None);
    let Ok(mut last) = LAST.lock() else {
        return true;
    };
    if last.as_ref() == Some(view) {
        return false;
    }
    *last = Some(view.clone());
    true
}

fn unix_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use voice_proto::v1::Phase;

    use super::{PROCESSING_HOLD, VoiceSnap, project};

    fn snap(phase: Phase) -> VoiceSnap {
        VoiceSnap {
            phase,
            partial: "lights".into(),
            final_text: String::new(),
            since_processing: Duration::from_millis(100),
            awaiting_final: false,
        }
    }

    #[test]
    fn listening_and_streaming_show_the_band() {
        let listening = project(&snap(Phase::Listening));
        assert!(listening.band);
        assert!(listening.wake_screen);
        assert_eq!(listening.label, "LISTENING");
        assert_eq!(listening.text, "lights");
        let streaming = project(&snap(Phase::Streaming));
        assert_eq!(streaming.label, "STREAMING");
        assert!(!streaming.offline);
    }

    #[test]
    fn processing_waits_for_a_final_then_gives_up() {
        let mut waiting = snap(Phase::Processing);
        waiting.awaiting_final = true;
        waiting.since_processing = Duration::from_millis(500);
        let view = project(&waiting);
        assert!(view.band);
        assert_eq!(view.label, "PROCESSING");
        assert_eq!(view.text, "lights");

        waiting.final_text = "lights on".into();
        let view = project(&waiting);
        assert_eq!(view.text, "lights on");
        assert!(view.band);

        waiting.since_processing = PROCESSING_HOLD;
        let view = project(&waiting);
        assert!(!view.band);

        let mut idle = snap(Phase::Idle);
        idle.awaiting_final = true;
        idle.since_processing = Duration::from_secs(1);
        assert!(project(&idle).band);
        idle.since_processing = PROCESSING_HOLD;
        assert!(!project(&idle).band);
    }

    #[test]
    fn mic_offline_is_a_chip() {
        let view = project(&snap(Phase::MicOffline));
        assert!(view.offline);
        assert!(!view.band);
        assert_eq!(view.label, "MIC OFFLINE");
    }
}
