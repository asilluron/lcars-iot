//! `lcars.v1` messages for the lcars-api NATS calendar.
//!
//! The schema is `proto/lcars/v1/calendar.proto`, copied from lcars-api.
//! Subjects and error headers match that server.

use prost::Message;

pub mod v1 {
    include!(concat!(env!("OUT_DIR"), "/lcars.v1.rs"));
}

/// Request a [`v1::GetCalendarRequest`]; the reply is a [`v1::GetCalendarResponse`].
pub const CALENDAR_GET: &str = "lcars.calendar.get";

/// A [`v1::CalendarUpdated`] is published here when the calendar changes.
pub const CALENDAR_UPDATED: &str = "lcars.calendar.updated";

/// HTTP-style status on an error reply, such as `"503"`.
pub const ERROR_CODE: &str = "Nats-Service-Error-Code";

/// Human-readable message on an error reply.
pub const ERROR: &str = "Nats-Service-Error";

pub use prost;

#[must_use]
pub fn encode(message: &impl Message) -> Vec<u8> {
    message.encode_to_vec()
}

pub fn decode<T: Message + Default>(bytes: &[u8]) -> Result<T, prost::DecodeError> {
    T::decode(bytes)
}

#[cfg(test)]
mod tests {
    use super::v1::{GetCalendarRequest, GetCalendarResponse};
    use super::{CALENDAR_GET, CALENDAR_UPDATED, decode, encode};

    #[test]
    fn calendar_request_round_trips() {
        let request = GetCalendarRequest {
            from: "2030-03-04".into(),
            days: 7,
        };
        let decoded: GetCalendarRequest = decode(&encode(&request)).unwrap();
        assert_eq!(decoded, request);
        let _ = GetCalendarResponse::default();
        assert_eq!(CALENDAR_GET, "lcars.calendar.get");
        assert_eq!(CALENDAR_UPDATED, "lcars.calendar.updated");
    }
}
