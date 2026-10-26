//! Current conditions for the home readout.
//!
//! The report comes from `lcars.weather.get`. The server chooses the location;
//! this process only displays the numbers.

use std::time::Duration;

use lcars_api_proto::prost::Message;
use lcars_api_proto::v1::{GetWeatherRequest, GetWeatherResponse};
use lcars_api_proto::{ERROR_CODE, WEATHER_GET};
use lcars_ui::AppWindow;
use lcars_ui::slint::Weak;
use tokio::sync::watch;

const REFRESH: Duration = Duration::from_secs(10 * 60);

pub async fn run(mut clients: watch::Receiver<Option<async_nats::Client>>, weak: Weak<AppWindow>) {
    let mut tick = tokio::time::interval(REFRESH);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            changed = clients.changed() => {
                if changed.is_err() {
                    return;
                }
                refresh(&clients, &weak).await;
            }
            _ = tick.tick() => refresh(&clients, &weak).await,
        }
    }
}

async fn refresh(clients: &watch::Receiver<Option<async_nats::Client>>, weak: &Weak<AppWindow>) {
    let Some(client) = clients.borrow().clone() else {
        return;
    };
    match fetch(&client).await {
        Ok(report) => {
            tracing::info!(
                summary = %report.summary,
                temperature_f = report.temperature_f,
                "weather report"
            );
            show(weak, report).await;
        }
        Err(error) => tracing::warn!(error = %error, "weather report failed"),
    }
}

async fn fetch(client: &async_nats::Client) -> Result<GetWeatherResponse, String> {
    let reply = tokio::time::timeout(
        Duration::from_secs(12),
        client.request(WEATHER_GET, GetWeatherRequest {}.encode_to_vec().into()),
    )
    .await
    .map_err(|_| "timed out".to_string())?
    .map_err(|error| error.to_string())?;
    if let Some(code) = reply
        .headers
        .as_ref()
        .and_then(|headers| headers.get(ERROR_CODE))
    {
        return Err(format!("status {}", code.as_str()));
    }
    GetWeatherResponse::decode(reply.payload).map_err(|error| error.to_string())
}

async fn show(weak: &Weak<AppWindow>, report: GetWeatherResponse) {
    for _ in 0..100 {
        let weak = weak.clone();
        let report = report.clone();
        if lcars_ui::slint::invoke_from_event_loop(move || apply(&weak, &report)).is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tracing::warn!("ui event loop did not accept a weather update");
}

fn apply(weak: &Weak<AppWindow>, report: &GetWeatherResponse) {
    let Some(app) = weak.upgrade() else {
        return;
    };
    app.set_weather_temp(report.temperature_f.to_string().into());
    app.set_weather_summary(report.summary.as_str().into());
    app.set_weather_detail(format!("Feels {}°", report.feels_like_f).into());
    app.set_weather_range(format!("High {} · Low {} °F", report.high_f, report.low_f).into());
    app.set_weather_wind(format!("Wind {} mph", report.wind_mph).into());
}
