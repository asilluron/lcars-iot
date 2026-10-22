//! Cross-thread commands. Capture, NATS, and `SIGHUP` all produce these.

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Cmd {
    Endpoint {
        session_id: String,
    },
    Cancel {
        session_id: String,
    },
    Reload,
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    MicOnline,
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    MicOffline,
}
