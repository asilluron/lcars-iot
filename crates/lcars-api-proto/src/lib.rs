//! `lcars.v1` messages for the lcars-api NATS calendar, camera catalog,
//! locks, reminders, training audio upload, and weather.
//!
//! The schemas are `proto/lcars/v1/calendar.proto`,
//! `proto/lcars/v1/cameras.proto`, `proto/lcars/v1/locks.proto`,
//! `proto/lcars/v1/reminders.proto`, `proto/lcars/v1/training.proto`, and
//! `proto/lcars/v1/weather.proto`, copied from lcars-api. Subjects and
//! error headers match that server. Video is not a NATS body:
//! `stream_path` and `snapshot_path` are HTTP paths on the same server.

use prost::Message;

pub mod v1 {
    include!(concat!(env!("OUT_DIR"), "/lcars.v1.rs"));
}

/// Request a [`v1::GetCalendarRequest`]; the reply is a [`v1::GetCalendarResponse`].
pub const CALENDAR_GET: &str = "lcars.calendar.get";

/// A [`v1::CalendarUpdated`] is published here when the calendar changes.
pub const CALENDAR_UPDATED: &str = "lcars.calendar.updated";

/// Request a [`v1::ListCamerasRequest`]; the reply is a [`v1::ListCamerasResponse`].
pub const CAMERAS_LIST: &str = "lcars.cameras.list";

/// Request a [`v1::ListLocksRequest`]; the reply is a [`v1::ListLocksResponse`].
pub const LOCKS_LIST: &str = "lcars.locks.list";

/// Send a [`v1::LockDoorRequest`]; the reply is a [`v1::LockDoorResponse`].
pub const LOCKS_LOCK: &str = "lcars.locks.lock";

/// Send a [`v1::UnlockDoorRequest`]; the reply is a [`v1::UnlockDoorResponse`].
pub const LOCKS_UNLOCK: &str = "lcars.locks.unlock";

/// Request a [`v1::ListRemindersRequest`]; the reply is a [`v1::ListRemindersResponse`].
pub const REMINDERS_LIST: &str = "lcars.reminders.list";

/// Request a [`v1::SetReminderRequest`]; the reply is the list afterwards.
pub const REMINDERS_SET: &str = "lcars.reminders.set";

/// A [`v1::RemindersUpdated`] is published here when the reminder list changes.
pub const REMINDERS_UPDATED: &str = "lcars.reminders.updated";

/// Send a [`v1::PutTrainingAudioRequest`]; the reply is a
/// [`v1::PutTrainingAudioResponse`].
pub const TRAINING_PUT: &str = "lcars.training.put";

/// Request a [`v1::GetWeatherRequest`]; the reply is a [`v1::GetWeatherResponse`].
pub const WEATHER_GET: &str = "lcars.weather.get";

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
    use super::v1::{
        Camera, GetCalendarRequest, GetCalendarResponse, GetWeatherRequest, GetWeatherResponse,
        ListCamerasRequest, ListCamerasResponse, ListLocksRequest, ListLocksResponse,
        ListRemindersRequest, Lock, LockDoorRequest, PutTrainingAudioRequest, Reminder,
        SetReminderRequest, UnlockDoorRequest,
    };
    use super::{
        CALENDAR_GET, CALENDAR_UPDATED, CAMERAS_LIST, LOCKS_LIST, LOCKS_LOCK, LOCKS_UNLOCK,
        REMINDERS_LIST, REMINDERS_SET, REMINDERS_UPDATED, TRAINING_PUT, WEATHER_GET, decode,
        encode,
    };

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

    #[test]
    fn training_audio_round_trips() {
        let training = PutTrainingAudioRequest {
            screen_id: "kitchen".into(),
            pcm: vec![0, 0, 1, 0],
        };
        let decoded: PutTrainingAudioRequest = decode(&encode(&training)).unwrap();
        assert_eq!(decoded, training);
        assert_eq!(TRAINING_PUT, "lcars.training.put");
    }

    #[test]
    fn camera_list_round_trips() {
        let request = ListCamerasRequest {};
        let decoded: ListCamerasRequest = decode(&encode(&request)).unwrap();
        assert_eq!(decoded, request);
        let response = ListCamerasResponse {
            cameras: vec![Camera {
                sn: "CAM1".into(),
                name: "Front door".into(),
                has_camera: true,
                stream_path: "/cameras/CAM1/stream".into(),
                snapshot_path: "/cameras/CAM1/snapshot".into(),
            }],
        };
        let decoded: ListCamerasResponse = decode(&encode(&response)).unwrap();
        assert_eq!(decoded, response);
        assert_eq!(CAMERAS_LIST, "lcars.cameras.list");
    }

    #[test]
    fn lock_messages_round_trip() {
        let request = ListLocksRequest {};
        let decoded: ListLocksRequest = decode(&encode(&request)).unwrap();
        assert_eq!(decoded, request);
        let response = ListLocksResponse {
            locks: vec![Lock {
                id: "11111111-1111-1111-1111-111111111111".into(),
                name: "Front door".into(),
                locked: true,
                online: true,
                can_lock: true,
                can_unlock: true,
            }],
        };
        let decoded: ListLocksResponse = decode(&encode(&response)).unwrap();
        assert_eq!(decoded, response);
        let lock = LockDoorRequest {
            id: "11111111-1111-1111-1111-111111111111".into(),
        };
        assert_eq!(decode::<LockDoorRequest>(&encode(&lock)).unwrap(), lock);
        let unlock = UnlockDoorRequest {
            id: "11111111-1111-1111-1111-111111111111".into(),
        };
        assert_eq!(
            decode::<UnlockDoorRequest>(&encode(&unlock)).unwrap(),
            unlock
        );
        assert_eq!(LOCKS_LIST, "lcars.locks.list");
        assert_eq!(LOCKS_LOCK, "lcars.locks.lock");
        assert_eq!(LOCKS_UNLOCK, "lcars.locks.unlock");
    }

    #[test]
    fn reminder_round_trips() {
        let request = SetReminderRequest {
            id: "rem_7f3k2m9q4x8w".into(),
            done: true,
        };
        let decoded: SetReminderRequest = decode(&encode(&request)).unwrap();
        assert_eq!(decoded, request);
        let _ = ListRemindersRequest::default();
        let _ = Reminder::default();
        assert_eq!(REMINDERS_LIST, "lcars.reminders.list");
        assert_eq!(REMINDERS_SET, "lcars.reminders.set");
        assert_eq!(REMINDERS_UPDATED, "lcars.reminders.updated");
    }

    #[test]
    fn weather_round_trips() {
        let request = GetWeatherRequest {};
        let decoded: GetWeatherRequest = decode(&encode(&request)).unwrap();
        assert_eq!(decoded, request);
        let response = GetWeatherResponse {
            summary: "Clear".into(),
            temperature_f: 72,
            feels_like_f: 70,
            high_f: 78,
            low_f: 55,
            wind_mph: 4,
            observed_at_unix: 1_895_000_000,
        };
        let decoded: GetWeatherResponse = decode(&encode(&response)).unwrap();
        assert_eq!(decoded, response);
        assert_eq!(WEATHER_GET, "lcars.weather.get");
    }
}
