//! NATS subjects and KV keys from the technical specification.

pub const KV_STATE: &str = "home_state";
pub const KV_CONFIG: &str = "ui_config";
pub const STREAM_EVENTS: &str = "EVENTS";
pub const BACKEND_KEY: &str = "sys.backend";

#[must_use]
pub fn command_subject(entity_id: &str) -> String {
    format!("cmd.{entity_id}")
}

#[must_use]
pub fn layout_key(screen_id: &str) -> String {
    format!("screen.{screen_id}")
}

#[must_use]
pub fn heartbeat_subject(screen_id: &str) -> String {
    format!("ui.{screen_id}.heartbeat")
}

#[must_use]
pub fn error_subject(screen_id: &str) -> String {
    format!("ui.{screen_id}.error")
}

#[cfg(test)]
mod tests {
    use super::{command_subject, error_subject, heartbeat_subject, layout_key};

    #[test]
    fn subjects_match_the_spec() {
        assert_eq!(command_subject("light.kitchen"), "cmd.light.kitchen");
        assert_eq!(layout_key("kitchen"), "screen.kitchen");
        assert_eq!(heartbeat_subject("kitchen"), "ui.kitchen.heartbeat");
        assert_eq!(error_subject("kitchen"), "ui.kitchen.error");
    }
}
