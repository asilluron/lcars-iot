//! Dev backend: seeds KV, answers `cmd.>`, and writes `sys.backend`.
//! The dev PIN for unlocking `lock.front` is `1234`.

use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures::StreamExt;
use home_proto::v1::command::Action;
use home_proto::v1::command_result::Status;
use home_proto::v1::entity_state::State;
use home_proto::v1::lock_state::State as LockPosition;
use home_proto::v1::{
    BackendHeartbeat, ClimateState, Command, CommandResult, CoverState, EntityState, HvacAction,
    HvacMode, LightState, LockState, SceneState, SetBrightness, TempUnit,
};
use home_proto::{decode, encode};
use lcars_sync::subjects::{BACKEND_KEY, KV_CONFIG, KV_STATE, STREAM_EVENTS, layout_key};
use lcars_sync::{baked_layout, validate_layout};
use tracing::info;

const DEV_PIN: &str = "1234";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let nats_url =
        std::env::var("NATS_URL").unwrap_or_else(|_| "nats://127.0.0.1:4222".to_string());
    let screen_ids = ["dev", "kitchen"];
    info!(%nats_url, dev_pin = DEV_PIN, "fake backend starting");

    let client = async_nats::connect(nats_url).await?;
    let context = async_nats::jetstream::new(client.clone());
    let state = ensure_kv(&context, KV_STATE, 1, 64 * 1024).await?;
    let config = ensure_kv(&context, KV_CONFIG, 5, 256 * 1024).await?;
    ensure_events(&context).await?;

    let layout = baked_layout();
    validate_layout(&layout)?;
    let encoded_layout = encode(&layout);
    for screen_id in screen_ids {
        config
            .put(layout_key(screen_id), encoded_layout.clone().into())
            .await?;
    }

    let mut entities = seed_entities();
    for entity in entities.values() {
        state.put(&entity.id, encode(entity).into()).await?;
    }
    publish_backend(&state).await?;

    let mut commands = client.subscribe("cmd.>").await?;
    let mut heartbeats = tokio::time::interval(Duration::from_secs(5));
    info!("fake backend ready");

    loop {
        tokio::select! {
            _ = heartbeats.tick() => {
                publish_backend(&state).await?;
            }
            maybe = commands.next() => {
                let Some(message) = maybe else { break };
                if let Some(reply) = message.reply.clone() {
                    let response = handle_command(&mut entities, &state, &message.payload).await?;
                    client.publish(reply, encode(&response).into()).await?;
                }
            }
        }
    }
    Ok(())
}

async fn ensure_kv(
    context: &async_nats::jetstream::Context,
    bucket: &str,
    history: i64,
    max_value_size: i32,
) -> Result<async_nats::jetstream::kv::Store, Box<dyn std::error::Error>> {
    if let Ok(store) = context.get_key_value(bucket).await {
        return Ok(store);
    }
    let config = async_nats::jetstream::kv::Config {
        bucket: bucket.to_string(),
        history,
        max_value_size,
        storage: async_nats::jetstream::stream::StorageType::File,
        num_replicas: 1,
        ..Default::default()
    };
    Ok(context.create_key_value(config).await?)
}

async fn ensure_events(
    context: &async_nats::jetstream::Context,
) -> Result<(), Box<dyn std::error::Error>> {
    if context.get_stream(STREAM_EVENTS).await.is_ok() {
        return Ok(());
    }
    let config = async_nats::jetstream::stream::Config {
        name: STREAM_EVENTS.to_string(),
        subjects: vec!["evt.>".into()],
        max_messages: 100_000,
        max_age: Duration::from_secs(24 * 60 * 60),
        storage: async_nats::jetstream::stream::StorageType::File,
        num_replicas: 1,
        ..Default::default()
    };
    context.create_stream(config).await?;
    Ok(())
}

async fn publish_backend(
    state: &async_nats::jetstream::kv::Store,
) -> Result<(), Box<dyn std::error::Error>> {
    let beat = BackendHeartbeat {
        version: "fake-0.1.0".into(),
        unix_ms: unix_ms() as i64,
    };
    state.put(BACKEND_KEY, encode(&beat).into()).await?;
    Ok(())
}

fn seed_entities() -> HashMap<String, EntityState> {
    let mut entities = HashMap::new();
    let kitchen = EntityState {
        id: "light.kitchen".into(),
        name: "Kitchen".into(),
        area: "kitchen".into(),
        available: true,
        updated_unix_ms: unix_ms() as i64,
        state: Some(State::Light(LightState {
            on: true,
            brightness_pct: 60,
            ..Default::default()
        })),
    };
    let dining = EntityState {
        id: "light.dining".into(),
        name: "Dining".into(),
        area: "dining".into(),
        available: true,
        updated_unix_ms: unix_ms() as i64,
        state: Some(State::Light(LightState {
            on: false,
            brightness_pct: 0,
            ..Default::default()
        })),
    };
    let climate = EntityState {
        id: "climate.downstairs".into(),
        name: "Downstairs".into(),
        area: "downstairs".into(),
        available: true,
        updated_unix_ms: unix_ms() as i64,
        state: Some(State::Climate(ClimateState {
            current_temp: 72.0,
            target_temp: 70.0,
            unit: TempUnit::Fahrenheit as i32,
            mode: HvacMode::Heat as i32,
            action: HvacAction::Heating as i32,
        })),
    };
    let lock = EntityState {
        id: "lock.front".into(),
        name: "Front Door".into(),
        area: "entry".into(),
        available: true,
        updated_unix_ms: unix_ms() as i64,
        state: Some(State::Lock(LockState {
            state: LockPosition::Locked as i32,
        })),
    };
    let scene = EntityState {
        id: "scene.evening".into(),
        name: "Evening".into(),
        area: "house".into(),
        available: true,
        updated_unix_ms: unix_ms() as i64,
        state: Some(State::Scene(SceneState::default())),
    };
    for entity in [kitchen, dining, climate, lock, scene] {
        entities.insert(entity.id.clone(), entity);
    }
    entities
}

async fn handle_command(
    entities: &mut HashMap<String, EntityState>,
    store: &async_nats::jetstream::kv::Store,
    payload: &[u8],
) -> Result<CommandResult, Box<dyn std::error::Error>> {
    let command = decode::<Command>(payload)?;
    let unlocking = matches!(command.action, Some(Action::SetLock(lock)) if !lock.locked);
    if command.entity_id.starts_with("lock.") && unlocking {
        match command.pin.as_deref() {
            None => {
                return Ok(result(&command, Status::PinRequired, "PIN REQUIRED"));
            }
            Some(pin) if pin != DEV_PIN => {
                return Ok(result(&command, Status::PinInvalid, "PIN INVALID"));
            }
            Some(_) => {}
        }
    }
    let entity = entities
        .entry(command.entity_id.clone())
        .or_insert_with(|| EntityState {
            id: command.entity_id.clone(),
            name: command.entity_id.clone(),
            available: true,
            ..Default::default()
        });
    if let Some(action) = &command.action {
        apply(entity, action);
    }
    entity.updated_unix_ms = unix_ms() as i64;
    store.put(&entity.id, encode(entity).into()).await?;
    Ok(result(&command, Status::Ok, ""))
}

fn apply(entity: &mut EntityState, action: &Action) {
    match action {
        Action::SetOnOff(value) => match &mut entity.state {
            Some(State::Light(light)) => light.on = value.on,
            _ => entity.state = Some(State::OnOff(home_proto::v1::OnOffState { on: value.on })),
        },
        Action::SetBrightness(SetBrightness { pct }) => {
            entity.state = Some(State::Light(LightState {
                on: *pct > 0,
                brightness_pct: *pct,
                ..light_keep(entity)
            }));
        }
        Action::SetColorTemp(value) => {
            let mut light = light_keep(entity);
            light.color_temp_k = Some(value.kelvin);
            entity.state = Some(State::Light(light));
        }
        Action::SetTargetTemp(value) => {
            let mut climate = climate_keep(entity);
            climate.target_temp = value.value;
            climate.unit = value.unit;
            entity.state = Some(State::Climate(climate));
        }
        Action::SetHvacMode(value) => {
            let mut climate = climate_keep(entity);
            climate.mode = value.mode;
            entity.state = Some(State::Climate(climate));
        }
        Action::SetLock(value) => {
            entity.state = Some(State::Lock(LockState {
                state: if value.locked {
                    LockPosition::Locked as i32
                } else {
                    LockPosition::Unlocked as i32
                },
            }));
        }
        Action::SetCoverPosition(value) => {
            entity.state = Some(State::Cover(CoverState {
                position_pct: value.pct,
            }));
        }
        Action::ActivateScene(_) => {
            entity.state = Some(State::Scene(SceneState {
                last_activated_unix_ms: unix_ms() as i64,
            }));
        }
    }
}

fn light_keep(entity: &EntityState) -> LightState {
    match &entity.state {
        Some(State::Light(light)) => *light,
        _ => LightState::default(),
    }
}

fn climate_keep(entity: &EntityState) -> ClimateState {
    match &entity.state {
        Some(State::Climate(climate)) => *climate,
        _ => ClimateState {
            unit: TempUnit::Fahrenheit as i32,
            ..Default::default()
        },
    }
}

fn result(command: &Command, status: Status, message: &str) -> CommandResult {
    CommandResult {
        command_id: command.command_id.clone(),
        status: status as i32,
        message: message.to_string(),
    }
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}
