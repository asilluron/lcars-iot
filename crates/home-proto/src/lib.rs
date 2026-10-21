//! Generated `home.v1` messages shared with the Go backend.

use prost::Message;

pub mod v1 {
    include!(concat!(env!("OUT_DIR"), "/home.v1.rs"));
}

/// NATS `Content-Type` header for a `home.v1` message.
#[must_use]
pub fn content_type(message: &str) -> String {
    format!("application/x-protobuf; msg=home.v1.{message}")
}

#[must_use]
pub fn encode(message: &impl Message) -> Vec<u8> {
    message.encode_to_vec()
}

pub fn decode<T: Message + Default>(bytes: &[u8]) -> Result<T, prost::DecodeError> {
    T::decode(bytes)
}

#[cfg(test)]
mod tests {
    use super::v1::{Command, EntityState, LightState, SetOnOff, command, entity_state};
    use super::{content_type, decode, encode};

    #[test]
    fn round_trip_entity_and_content_type() {
        let state = EntityState {
            id: "light.kitchen".into(),
            name: "Kitchen".into(),
            available: true,
            state: Some(entity_state::State::Light(LightState {
                on: true,
                brightness_pct: 80,
                ..Default::default()
            })),
            ..Default::default()
        };
        let decoded: EntityState = decode(&encode(&state)).unwrap();
        assert_eq!(decoded, state);
        assert_eq!(
            content_type("Command"),
            "application/x-protobuf; msg=home.v1.Command"
        );

        let command = Command {
            command_id: "018f".into(),
            entity_id: "light.kitchen".into(),
            action: Some(command::Action::SetOnOff(SetOnOff { on: false })),
            ..Default::default()
        };
        let decoded: Command = decode(&encode(&command)).unwrap();
        assert_eq!(decoded.entity_id, "light.kitchen");
    }
}
