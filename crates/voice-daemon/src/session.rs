//! When an open session stops sending audio.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub silence_ms: u64,
    pub hard_cap_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActiveSession {
    pub started_ms: u64,
    pub last_speech_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stop {
    ServerEndpoint,
    Silence,
    HardCap,
    Cancel,
}

impl ActiveSession {
    #[must_use]
    pub fn stop(&self, now_ms: u64, limits: Limits, endpoint: bool, cancel: bool) -> Option<Stop> {
        if cancel {
            return Some(Stop::Cancel);
        }
        if endpoint {
            return Some(Stop::ServerEndpoint);
        }
        if now_ms.saturating_sub(self.started_ms) >= limits.hard_cap_ms {
            return Some(Stop::HardCap);
        }
        if now_ms.saturating_sub(self.last_speech_ms) >= limits.silence_ms {
            return Some(Stop::Silence);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{ActiveSession, Limits, Stop};

    fn limits() -> Limits {
        Limits {
            silence_ms: 8_000,
            hard_cap_ms: 30_000,
        }
    }

    fn session() -> ActiveSession {
        ActiveSession {
            started_ms: 1_000,
            last_speech_ms: 1_000,
        }
    }

    #[test]
    fn eight_seconds_without_speech_ends_the_session() {
        let mut active = session();
        assert_eq!(active.stop(8_999, limits(), false, false), None);
        assert_eq!(
            active.stop(9_000, limits(), false, false),
            Some(Stop::Silence)
        );
        active.last_speech_ms = 5_000;
        assert_eq!(active.stop(12_999, limits(), false, false), None);
        assert_eq!(
            active.stop(13_000, limits(), false, false),
            Some(Stop::Silence)
        );
    }

    #[test]
    fn hard_cap_endpoint_and_cancel_win_in_order() {
        let active = session();
        assert_eq!(
            active.stop(1_100, limits(), false, true),
            Some(Stop::Cancel)
        );
        assert_eq!(
            active.stop(40_000, limits(), true, false),
            Some(Stop::ServerEndpoint)
        );
        assert_eq!(
            active.stop(31_000, limits(), false, false),
            Some(Stop::HardCap)
        );
    }
}
