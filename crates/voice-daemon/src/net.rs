//! Publish state and PCM on the hub. A down link drops audio; it is not queued.
//! Training chunks are requested on `lcars.training.put` from a side task, so a
//! slow upload does not stall wake-word audio.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_nats::HeaderMap;
use futures::StreamExt;
use voice_proto::v1::{SessionCancel, SessionEndpoint, SessionStart, VoiceState};
use voice_proto::{
    audio_headers, audio_subject, cancel_subject, content_type, endpoint_subject,
    session_start_subject, state_subject,
};

use crate::cmd::Cmd;
use crate::link::Hub;
use crate::outbound::{AudioFrame, Outbound};

pub async fn run(
    hub: Hub,
    screen_id: String,
    mut inbound: tokio::sync::mpsc::Receiver<Outbound>,
    training: tokio::sync::mpsc::Receiver<Vec<u8>>,
    cmd: std::sync::mpsc::Sender<Cmd>,
) {
    let slot = Arc::new(Mutex::new(None));
    let upload_slot = Arc::clone(&slot);
    let upload_screen = screen_id.clone();
    tokio::spawn(async move { upload_training(upload_slot, upload_screen, training).await });

    let mut client: Option<async_nats::Client> = None;
    let mut endpoint = None;
    let mut cancel = None;
    let mut retry = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            _ = retry.tick() => {
                if client.is_none() {
                    connect(&hub, &screen_id, &mut client, &mut endpoint, &mut cancel).await;
                    remember(&slot, &client);
                }
            }
            item = inbound.recv() => {
                let Some(item) = item else { return };
                let Some(current) = client.as_ref() else { continue };
                if let Err(error) = publish(current, &screen_id, item).await {
                    tracing::warn!(%error, "voice publish failed");
                    client = None;
                    endpoint = None;
                    cancel = None;
                    remember(&slot, &client);
                }
            }
            message = next_message(&mut endpoint) => {
                match message {
                    Some(message) => {
                        let session_id = voice_proto::decode::<SessionEndpoint>(&message.payload)
                            .map(|endpoint| endpoint.session_id)
                            .unwrap_or_default();
                        let _ = cmd.send(Cmd::Endpoint { session_id });
                    }
                    None => {
                        client = None;
                        endpoint = None;
                        cancel = None;
                        remember(&slot, &client);
                    }
                }
            }
            message = next_message(&mut cancel) => {
                match message {
                    Some(message) => {
                        let session_id = voice_proto::decode::<SessionCancel>(&message.payload)
                            .map(|cancel| cancel.session_id)
                            .unwrap_or_default();
                        let _ = cmd.send(Cmd::Cancel { session_id });
                    }
                    None => {
                        client = None;
                        endpoint = None;
                        cancel = None;
                        remember(&slot, &client);
                    }
                }
            }
        }
    }
}

fn remember(slot: &Mutex<Option<async_nats::Client>>, client: &Option<async_nats::Client>) {
    *slot.lock().expect("training client") = client.clone();
}

async fn upload_training(
    slot: Arc<Mutex<Option<async_nats::Client>>>,
    screen_id: String,
    mut training: tokio::sync::mpsc::Receiver<Vec<u8>>,
) {
    let mut dropped = 0_u64;
    while let Some(pcm) = training.recv().await {
        let client = slot.lock().expect("training client").clone();
        let Some(client) = client else {
            note_drop(&mut dropped, "nats is down");
            continue;
        };
        if let Err(error) = put_training(&client, &screen_id, pcm).await {
            note_drop(&mut dropped, &error);
        }
    }
}

fn note_drop(dropped: &mut u64, error: &str) {
    *dropped += 1;
    if *dropped == 1 || dropped.is_multiple_of(60) {
        tracing::warn!(dropped = *dropped, error, "dropped training audio");
    }
}

async fn put_training(
    client: &async_nats::Client,
    screen_id: &str,
    pcm: Vec<u8>,
) -> Result<(), String> {
    let request = lcars_api_proto::v1::PutTrainingAudioRequest {
        screen_id: screen_id.to_string(),
        pcm,
    };
    let reply = tokio::time::timeout(
        Duration::from_secs(2),
        client.request(
            lcars_api_proto::TRAINING_PUT,
            lcars_api_proto::encode(&request).into(),
        ),
    )
    .await
    .map_err(|_| "timed out".to_string())?
    .map_err(|error| error.to_string())?;
    if let Some(code) = reply
        .headers
        .as_ref()
        .and_then(|headers| headers.get(lcars_api_proto::ERROR_CODE))
    {
        let msg = reply
            .headers
            .as_ref()
            .and_then(|headers| headers.get(lcars_api_proto::ERROR))
            .map(|value| value.as_str())
            .unwrap_or("");
        return Err(format!("{}: {msg}", code.as_str()));
    }
    Ok(())
}

async fn connect(
    hub: &Hub,
    screen_id: &str,
    client: &mut Option<async_nats::Client>,
    endpoint: &mut Option<async_nats::Subscriber>,
    cancel: &mut Option<async_nats::Subscriber>,
) {
    let next = match hub.connect().await {
        Ok(next) => next,
        Err(error) => {
            tracing::warn!(%error, "nats connect failed");
            return;
        }
    };
    let end = match next.subscribe(endpoint_subject(screen_id)).await {
        Ok(end) => end,
        Err(error) => {
            tracing::warn!(%error, "endpoint subscribe failed");
            return;
        }
    };
    let stop = match next.subscribe(cancel_subject(screen_id)).await {
        Ok(stop) => stop,
        Err(error) => {
            tracing::warn!(%error, "cancel subscribe failed");
            return;
        }
    };
    tracing::info!(screen_id, "nats connected");
    *client = Some(next);
    *endpoint = Some(end);
    *cancel = Some(stop);
}

async fn publish(
    client: &async_nats::Client,
    screen_id: &str,
    item: Outbound,
) -> Result<(), String> {
    match item {
        Outbound::State(state) => {
            send(
                client,
                &state_subject(screen_id),
                protobuf_headers("VoiceState")?,
                voice_proto::encode(&state),
            )
            .await
        }
        Outbound::Start(start) => {
            send(
                client,
                &session_start_subject(screen_id),
                protobuf_headers("SessionStart")?,
                voice_proto::encode(&start),
            )
            .await
        }
        Outbound::Preroll { session_id, frames } => {
            for (seq, pcm) in frames.into_iter().enumerate() {
                send(
                    client,
                    &audio_subject(screen_id),
                    audio_header_map(&session_id, seq as u32, true)?,
                    pcm,
                )
                .await?;
            }
            Ok(())
        }
        Outbound::Frame(frame) => {
            send(
                client,
                &audio_subject(screen_id),
                audio_header_map(&frame.session_id, frame.seq, false)?,
                frame.pcm,
            )
            .await
        }
    }
}

async fn send(
    client: &async_nats::Client,
    subject: &str,
    headers: HeaderMap,
    payload: Vec<u8>,
) -> Result<(), String> {
    client
        .publish_with_headers(subject.to_string(), headers, payload.into())
        .await
        .map_err(|error| error.to_string())
}

fn protobuf_headers(message: &str) -> Result<HeaderMap, String> {
    let mut headers = HeaderMap::new();
    headers.insert("Content-Type", content_type(message));
    Ok(headers)
}

fn audio_header_map(session_id: &str, seq: u32, preroll: bool) -> Result<HeaderMap, String> {
    let mut headers = HeaderMap::new();
    for (key, value) in audio_headers(session_id, seq, preroll) {
        headers.insert(key, value);
    }
    Ok(headers)
}

async fn next_message(
    subscriber: &mut Option<async_nats::Subscriber>,
) -> Option<async_nats::Message> {
    match subscriber.as_mut() {
        Some(subscriber) => subscriber.next().await,
        None => std::future::pending().await,
    }
}

#[allow(dead_code)]
fn _types(start: SessionStart, state: VoiceState, frame: AudioFrame) {
    let _ = (start, state, frame);
}
