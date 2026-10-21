//! Project store rows into the strings the panel draws.

use home_proto::v1::entity_state::State;
use home_proto::v1::lock_state::State as LockPosition;
use home_proto::v1::{HvacMode, PanelKind, ScreenLayout, TempUnit};

use crate::store::StateStore;

#[derive(Clone, Debug, PartialEq)]
pub struct TileModel {
    pub entity_id: String,
    pub name: String,
    pub value: String,
    pub kind: String,
    pub level: i32,
    pub on: bool,
    pub available: bool,
    pub in_flight: bool,
    pub pending_visible: bool,
}

#[must_use]
pub fn project_layout(layout: &ScreenLayout, store: &StateStore, now_ms: u64) -> Vec<TileModel> {
    let mut tiles = Vec::new();
    for panel in &layout.panels {
        for entity_id in &panel.entity_ids {
            tiles.push(project_entity(entity_id, panel.kind, store, now_ms));
        }
    }
    tiles
}

fn project_entity(entity_id: &str, panel_kind: i32, store: &StateStore, now_ms: u64) -> TileModel {
    let in_flight = store.in_flight(entity_id);
    let pending_visible = store.pending_visible(entity_id, now_ms);
    let Some(state) = store.displayed(entity_id) else {
        return TileModel {
            entity_id: entity_id.to_string(),
            name: entity_id.to_string(),
            value: "—".into(),
            kind: kind_name(panel_kind),
            level: -1,
            on: false,
            available: false,
            in_flight,
            pending_visible,
        };
    };
    let (value, kind, level, on) = match &state.state {
        Some(State::Light(light)) => (
            if light.on {
                format!("ON {}", light.brightness_pct)
            } else {
                "OFF".into()
            },
            "light",
            i32::try_from(light.brightness_pct).unwrap_or(0),
            light.on,
        ),
        Some(State::OnOff(on_off)) => (
            if on_off.on { "ON" } else { "OFF" }.into(),
            "on-off",
            if on_off.on { 100 } else { 0 },
            on_off.on,
        ),
        Some(State::Climate(climate)) => (
            format!(
                "{:.0}{}  {:.0}{}",
                climate.current_temp,
                unit_label(climate.unit),
                climate.target_temp,
                unit_label(climate.unit)
            ),
            "climate",
            -1,
            climate.mode != HvacMode::Off as i32 && climate.mode != HvacMode::Unspecified as i32,
        ),
        Some(State::Sensor(sensor)) => (
            format!("{} {}", trim_number(sensor.value), sensor.unit),
            "sensor",
            -1,
            false,
        ),
        Some(State::Lock(lock)) => {
            let (value, on) =
                match LockPosition::try_from(lock.state).unwrap_or(LockPosition::Unspecified) {
                    LockPosition::Locked => ("LOCKED", true),
                    LockPosition::Unlocked => ("UNLOCKED", false),
                    LockPosition::Jammed => ("JAMMED", false),
                    LockPosition::Unspecified => ("UNSUPPORTED", false),
                };
            (value.into(), "lock", -1, on)
        }
        Some(State::Cover(cover)) => (
            format!("{}%", cover.position_pct),
            "cover",
            i32::try_from(cover.position_pct).unwrap_or(0),
            cover.position_pct > 0,
        ),
        Some(State::Scene(_)) => ("READY".into(), "scene", -1, false),
        None => ("UNSUPPORTED".into(), "unsupported", -1, false),
    };
    TileModel {
        entity_id: entity_id.to_string(),
        name: if state.name.is_empty() {
            entity_id.to_string()
        } else {
            state.name.clone()
        },
        value,
        kind: kind.to_string(),
        level,
        on,
        available: state.available,
        in_flight,
        pending_visible,
    }
}

fn kind_name(panel_kind: i32) -> String {
    match PanelKind::try_from(panel_kind) {
        Ok(PanelKind::Lights) => "light",
        Ok(PanelKind::Climate) => "climate",
        Ok(PanelKind::Security) => "lock",
        Ok(PanelKind::Sensors) => "sensor",
        Ok(PanelKind::Scenes) => "scene",
        Ok(PanelKind::Alerts) => "alert",
        _ => "unsupported",
    }
    .into()
}

fn unit_label(unit: i32) -> &'static str {
    match TempUnit::try_from(unit) {
        Ok(TempUnit::Celsius) => "C",
        Ok(TempUnit::Fahrenheit) => "F",
        _ => "",
    }
}

fn trim_number(value: f64) -> String {
    if (value - value.round()).abs() < 0.05 {
        format!("{:.0}", value.round())
    } else {
        format!("{value:.1}")
    }
}

#[cfg(test)]
mod tests {
    use super::project_layout;
    use crate::layout::baked_layout;
    use crate::store::StateStore;
    use home_proto::v1::entity_state::State;
    use home_proto::v1::{EntityState, LightState};

    #[test]
    fn missing_entity_is_a_placeholder_and_unknown_state_is_unsupported() {
        let layout = baked_layout();
        let mut store = StateStore::new();
        let tiles = project_layout(&layout, &store, 0);
        assert!(tiles.iter().any(|tile| tile.value == "—"));

        store.apply_kv(
            1,
            EntityState {
                id: "light.kitchen".into(),
                name: "Kitchen".into(),
                available: true,
                state: None,
                ..Default::default()
            },
        );
        store.apply_kv(
            2,
            EntityState {
                id: "light.dining".into(),
                name: "Dining".into(),
                available: true,
                state: Some(State::Light(LightState {
                    on: true,
                    brightness_pct: 40,
                    ..Default::default()
                })),
                ..Default::default()
            },
        );
        let tiles = project_layout(&layout, &store, 0);
        let kitchen = tiles
            .iter()
            .find(|tile| tile.entity_id == "light.kitchen")
            .unwrap();
        assert_eq!(kitchen.value, "UNSUPPORTED");
        let dining = tiles
            .iter()
            .find(|tile| tile.entity_id == "light.dining")
            .unwrap();
        assert_eq!(dining.value, "ON 40");
        assert_eq!(dining.kind, "light");
    }
}
