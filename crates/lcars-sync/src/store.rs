//! Entity store. KV revisions win, and in-flight taps stay local until they do.

use std::collections::HashMap;

use home_proto::v1::command::Action;
use home_proto::v1::command_result::Status;
use home_proto::v1::entity_state::State;
use home_proto::v1::{
    ActivateScene, ClimateState, Command, CommandResult, CoverState, EntityState, LightState,
    LockState, OnOffState, SceneState, SetBrightness, SetColorTemp, SetCoverPosition, SetHvacMode,
    SetLock, SetOnOff, SetTargetTemp,
};
use uuid::Uuid;

use crate::intent::UiIntent;

pub const COMMAND_RETRY_MS: u64 = 1_000;
pub const COMMAND_REVERT_MS: u64 = 2_000;
pub const PENDING_VISIBLE_MS: u64 = 150;
const LATENCY_CAP: usize = 64;

#[derive(Clone, Debug)]
struct Stored {
    state: Option<EntityState>,
    revision: u64,
}

struct Pending {
    command: Command,
    previous: EntityState,
    optimistic: EntityState,
    sent_ms: u64,
    attempts: u8,
    confirmed: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum KvApply {
    Applied,
    Stale,
}

#[derive(Debug, PartialEq, Eq)]
pub enum StoreError {
    Offline,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ReplyEffect {
    WaitingForState,
    Reverted {
        entity_id: String,
        message: String,
    },
    PinRequired {
        entity_id: String,
        command_id: String,
    },
    Ignored,
}

#[derive(Debug, PartialEq)]
pub enum TickAction {
    Retry(Command),
    Revert { entity_id: String, message: String },
}

pub struct StateStore {
    entities: HashMap<String, Stored>,
    pending: HashMap<String, Pending>,
    latencies_ms: Vec<u64>,
    online: bool,
}

impl StateStore {
    #[must_use]
    pub fn new() -> Self {
        Self {
            entities: HashMap::new(),
            pending: HashMap::new(),
            latencies_ms: Vec::new(),
            online: false,
        }
    }

    pub fn set_online(&mut self, online: bool) {
        self.online = online;
    }

    #[must_use]
    pub fn is_online(&self) -> bool {
        self.online
    }

    pub fn apply_kv(&mut self, revision: u64, state: EntityState) -> KvApply {
        let id = state.id.clone();
        if let Some(existing) = self.entities.get(&id)
            && revision <= existing.revision
        {
            return KvApply::Stale;
        }
        if let Some(pending) = self.pending.remove(&id) {
            self.record_latency(revision_latency(&pending, state.updated_unix_ms));
        }
        self.entities.insert(
            id,
            Stored {
                state: Some(state),
                revision,
            },
        );
        KvApply::Applied
    }

    pub fn apply_delete(&mut self, id: &str, revision: u64) -> KvApply {
        if let Some(existing) = self.entities.get(id)
            && revision <= existing.revision
        {
            return KvApply::Stale;
        }
        self.pending.remove(id);
        self.entities.insert(
            id.to_string(),
            Stored {
                state: None,
                revision,
            },
        );
        KvApply::Applied
    }

    #[must_use]
    pub fn displayed(&self, id: &str) -> Option<&EntityState> {
        if let Some(pending) = self.pending.get(id) {
            return Some(&pending.optimistic);
        }
        self.entities
            .get(id)
            .and_then(|stored| stored.state.as_ref())
    }

    #[must_use]
    pub fn in_flight(&self, id: &str) -> bool {
        self.pending.contains_key(id)
    }

    #[must_use]
    pub fn pending_visible(&self, id: &str, now_ms: u64) -> bool {
        self.pending
            .get(id)
            .is_some_and(|pending| now_ms.saturating_sub(pending.sent_ms) > PENDING_VISIBLE_MS)
    }

    #[must_use]
    pub fn latency_ms_p95(&self) -> f32 {
        percentile_95(&self.latencies_ms)
    }

    pub fn begin(
        &mut self,
        intent: &UiIntent,
        screen_id: &str,
        now_ms: u64,
    ) -> Result<Command, StoreError> {
        if !self.online {
            return Err(StoreError::Offline);
        }
        let Some(entity_id) = intent.entity_id() else {
            return Err(StoreError::Offline);
        };
        let entity_id = entity_id.to_string();
        let action = action_from_intent(intent);
        let previous = self.authoritative(&entity_id);
        let mut optimistic = self
            .displayed(&entity_id)
            .cloned()
            .unwrap_or_else(|| previous.clone());
        apply_action(&mut optimistic, &action, now_ms);
        optimistic.id = entity_id.clone();
        let command = Command {
            command_id: Uuid::now_v7().to_string(),
            entity_id: entity_id.clone(),
            screen_id: screen_id.to_string(),
            pin: pin_from_intent(intent),
            action: Some(action),
        };
        self.pending.insert(
            entity_id,
            Pending {
                command: command.clone(),
                previous,
                optimistic,
                sent_ms: now_ms,
                attempts: 1,
                confirmed: false,
            },
        );
        Ok(command)
    }

    pub fn attach_pin(&mut self, command_id: &str, pin: String) -> Option<Command> {
        let pending = self
            .pending
            .values_mut()
            .find(|pending| pending.command.command_id == command_id)?;
        pending.command.pin = Some(pin);
        pending.confirmed = false;
        Some(pending.command.clone())
    }

    pub fn observe_reply(&mut self, result: &CommandResult) -> ReplyEffect {
        let Some(entity_id) = self
            .pending
            .iter()
            .find(|(_, pending)| pending.command.command_id == result.command_id)
            .map(|(id, _)| id.clone())
        else {
            return ReplyEffect::Ignored;
        };
        let status = Status::try_from(result.status).unwrap_or(Status::Unspecified);
        match status {
            Status::Ok => {
                if let Some(pending) = self.pending.get_mut(&entity_id) {
                    pending.confirmed = true;
                }
                ReplyEffect::WaitingForState
            }
            Status::PinRequired | Status::PinInvalid => ReplyEffect::PinRequired {
                entity_id,
                command_id: result.command_id.clone(),
            },
            Status::Rejected
            | Status::DeviceUnavailable
            | Status::DeviceTimeout
            | Status::Unspecified => {
                self.pending.remove(&entity_id);
                let message = if result.message.is_empty() {
                    "REJECTED".to_string()
                } else {
                    result.message.clone()
                };
                ReplyEffect::Reverted { entity_id, message }
            }
        }
    }

    pub fn tick(&mut self, now_ms: u64) -> Vec<TickAction> {
        let mut retries = Vec::new();
        let mut reverts = Vec::new();
        for (id, pending) in &mut self.pending {
            if pending.confirmed {
                continue;
            }
            let age = now_ms.saturating_sub(pending.sent_ms);
            if pending.attempts == 1 && age >= COMMAND_RETRY_MS {
                pending.attempts = 2;
                retries.push(TickAction::Retry(pending.command.clone()));
            } else if pending.attempts >= 2 && age >= COMMAND_REVERT_MS {
                reverts.push(id.clone());
            }
        }
        reverts
            .into_iter()
            .map(|entity_id| {
                self.pending.remove(&entity_id);
                TickAction::Revert {
                    entity_id,
                    message: "NO RESPONSE".into(),
                }
            })
            .chain(retries)
            .collect()
    }

    fn authoritative(&self, id: &str) -> EntityState {
        if let Some(pending) = self.pending.get(id) {
            return pending.previous.clone();
        }
        if let Some(state) = self
            .entities
            .get(id)
            .and_then(|stored| stored.state.clone())
        {
            return state;
        }
        EntityState {
            id: id.to_string(),
            available: true,
            ..Default::default()
        }
    }

    fn record_latency(&mut self, sample_ms: u64) {
        if self.latencies_ms.len() == LATENCY_CAP {
            self.latencies_ms.remove(0);
        }
        self.latencies_ms.push(sample_ms);
    }
}

impl Default for StateStore {
    fn default() -> Self {
        Self::new()
    }
}

fn revision_latency(pending: &Pending, updated_unix_ms: i64) -> u64 {
    if updated_unix_ms <= 0 {
        return 0;
    }
    let updated = u64::try_from(updated_unix_ms).unwrap_or(0);
    updated.saturating_sub(pending.sent_ms)
}

fn pin_from_intent(intent: &UiIntent) -> Option<String> {
    match intent {
        UiIntent::SetLock { pin, .. } => pin.clone(),
        _ => None,
    }
}

fn action_from_intent(intent: &UiIntent) -> Action {
    match intent {
        UiIntent::SetOnOff { on, .. } => Action::SetOnOff(SetOnOff { on: *on }),
        UiIntent::SetBrightness { pct, .. } => Action::SetBrightness(SetBrightness { pct: *pct }),
        UiIntent::SetColorTemp { kelvin, .. } => {
            Action::SetColorTemp(SetColorTemp { kelvin: *kelvin })
        }
        UiIntent::SetTargetTemp { value, unit, .. } => Action::SetTargetTemp(SetTargetTemp {
            value: *value,
            unit: *unit,
        }),
        UiIntent::SetHvacMode { mode, .. } => Action::SetHvacMode(SetHvacMode { mode: *mode }),
        UiIntent::SetLock { locked, .. } => Action::SetLock(SetLock { locked: *locked }),
        UiIntent::SetCoverPosition { pct, .. } => {
            Action::SetCoverPosition(SetCoverPosition { pct: *pct })
        }
        UiIntent::ActivateScene { .. } | UiIntent::SubmitPin { .. } => {
            Action::ActivateScene(ActivateScene {})
        }
    }
}

fn apply_action(state: &mut EntityState, action: &Action, now_ms: u64) {
    state.updated_unix_ms = i64::try_from(now_ms).unwrap_or(i64::MAX);
    match action {
        Action::SetOnOff(value) => match &mut state.state {
            Some(State::Light(light)) => light.on = value.on,
            Some(State::OnOff(on_off)) => on_off.on = value.on,
            _ => state.state = Some(State::OnOff(OnOffState { on: value.on })),
        },
        Action::SetBrightness(value) => {
            let light = match &mut state.state {
                Some(State::Light(light)) => light,
                _ => {
                    state.state = Some(State::Light(LightState::default()));
                    match &mut state.state {
                        Some(State::Light(light)) => light,
                        _ => return,
                    }
                }
            };
            light.on = value.pct > 0;
            light.brightness_pct = value.pct;
        }
        Action::SetColorTemp(value) => {
            if !matches!(state.state, Some(State::Light(_))) {
                state.state = Some(State::Light(LightState::default()));
            }
            if let Some(State::Light(light)) = &mut state.state {
                light.color_temp_k = Some(value.kelvin);
            }
        }
        Action::SetTargetTemp(value) => {
            let climate = climate_mut(state);
            climate.target_temp = value.value;
            climate.unit = value.unit;
        }
        Action::SetHvacMode(value) => {
            climate_mut(state).mode = value.mode;
        }
        Action::SetLock(value) => {
            state.state = Some(State::Lock(LockState {
                state: if value.locked {
                    home_proto::v1::lock_state::State::Locked as i32
                } else {
                    home_proto::v1::lock_state::State::Unlocked as i32
                },
            }));
        }
        Action::SetCoverPosition(value) => {
            state.state = Some(State::Cover(CoverState {
                position_pct: value.pct,
            }));
        }
        Action::ActivateScene(_) => {
            state.state = Some(State::Scene(SceneState {
                last_activated_unix_ms: state.updated_unix_ms,
            }));
        }
    }
}

fn climate_mut(state: &mut EntityState) -> &mut ClimateState {
    if !matches!(state.state, Some(State::Climate(_))) {
        state.state = Some(State::Climate(ClimateState::default()));
    }
    match &mut state.state {
        Some(State::Climate(climate)) => climate,
        _ => unreachable!("climate just inserted"),
    }
}

fn percentile_95(samples: &[u64]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let index = ((sorted.len() - 1) as f32 * 0.95).round() as usize;
    sorted[index] as f32
}

#[cfg(test)]
mod tests {
    use super::{KvApply, ReplyEffect, StateStore, StoreError, TickAction};
    use crate::intent::UiIntent;
    use home_proto::v1::command_result::Status;
    use home_proto::v1::entity_state::State;
    use home_proto::v1::{CommandResult, EntityState, LightState};

    fn light(on: bool, brightness: u32, revision_ms: i64) -> EntityState {
        EntityState {
            id: "light.kitchen".into(),
            name: "Kitchen".into(),
            available: true,
            updated_unix_ms: revision_ms,
            state: Some(State::Light(LightState {
                on,
                brightness_pct: brightness,
                ..Default::default()
            })),
            ..Default::default()
        }
    }

    #[test]
    fn higher_revision_wins_and_replays_are_stale() {
        let mut store = StateStore::new();
        assert_eq!(store.apply_kv(2, light(true, 10, 2)), KvApply::Applied);
        assert_eq!(store.apply_kv(2, light(false, 0, 2)), KvApply::Stale);
        assert_eq!(store.apply_kv(1, light(false, 0, 1)), KvApply::Stale);
        assert_eq!(store.apply_kv(3, light(false, 0, 3)), KvApply::Applied);
        match &store.displayed("light.kitchen").unwrap().state {
            Some(State::Light(state)) => assert!(!state.on),
            _ => panic!("expected light"),
        }
    }

    #[test]
    fn optimistic_value_clears_when_kv_arrives() {
        let mut store = StateStore::new();
        store.set_online(true);
        store.apply_kv(1, light(false, 0, 1));
        let command = store
            .begin(
                &UiIntent::SetOnOff {
                    entity_id: "light.kitchen".into(),
                    on: true,
                },
                "kitchen",
                1_000,
            )
            .unwrap();
        match &store.displayed("light.kitchen").unwrap().state {
            Some(State::Light(state)) => assert!(state.on),
            _ => panic!("expected optimistic light"),
        }
        assert!(!store.pending_visible("light.kitchen", 1_100));
        assert!(store.pending_visible("light.kitchen", 1_151));
        assert_eq!(
            store.observe_reply(&CommandResult {
                command_id: command.command_id.clone(),
                status: Status::Ok as i32,
                message: String::new(),
            }),
            ReplyEffect::WaitingForState
        );
        assert!(store.in_flight("light.kitchen"));
        assert_eq!(store.apply_kv(4, light(true, 80, 1_040)), KvApply::Applied);
        assert!(!store.in_flight("light.kitchen"));
        assert!((store.latency_ms_p95() - 40.0).abs() < f32::EPSILON);
    }

    #[test]
    fn rejected_reply_reverts_and_offline_drops() {
        let mut store = StateStore::new();
        store.set_online(true);
        store.apply_kv(1, light(true, 50, 1));
        let command = store
            .begin(
                &UiIntent::SetOnOff {
                    entity_id: "light.kitchen".into(),
                    on: false,
                },
                "kitchen",
                500,
            )
            .unwrap();
        let effect = store.observe_reply(&CommandResult {
            command_id: command.command_id,
            status: Status::DeviceUnavailable as i32,
            message: "UNAVAILABLE".into(),
        });
        assert_eq!(
            effect,
            ReplyEffect::Reverted {
                entity_id: "light.kitchen".into(),
                message: "UNAVAILABLE".into(),
            }
        );
        match &store.displayed("light.kitchen").unwrap().state {
            Some(State::Light(state)) => assert!(state.on),
            _ => panic!("expected revert"),
        }
        store.set_online(false);
        let dropped = store.begin(
            &UiIntent::SetOnOff {
                entity_id: "light.kitchen".into(),
                on: false,
            },
            "kitchen",
            800,
        );
        assert_eq!(dropped, Err(StoreError::Offline));
    }

    #[test]
    fn pin_keeps_the_command_id_and_timeout_retries_once() {
        let mut store = StateStore::new();
        store.set_online(true);
        let command = store
            .begin(
                &UiIntent::SetLock {
                    entity_id: "lock.front".into(),
                    locked: false,
                    pin: None,
                },
                "kitchen",
                0,
            )
            .unwrap();
        let effect = store.observe_reply(&CommandResult {
            command_id: command.command_id.clone(),
            status: Status::PinRequired as i32,
            message: String::new(),
        });
        assert!(matches!(effect, ReplyEffect::PinRequired { .. }));
        let resent = store
            .attach_pin(&command.command_id, "1234".into())
            .unwrap();
        assert_eq!(resent.command_id, command.command_id);
        assert_eq!(resent.pin.as_deref(), Some("1234"));

        let mut store = StateStore::new();
        store.set_online(true);
        let command = store
            .begin(
                &UiIntent::SetOnOff {
                    entity_id: "light.kitchen".into(),
                    on: true,
                },
                "kitchen",
                0,
            )
            .unwrap();
        let tick = store.tick(1_000);
        assert!(matches!(tick[0], TickAction::Retry(_)));
        match &tick[0] {
            TickAction::Retry(retry) => assert_eq!(retry.command_id, command.command_id),
            _ => unreachable!(),
        }
        let tick = store.tick(2_000);
        assert_eq!(
            tick,
            vec![TickAction::Revert {
                entity_id: "light.kitchen".into(),
                message: "NO RESPONSE".into(),
            }]
        );
        assert!(!store.in_flight("light.kitchen"));
    }

    #[test]
    fn delete_tombstone_blocks_an_older_put() {
        let mut store = StateStore::new();
        store.apply_kv(1, light(true, 1, 1));
        assert_eq!(store.apply_delete("light.kitchen", 2), KvApply::Applied);
        assert!(store.displayed("light.kitchen").is_none());
        assert_eq!(store.apply_kv(2, light(true, 1, 2)), KvApply::Stale);
        assert_eq!(store.apply_kv(3, light(false, 0, 3)), KvApply::Applied);
    }
}
