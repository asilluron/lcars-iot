//! Ordered messages toward NATS. A full queue drops the new item.

use tokio::sync::mpsc::error::TrySendError;
use voice_proto::v1::{SessionStart, VoiceState};

pub const QUEUE_DEPTH: usize = 16;

pub struct AudioFrame {
    pub session_id: String,
    pub seq: u32,
    pub pcm: Vec<u8>,
}

pub enum Outbound {
    State(VoiceState),
    Start(SessionStart),
    Preroll {
        session_id: String,
        frames: Vec<Vec<u8>>,
    },
    Frame(AudioFrame),
}

#[must_use]
pub fn offer(tx: &tokio::sync::mpsc::Sender<Outbound>, item: Outbound) -> bool {
    match tx.try_send(item) {
        Ok(()) => true,
        Err(TrySendError::Full(_)) | Err(TrySendError::Closed(_)) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::{Outbound, offer};
    use voice_proto::v1::VoiceState;

    #[test]
    fn a_full_queue_drops_the_new_frame() {
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        assert!(offer(
            &tx,
            Outbound::State(VoiceState {
                phase: 1,
                ..Default::default()
            })
        ));
        assert!(!offer(
            &tx,
            Outbound::State(VoiceState {
                phase: 2,
                ..Default::default()
            })
        ));
    }
}
