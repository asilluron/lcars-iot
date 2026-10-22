//! Screen process. The calendar is local until `NATS_URL` is set.

mod agenda;
mod audio;
#[allow(dead_code)]
mod bridge;
mod clock;
mod config;
mod health;
#[allow(dead_code)]
mod power;
mod voice;

use std::time::Duration;

use chrono::{Datelike, Timelike, Weekday};
use config::BootstrapConfig;
use health::{materialize, touch_heartbeat};
use lcars_ui::AppWindow;
use lcars_ui::slint::{ComponentHandle, Timer, TimerMode};
use power::in_night;

fn main() {
    let config = match BootstrapConfig::from_env() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    };
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(&config.log_level))
        .init();
    clock::install_default();
    let files = match materialize(&config) {
        Ok(files) => files,
        Err(error) => {
            tracing::error!(%error, "failed to prepare the run directory");
            std::process::exit(1);
        }
    };
    let app = AppWindow::new().unwrap_or_else(|error| {
        tracing::error!(%error, "failed to open the display");
        std::process::exit(1);
    });
    app.set_version_text(env!("CARGO_PKG_VERSION").into());
    wire_audio(&app, &config);
    wire_night(&app, config.brightness_pct);
    publish_clock(&app);
    if !config.nats_urls.is_empty() {
        agenda::paint(
            &app,
            "LINKING",
            Some(agenda::local_week(clock::now().date_naive())),
        );
        let (voice_cancel, _) = tokio::sync::broadcast::channel(8);
        let cancel = voice_cancel.clone();
        app.on_voice_cancelled(move || {
            let _ = cancel.send(());
        });
        let link = agenda::AgendaLink {
            urls: config.nats_urls.clone(),
            ca_path: config
                .nats_ca_file
                .as_ref()
                .map(std::path::PathBuf::from)
                .or_else(|| files.ca_path.clone()),
            creds_path: files.creds_path.clone(),
            require_tls: config.nats_urls.iter().any(|url| url.starts_with("tls://")),
            screen_id: config.screen_id.clone(),
        };
        let weak = app.as_weak();
        std::thread::spawn(move || {
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    tracing::error!(%error, "failed to start the calendar runtime");
                    return;
                }
            };
            runtime.block_on(agenda::run(link, weak, voice_cancel));
        });
    }

    let clock = Timer::default();
    let weak = app.as_weak();
    clock.start(TimerMode::Repeated, Duration::from_secs(1), move || {
        if let Some(app) = weak.upgrade() {
            publish_clock(&app);
        }
    });

    let heartbeat = Timer::default();
    let dir = files.run_dir.clone();
    heartbeat.start(TimerMode::Repeated, Duration::from_secs(1), move || {
        touch_heartbeat(&dir);
    });

    if let Err(error) = app.run() {
        tracing::error!(%error, "ui stopped");
        std::process::exit(1);
    }
}

struct ClockFace {
    hm: String,
    suffix: &'static str,
    weekday: String,
    date_line: String,
    now_label: String,
}

fn wire_audio(app: &AppWindow, config: &BootstrapConfig) {
    audio::warm(config.audio_enabled);
    let enabled = config.audio_enabled;
    let volume = config.audio_volume;
    let night_start = config.night_start_min;
    let night_end = config.night_end_min;
    app.on_pressed(move |name| {
        let quiet = in_night(minute_of_day(), night_start, night_end);
        audio::cue(
            name.as_str(),
            audio::playback_volume(enabled, volume, quiet),
        );
    });
}

fn wire_night(app: &AppWindow, day_pct: u32) {
    const NIGHT_PCT: u32 = 28;
    app.on_night_changed(move |on| {
        let pct = if on { day_pct.min(NIGHT_PCT) } else { day_pct };
        power::write_brightness(pct);
    });
}

fn minute_of_day() -> u16 {
    let now = clock::now();
    (now.hour() as u16) * 60 + now.minute() as u16
}

fn publish_clock(app: &AppWindow) {
    let face = clock_face(clock::now());
    app.set_clock_hm(face.hm.into());
    app.set_clock_suffix(face.suffix.into());
    app.set_weekday_name(face.weekday.into());
    app.set_date_line(face.date_line.into());
    app.set_now_label(face.now_label.into());
}

fn clock_face(now: chrono::DateTime<impl chrono::TimeZone>) -> ClockFace {
    let (hour, suffix) = hour12(now.hour());
    let hm = format!("{hour}:{:02}", now.minute());
    let weekday = weekday_name(now.weekday()).to_string();
    let date_line = format!("{} {} {}", now.day(), month_name(now.month()), now.year());
    ClockFace {
        now_label: format!("NOW {hm} {suffix}"),
        hm,
        suffix,
        weekday,
        date_line,
    }
}

fn weekday_name(day: Weekday) -> &'static str {
    match day {
        Weekday::Mon => "MONDAY",
        Weekday::Tue => "TUESDAY",
        Weekday::Wed => "WEDNESDAY",
        Weekday::Thu => "THURSDAY",
        Weekday::Fri => "FRIDAY",
        Weekday::Sat => "SATURDAY",
        Weekday::Sun => "SUNDAY",
    }
}

fn month_name(month: u32) -> &'static str {
    match month {
        1 => "JANUARY",
        2 => "FEBRUARY",
        3 => "MARCH",
        4 => "APRIL",
        5 => "MAY",
        6 => "JUNE",
        7 => "JULY",
        8 => "AUGUST",
        9 => "SEPTEMBER",
        10 => "OCTOBER",
        11 => "NOVEMBER",
        _ => "DECEMBER",
    }
}

fn hour12(hour24: u32) -> (u32, &'static str) {
    let suffix = if hour24 < 12 { "AM" } else { "PM" };
    let hour = match hour24 % 12 {
        0 => 12,
        hour => hour,
    };
    (hour, suffix)
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::{clock_face, hour12};

    #[test]
    fn twelve_hour_clock_wraps_midnight_and_noon() {
        assert_eq!(hour12(0), (12, "AM"));
        assert_eq!(hour12(10), (10, "AM"));
        assert_eq!(hour12(12), (12, "PM"));
        assert_eq!(hour12(15), (3, "PM"));
    }

    #[test]
    fn face_matches_the_panel_date() {
        let now = chrono_tz::America::Denver
            .with_ymd_and_hms(2026, 10, 21, 10, 42, 0)
            .single()
            .expect("mountain time");
        let face = clock_face(now);
        assert_eq!(face.hm, "10:42");
        assert_eq!(face.suffix, "AM");
        assert_eq!(face.weekday, "WEDNESDAY");
        assert_eq!(face.date_line, "21 OCTOBER 2026");
        assert_eq!(face.now_label, "NOW 10:42 AM");
    }
}
