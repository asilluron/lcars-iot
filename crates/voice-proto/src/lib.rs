//! `voice.v1` messages for wake-word state, session control, and transcripts.
//!
//! PCM frames are raw bytes. See [`audio_headers`].

use prost::Message;

pub mod v1 {
    include!(concat!(env!("OUT_DIR"), "/voice.v1.rs"));
}

/// Samples in one 20 ms mono frame at 16 kHz.
pub const FRAME_SAMPLES: usize = 320;

/// Bytes in one 20 ms S16LE frame.
pub const FRAME_BYTES: usize = FRAME_SAMPLES * 2;

/// Pre-roll frames. 75 × 20 ms is 1.5 s.
pub const PREROLL_FRAMES: usize = 75;

/// Samples in the 1.5 s pre-roll.
pub const PREROLL_SAMPLES: usize = FRAME_SAMPLES * PREROLL_FRAMES;

/// Capture rate the models expect.
pub const SAMPLE_RATE: u32 = 16_000;

/// NATS header carrying the session id on an audio frame.
pub const SESSION_ID: &str = "Session-Id";

/// NATS header carrying the frame sequence, starting at 0.
pub const SEQ: &str = "Seq";

/// NATS header: `preroll` or `live`.
pub const KIND: &str = "Kind";

pub const KIND_PREROLL: &str = "preroll";

pub const KIND_LIVE: &str = "live";

pub use prost;

/// NATS `Content-Type` header for a `voice.v1` message.
#[must_use]
pub fn content_type(message: &str) -> String {
    format!("application/x-protobuf; msg=voice.v1.{message}")
}

#[must_use]
pub fn encode(message: &impl Message) -> Vec<u8> {
    message.encode_to_vec()
}

pub fn decode<T: Message + Default>(bytes: &[u8]) -> Result<T, prost::DecodeError> {
    T::decode(bytes)
}

#[must_use]
pub fn state_subject(screen_id: &str) -> String {
    format!("voice.state.{screen_id}")
}

#[must_use]
pub fn session_start_subject(screen_id: &str) -> String {
    format!("voice.session.start.{screen_id}")
}

#[must_use]
pub fn audio_subject(screen_id: &str) -> String {
    format!("voice.audio.{screen_id}")
}

#[must_use]
pub fn endpoint_subject(screen_id: &str) -> String {
    format!("voice.session.endpoint.{screen_id}")
}

#[must_use]
pub fn cancel_subject(screen_id: &str) -> String {
    format!("voice.session.cancel.{screen_id}")
}

#[must_use]
pub fn partial_subject(screen_id: &str) -> String {
    format!("transcript.partial.{screen_id}")
}

#[must_use]
pub fn final_subject(screen_id: &str) -> String {
    format!("transcript.final.{screen_id}")
}

/// Headers on one raw PCM frame.
#[must_use]
pub fn audio_headers(session_id: &str, seq: u32, preroll: bool) -> [(&'static str, String); 3] {
    [
        (SESSION_ID, session_id.to_string()),
        (SEQ, seq.to_string()),
        (
            KIND,
            if preroll {
                KIND_PREROLL.to_string()
            } else {
                KIND_LIVE.to_string()
            },
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::v1::{Phase, SessionStart, Transcript, VoiceState};
    use super::{
        FRAME_BYTES, KIND_LIVE, KIND_PREROLL, PREROLL_FRAMES, audio_headers, audio_subject,
        cancel_subject, content_type, decode, encode, endpoint_subject, final_subject,
        partial_subject, session_start_subject, state_subject,
    };

    #[test]
    fn subjects_follow_the_screen() {
        assert_eq!(state_subject("kitchen"), "voice.state.kitchen");
        assert_eq!(
            session_start_subject("kitchen"),
            "voice.session.start.kitchen"
        );
        assert_eq!(audio_subject("kitchen"), "voice.audio.kitchen");
        assert_eq!(
            endpoint_subject("kitchen"),
            "voice.session.endpoint.kitchen"
        );
        assert_eq!(cancel_subject("kitchen"), "voice.session.cancel.kitchen");
        assert_eq!(partial_subject("kitchen"), "transcript.partial.kitchen");
        assert_eq!(final_subject("kitchen"), "transcript.final.kitchen");
    }

    #[test]
    fn state_round_trips() {
        let state = VoiceState {
            phase: Phase::Listening as i32,
            session_id: "018f".into(),
            unix_ms: 10,
            screen_id: "kitchen".into(),
        };
        let decoded: VoiceState = decode(&encode(&state)).unwrap();
        assert_eq!(decoded, state);
        assert_eq!(
            content_type("VoiceState"),
            "application/x-protobuf; msg=voice.v1.VoiceState"
        );
        let _ = SessionStart::default();
        let _ = Transcript::default();
    }

    #[test]
    fn preroll_headers_mark_the_first_frames() {
        let headers = audio_headers("abc", 0, true);
        assert_eq!(headers[0], ("Session-Id", "abc".to_string()));
        assert_eq!(headers[1], ("Seq", "0".to_string()));
        assert_eq!(headers[2], ("Kind", KIND_PREROLL.to_string()));
        assert_eq!(audio_headers("abc", 75, false)[2].1, KIND_LIVE);
        assert_eq!(PREROLL_FRAMES, 75);
        assert_eq!(FRAME_BYTES, 640);
    }
}
