//! Wall clock. Mountain time until the calendar server names its own zone.

use std::sync::RwLock;

use chrono::{DateTime, Utc};
use chrono_tz::Tz;

const MOUNTAIN: Tz = chrono_tz::America::Denver;

static ZONE: RwLock<Tz> = RwLock::new(MOUNTAIN);

/// Use `TZ` when it names a real zone. Otherwise stay on Mountain time.
pub fn install_default() {
    let Some(name) = std::env::var("TZ").ok().filter(|name| !name.is_empty()) else {
        tracing::info!(zone = MOUNTAIN.name(), "clock timezone");
        return;
    };
    if adopt(&name) {
        tracing::info!(zone = name, "clock timezone");
    } else {
        tracing::warn!(zone = name, "unknown TZ; clock stays on Mountain time");
    }
}

/// Follow the zone from a calendar reply. An empty or unknown name leaves
/// the clock where it is.
pub fn adopt_server(name: &str) {
    let name = name.trim();
    if name.is_empty() {
        return;
    }
    let before = current();
    if !adopt(name) {
        tracing::warn!(zone = name, "unknown server timezone; clock unchanged");
        return;
    }
    if current() != before {
        tracing::info!(zone = name, "clock set from the server");
    }
}

pub fn now() -> DateTime<Tz> {
    Utc::now().with_timezone(&current())
}

fn current() -> Tz {
    *ZONE.read().unwrap_or_else(|err| err.into_inner())
}

fn adopt(name: &str) -> bool {
    let Ok(tz) = name.parse::<Tz>() else {
        return false;
    };
    let mut zone = ZONE.write().unwrap_or_else(|err| err.into_inner());
    *zone = tz;
    true
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Timelike};

    use super::MOUNTAIN;

    #[test]
    fn october_evening_is_not_utc() {
        let utc = chrono::Utc
            .with_ymd_and_hms(2026, 10, 22, 3, 2, 0)
            .single()
            .expect("utc");
        let local = utc.with_timezone(&MOUNTAIN);
        assert_eq!((local.hour(), local.minute()), (21, 2));
    }

    #[test]
    fn january_is_standard_time() {
        let utc = chrono::Utc
            .with_ymd_and_hms(2026, 1, 15, 3, 2, 0)
            .single()
            .expect("utc");
        let local = utc.with_timezone(&MOUNTAIN);
        assert_eq!((local.hour(), local.minute()), (20, 2));
    }

    #[test]
    fn server_name_parses_and_unknown_names_do_not() {
        assert_eq!(
            "America/New_York".parse::<chrono_tz::Tz>().ok(),
            Some(chrono_tz::America::New_York)
        );
        assert!("Mars/Olympus_Mons".parse::<chrono_tz::Tz>().is_err());
    }
}
