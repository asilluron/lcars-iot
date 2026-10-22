//! Calendar client for the lcars-api NATS service.
//!
//! The screen requests `lcars.calendar.get` and refreshes when
//! `lcars.calendar.updated` is published.

use std::path::PathBuf;
use std::time::Duration;

use chrono::{Datelike, NaiveDate, NaiveTime, TimeDelta, Timelike};
use futures::StreamExt;
use lcars_api_proto::prost::Message;
use lcars_api_proto::v1::{CalendarEvent, GetCalendarRequest, GetCalendarResponse};
use lcars_api_proto::{CALENDAR_GET, CALENDAR_UPDATED, ERROR, ERROR_CODE};
use lcars_ui::slint::{Color, ModelRc, SharedString, VecModel, Weak};
use lcars_ui::{AppWindow, DayItem, EventItem};

const TIMED_ROWS: usize = 6;
const SOON_MINUTES: i32 = 90;

pub struct AgendaLink {
    pub urls: Vec<String>,
    pub ca_path: Option<PathBuf>,
    pub creds_path: Option<PathBuf>,
    pub require_tls: bool,
    pub screen_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgendaModel {
    pub days: Vec<DayRow>,
    pub heads: Vec<String>,
    pub all_day: Vec<String>,
    pub events: Vec<EventRow>,
    pub week: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DayRow {
    pub num: String,
    pub name: String,
    pub dots: i32,
    pub count: i32,
    pub today: bool,
    pub weekend: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventRow {
    pub day: i32,
    pub time: String,
    pub suffix: String,
    pub title: String,
    pub detail: String,
    pub badge: String,
    pub tag: String,
    pub lane: String,
}

pub fn paint(app: &AppWindow, status: &str, model: Option<AgendaModel>) {
    app.set_status(status.into());
    let Some(model) = model else {
        return;
    };
    app.set_week_label(model.week.into());
    app.set_days(ModelRc::new(VecModel::from(
        model.days.into_iter().map(day_item).collect::<Vec<_>>(),
    )));
    app.set_day_heads(string_model(model.heads));
    app.set_all_day_labels(string_model(model.all_day));
    app.set_events(ModelRc::new(VecModel::from(
        model.events.into_iter().map(event_item).collect::<Vec<_>>(),
    )));
}

pub async fn run(
    link: AgendaLink,
    weak: Weak<AppWindow>,
    voice_cancel: tokio::sync::broadcast::Sender<()>,
) {
    let mut attempt = 0_u32;
    let mut had_calendar = false;
    loop {
        if !had_calendar {
            post(&weak, "LINKING", None).await;
        }
        match connect(&link).await {
            Ok(client) => {
                attempt = 0;
                tracing::info!("nats connected");
                let voice = tokio::spawn(crate::voice::watch(
                    client.clone(),
                    link.screen_id.clone(),
                    weak.clone(),
                    voice_cancel.subscribe(),
                ));
                loop {
                    match session(&client, &weak, &mut had_calendar).await {
                        Ok(()) => {
                            voice.abort();
                            return;
                        }
                        Err(error) => {
                            tracing::warn!(error = %error, "calendar session ended");
                            let status = status_for(&error);
                            post(&weak, &status, None).await;
                            if client.connection_state()
                                == async_nats::connection::State::Disconnected
                            {
                                break;
                            }
                            tokio::time::sleep(Duration::from_secs(5)).await;
                        }
                    }
                }
                voice.abort();
            }
            Err(error) => {
                tracing::warn!(error = %error, "nats connect failed");
                let model = if had_calendar {
                    None
                } else {
                    Some(local_week(crate::clock::now().date_naive()))
                };
                post(&weak, "NO LINK", model).await;
            }
        }
        let shift = attempt.min(4);
        let delay = Duration::from_secs(1_u64 << shift);
        attempt = attempt.saturating_add(1);
        tokio::time::sleep(delay).await;
    }
}

async fn session(
    client: &async_nats::Client,
    weak: &Weak<AppWindow>,
    had_calendar: &mut bool,
) -> Result<(), String> {
    let mut updates = client
        .subscribe(CALENDAR_UPDATED)
        .await
        .map_err(|err| err.to_string())?;
    loop {
        let calendar = fetch(client).await?;
        *had_calendar = true;
        crate::clock::adopt_server(&calendar.timezone);
        let now = crate::clock::now();
        let model = project(&calendar, now.time());
        let synced = clock_label(now.hour(), now.minute());
        post(weak, &format!("SYNCED {synced}"), Some(model)).await;
        tokio::select! {
            message = updates.next() => {
                if message.is_none() {
                    return Err("calendar updates closed".into());
                }
            }
            () = tokio::time::sleep(Duration::from_secs(5 * 60)) => {}
        }
    }
}

async fn fetch(client: &async_nats::Client) -> Result<GetCalendarResponse, String> {
    let request = GetCalendarRequest {
        from: String::new(),
        days: 7,
    };
    let reply = client
        .request(CALENDAR_GET, request.encode_to_vec().into())
        .await
        .map_err(|err| err.to_string())?;
    if let Some(code) = reply
        .headers
        .as_ref()
        .and_then(|headers| headers.get(ERROR_CODE))
    {
        let message = reply
            .headers
            .as_ref()
            .and_then(|headers| headers.get(ERROR))
            .map(|value| value.as_str())
            .unwrap_or_default();
        return Err(format!("server returned {}: {message}", code.as_str()));
    }
    GetCalendarResponse::decode(reply.payload).map_err(|err| err.to_string())
}

async fn connect(link: &AgendaLink) -> Result<async_nats::Client, String> {
    let mut options = async_nats::ConnectOptions::new()
        .name("lcars-screen")
        .connection_timeout(Duration::from_secs(5))
        .request_timeout(Some(Duration::from_secs(3)))
        .max_reconnects(None);
    if link.require_tls {
        options = options.require_tls(true);
    }
    if let Some(path) = &link.ca_path {
        options = options.add_root_certificates(path.clone());
    }
    if let Some(path) = &link.creds_path {
        options = options
            .credentials_file(path)
            .await
            .map_err(|err: std::io::Error| err.to_string())?;
    }
    let addrs = link
        .urls
        .iter()
        .map(|url| url.parse::<async_nats::ServerAddr>())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| err.to_string())?;
    options.connect(addrs).await.map_err(|err| err.to_string())
}

async fn post(weak: &Weak<AppWindow>, status: &str, model: Option<AgendaModel>) {
    for _ in 0..100 {
        let weak = weak.clone();
        let status = status.to_string();
        let model = model.clone();
        if lcars_ui::slint::invoke_from_event_loop(move || {
            if let Some(app) = weak.upgrade() {
                paint(&app, &status, model);
            }
        })
        .is_ok()
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tracing::warn!("ui event loop did not accept a calendar update");
}

fn status_for(error: &str) -> String {
    if error.contains("503") {
        "NO CALENDAR".into()
    } else {
        "NO LINK".into()
    }
}

/// Seven empty days starting at `today`, used before the first reply.
#[must_use]
pub fn local_week(today: NaiveDate) -> AgendaModel {
    let mut days = Vec::with_capacity(7);
    let mut heads = Vec::with_capacity(7);
    let mut all_day = Vec::with_capacity(7);
    for offset in 0..7 {
        let date = today
            .checked_add_signed(TimeDelta::days(offset))
            .unwrap_or(today);
        days.push(DayRow {
            num: date.day().to_string(),
            name: weekday_short(date.weekday().number_from_monday()),
            dots: 0,
            count: 0,
            today: offset == 0,
            weekend: date.weekday().number_from_monday() >= 6,
        });
        heads.push(format!(
            "{} {} {}",
            weekday_short(date.weekday().number_from_monday()),
            date.day(),
            month_short(date.month())
        ));
        all_day.push("No all-day items".into());
    }
    AgendaModel {
        days,
        heads,
        all_day,
        events: Vec::new(),
        week: week_label(today),
    }
}

/// Projects a server calendar onto the panel's week strip and event rows.
#[must_use]
pub fn project(calendar: &GetCalendarResponse, now: NaiveTime) -> AgendaModel {
    let mut days = Vec::with_capacity(calendar.days.len());
    let mut heads = Vec::with_capacity(calendar.days.len());
    let mut all_day = Vec::with_capacity(calendar.days.len());
    let mut events = Vec::new();
    let mut badge_used = false;
    for (index, day) in calendar.days.iter().enumerate() {
        let date = NaiveDate::parse_from_str(&day.date, "%Y-%m-%d").ok();
        let weekday = date
            .map(|value| value.weekday().number_from_monday())
            .unwrap_or(1);
        let name = short_name(&day.weekday, weekday);
        let num = date
            .map(|value| value.day().to_string())
            .unwrap_or_else(|| day.date.clone());
        let count = i32::try_from(day.events.len()).unwrap_or(i32::MAX);
        days.push(DayRow {
            num,
            name: name.clone(),
            dots: count.min(5),
            count,
            today: day.today,
            weekend: weekday >= 6,
        });
        heads.push(match date {
            Some(value) => format!("{} {} {}", name, value.day(), month_short(value.month())),
            None => name.clone(),
        });
        all_day.push(all_day_label(
            day.events.iter().filter(|event| event.all_day),
        ));
        let day_index = i32::try_from(index).unwrap_or(i32::MAX);
        let mut timed = 0;
        for event in &day.events {
            if event.all_day {
                continue;
            }
            if timed == TIMED_ROWS {
                break;
            }
            timed += 1;
            let (time, suffix) = event
                .start_time
                .as_deref()
                .map(clock_parts)
                .unwrap_or_else(|| ("—".into(), String::new()));
            let badge = if day.today && !badge_used {
                soon_badge(now, event.start_time.as_deref())
            } else {
                String::new()
            };
            if !badge.is_empty() {
                badge_used = true;
            }
            events.push(EventRow {
                day: day_index,
                time,
                suffix,
                title: upper_limit(
                    if event.title.is_empty() {
                        "UNTITLED"
                    } else {
                        &event.title
                    },
                    28,
                ),
                detail: detail(event),
                badge,
                tag: upper_limit(&event.calendar, 8),
                lane: lane_of(&event.calendar),
            });
        }
    }
    let week = NaiveDate::parse_from_str(&calendar.from, "%Y-%m-%d")
        .map(week_label)
        .unwrap_or_else(|_| "WEEK".into());
    AgendaModel {
        days,
        heads,
        all_day,
        events,
        week,
    }
}

fn all_day_label<'a>(events: impl Iterator<Item = &'a CalendarEvent>) -> String {
    let titles: Vec<String> = events
        .map(|event| {
            if event.title.is_empty() {
                "UNTITLED".into()
            } else {
                event.title.to_uppercase()
            }
        })
        .collect();
    if titles.is_empty() {
        return "No all-day items".into();
    }
    let joined = titles.join(" · ");
    if joined.chars().count() <= 42 {
        joined
    } else {
        let mut short: String = joined.chars().take(41).collect();
        short.push('…');
        short
    }
}

fn detail(event: &CalendarEvent) -> String {
    let when = match (event.start_time.as_deref(), event.end_time.as_deref()) {
        (Some(start), Some(end)) => format!("{} to {}", pretty(start), pretty(end)),
        (Some(start), None) => pretty(start),
        _ => "Continues".into(),
    };
    if event.location.is_empty() {
        when
    } else {
        format!("{when} · {}", event.location)
    }
}

fn soon_badge(now: NaiveTime, start: Option<&str>) -> String {
    let Some(start) = start else {
        return String::new();
    };
    let Some((hour, minute)) = parse_hhmm(start) else {
        return String::new();
    };
    if hour == 24 {
        return String::new();
    }
    let delta = (hour * 60 + minute) as i32 - (now.hour() * 60 + now.minute()) as i32;
    match delta {
        0 => "NOW".into(),
        1..=SOON_MINUTES => format!("IN {delta} MIN"),
        _ => String::new(),
    }
}

fn lane_of(calendar: &str) -> String {
    let name = calendar.to_ascii_lowercase();
    if name.contains("work") {
        "work"
    } else if name.contains("band") {
        "band"
    } else if name.contains("personal") {
        "personal"
    } else if name.contains("house") || name.contains("home") {
        "house"
    } else {
        "other"
    }
    .into()
}

fn clock_parts(hhmm: &str) -> (String, String) {
    let Some((hour, minute)) = parse_hhmm(hhmm) else {
        return (hhmm.to_string(), String::new());
    };
    let hour = if hour == 24 { 0 } else { hour };
    let (hour, suffix) = hour12(hour);
    (format!("{hour}:{minute:02}"), suffix.into())
}

fn pretty(hhmm: &str) -> String {
    let (time, suffix) = clock_parts(hhmm);
    if suffix.is_empty() {
        time
    } else {
        format!("{time} {suffix}")
    }
}

fn parse_hhmm(value: &str) -> Option<(u32, u32)> {
    let (hour, minute) = value.split_once(':')?;
    let hour = hour.parse().ok()?;
    let minute = minute.parse().ok()?;
    if hour > 24 || minute > 59 || (hour == 24 && minute != 0) {
        return None;
    }
    Some((hour, minute))
}

fn hour12(hour24: u32) -> (u32, &'static str) {
    let suffix = if hour24 < 12 { "AM" } else { "PM" };
    let hour = match hour24 % 12 {
        0 => 12,
        hour => hour,
    };
    (hour, suffix)
}

fn clock_label(hour24: u32, minute: u32) -> String {
    let (hour, suffix) = hour12(hour24);
    format!("{hour}:{minute:02} {suffix}")
}

fn week_label(date: NaiveDate) -> String {
    format!("WEEK {:02}", date.iso_week().week())
}

fn short_name(weekday: &str, number: u32) -> String {
    let letters: String = weekday.chars().take(3).collect();
    if letters.chars().count() == 3 {
        letters.to_uppercase()
    } else {
        weekday_short(number)
    }
}

fn weekday_short(number_from_monday: u32) -> String {
    match number_from_monday {
        1 => "MON",
        2 => "TUE",
        3 => "WED",
        4 => "THU",
        5 => "FRI",
        6 => "SAT",
        _ => "SUN",
    }
    .into()
}

fn month_short(month: u32) -> &'static str {
    match month {
        1 => "JAN",
        2 => "FEB",
        3 => "MAR",
        4 => "APR",
        5 => "MAY",
        6 => "JUN",
        7 => "JUL",
        8 => "AUG",
        9 => "SEP",
        10 => "OCT",
        11 => "NOV",
        _ => "DEC",
    }
}

fn upper_limit(value: &str, max: usize) -> String {
    let upper = value.to_uppercase();
    if upper.chars().count() <= max {
        upper
    } else {
        upper.chars().take(max).collect()
    }
}

fn day_item(day: DayRow) -> DayItem {
    DayItem {
        num: day.num.into(),
        name: day.name.into(),
        dots: day.dots,
        count: day.count,
        today: day.today,
        weekend: day.weekend,
        accent: if day.today {
            Color::from_rgb_u8(0xff, 0xe6, 0xbe)
        } else if day.weekend {
            Color::from_rgb_u8(0xc7, 0x9a, 0xdf)
        } else {
            Color::from_rgb_u8(0x9b, 0xa4, 0xff)
        },
    }
}

fn event_item(event: EventRow) -> EventItem {
    let accent = match event.lane.as_str() {
        "work" => Color::from_rgb_u8(0x9b, 0xa4, 0xff),
        "band" => Color::from_rgb_u8(0xff, 0x9c, 0x34),
        "personal" => Color::from_rgb_u8(0xfe, 0xc8, 0x93),
        "house" => Color::from_rgb_u8(0xc7, 0x9a, 0xdf),
        _ => Color::from_rgb_u8(0xff, 0x99, 0x66),
    };
    EventItem {
        day: event.day,
        time: event.time.into(),
        suffix: event.suffix.into(),
        title: event.title.into(),
        detail: event.detail.into(),
        badge: event.badge.into(),
        tag: event.tag.into(),
        lane: event.lane.into(),
        accent,
    }
}

fn string_model(values: Vec<String>) -> ModelRc<SharedString> {
    ModelRc::new(VecModel::from(
        values
            .into_iter()
            .map(SharedString::from)
            .collect::<Vec<_>>(),
    ))
}

#[cfg(test)]
mod tests {
    use chrono::NaiveTime;
    use lcars_api_proto::v1::{CalendarDay, CalendarEvent, GetCalendarResponse};

    use super::{local_week, project};

    #[test]
    fn projects_timed_and_all_day_events() {
        let calendar = GetCalendarResponse {
            timezone: "America/New_York".into(),
            from: "2030-03-04".into(),
            to: "2030-03-04".into(),
            days: vec![CalendarDay {
                date: "2030-03-04".into(),
                weekday: "Monday".into(),
                today: true,
                events: vec![
                    CalendarEvent {
                        title: "Soccer practice".into(),
                        calendar: "personal".into(),
                        all_day: true,
                        ..Default::default()
                    },
                    CalendarEvent {
                        title: "Team standup".into(),
                        calendar: "Work".into(),
                        location: "Community park".into(),
                        start_time: Some("09:00".into()),
                        end_time: Some("09:15".into()),
                        ..Default::default()
                    },
                ],
            }],
            ..Default::default()
        };
        let view = project(&calendar, NaiveTime::from_hms_opt(8, 30, 0).expect("time"));
        assert_eq!(view.week, "WEEK 10");
        assert_eq!(view.days[0].name, "MON");
        assert_eq!(view.days[0].num, "4");
        assert_eq!(view.days[0].count, 2);
        assert_eq!(view.days[0].dots, 2);
        assert!(view.days[0].today);
        assert_eq!(view.heads[0], "MON 4 MAR");
        assert_eq!(view.all_day[0], "SOCCER PRACTICE");
        assert_eq!(view.events.len(), 1);
        assert_eq!(view.events[0].title, "TEAM STANDUP");
        assert_eq!(view.events[0].time, "9:00");
        assert_eq!(view.events[0].suffix, "AM");
        assert_eq!(view.events[0].lane, "work");
        assert_eq!(view.events[0].tag, "WORK");
        assert_eq!(view.events[0].badge, "IN 30 MIN");
        assert_eq!(view.events[0].detail, "9:00 AM to 9:15 AM · Community park");
    }

    #[test]
    fn caps_timed_rows_and_leaves_other_calendars_visible() {
        let events = (0..8)
            .map(|index| CalendarEvent {
                title: format!("Session {index}"),
                calendar: "Town".into(),
                start_time: Some(format!("{index:02}:00")),
                end_time: Some(format!("{index:02}:30")),
                ..Default::default()
            })
            .collect();
        let calendar = GetCalendarResponse {
            from: "2030-03-09".into(),
            days: vec![CalendarDay {
                date: "2030-03-09".into(),
                weekday: "Saturday".into(),
                events,
                ..Default::default()
            }],
            ..Default::default()
        };
        let view = project(&calendar, NaiveTime::from_hms_opt(12, 0, 0).expect("time"));
        assert_eq!(view.events.len(), 6);
        assert!(view.events.iter().all(|event| event.lane == "other"));
        assert!(view.days[0].weekend);
        assert_eq!(view.days[0].count, 8);
        assert_eq!(view.days[0].dots, 5);
    }

    #[test]
    fn local_week_is_seven_empty_days() {
        let today = chrono::NaiveDate::from_ymd_opt(2030, 3, 4).expect("date");
        let view = local_week(today);
        assert_eq!(view.days.len(), 7);
        assert_eq!(view.days[0].name, "MON");
        assert!(view.days[0].today);
        assert!(view.events.is_empty());
        assert_eq!(view.all_day[0], "No all-day items");
        assert_eq!(view.week, "WEEK 10");
    }
}
