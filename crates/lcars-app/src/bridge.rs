//! UI-thread state. Optimistic taps paint here; sync corrections arrive later.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Instant;

use chrono::Timelike;
use home_proto::v1::ScreenLayout;
use lcars_sync::{StateStore, TileModel, UiIntent, UiUpdate, project_layout};
use lcars_ui::slint::{Color, ComponentHandle, ModelRc, VecModel};
use lcars_ui::{AppWindow, PanelItem, Theme, Tile};

use crate::audio::{self, playback_volume};
use crate::config::BootstrapConfig;
use crate::power::{self, DisplayPower, PowerPhase, PowerPolicy, TouchEffect};

#[derive(Clone, Copy, PartialEq, Eq)]
enum HoldPhase {
    Waiting,
    InFlight,
}

struct Hold {
    phase: HoldPhase,
    at: Instant,
}

pub struct UiState {
    layout: ScreenLayout,
    tiles: Vec<TileModel>,
    holds: HashMap<String, Hold>,
    active: usize,
    controls: bool,
    pin_command: Option<String>,
    panels: Rc<VecModel<PanelItem>>,
    tile_model: Rc<VecModel<Tile>>,
    power: DisplayPower,
    audio_enabled: bool,
    audio_volume: u8,
    night_start: u16,
    night_end: u16,
    intent_tx: Option<tokio::sync::mpsc::Sender<UiIntent>>,
    updates: Option<std::sync::mpsc::Receiver<UiUpdate>>,
}

impl UiState {
    pub fn new(app: &AppWindow, config: &BootstrapConfig) -> Rc<RefCell<Self>> {
        let layout = lcars_sync::baked_layout();
        let tiles = project_layout(&layout, &StateStore::new(), 0);
        let panels = Rc::new(VecModel::from(panel_items(&layout)));
        let tile_model = Rc::new(VecModel::from(Vec::<Tile>::new()));
        app.set_panels(ModelRc::from(panels.clone()));
        app.set_tiles(ModelRc::from(tile_model.clone()));
        app.set_version_text(env!("CARGO_PKG_VERSION").into());
        app.global::<Theme<'_>>().set_scale(config.ui_scale);
        let state = Self {
            layout,
            tiles,
            holds: HashMap::new(),
            active: 0,
            controls: false,
            pin_command: None,
            panels,
            tile_model,
            power: DisplayPower::new(
                PowerPolicy {
                    brightness_pct: config.brightness_pct,
                    idle_dim_s: config.idle_dim_s,
                    idle_off_s: config.idle_off_s,
                    night_cap_pct: 30,
                },
                Instant::now(),
            ),
            audio_enabled: config.audio_enabled,
            audio_volume: config.audio_volume,
            night_start: config.night_start_min,
            night_end: config.night_end_min,
            intent_tx: None,
            updates: None,
        };
        let state = Rc::new(RefCell::new(state));
        state.borrow_mut().select_home(app);
        state.borrow().redraw(app);
        state
    }

    pub fn set_sender(&mut self, sender: tokio::sync::mpsc::Sender<UiIntent>) {
        self.intent_tx = Some(sender);
    }

    pub fn set_updates(&mut self, updates: std::sync::mpsc::Receiver<UiUpdate>) {
        self.updates = Some(updates);
    }

    pub fn drain(&mut self, app: &AppWindow) {
        let batch: Vec<UiUpdate> = {
            let Some(updates) = self.updates.as_ref() else {
                return;
            };
            std::iter::from_fn(|| updates.try_recv().ok()).collect()
        };
        for update in batch {
            self.apply(app, update);
        }
    }

    pub fn apply(&mut self, app: &AppWindow, update: UiUpdate) {
        match update {
            UiUpdate::Link {
                label,
                controls_enabled,
                values_dimmed,
            } => {
                app.set_status(label.into());
                app.set_controls_enabled(controls_enabled);
                app.set_values_dimmed(values_dimmed);
                self.controls = controls_enabled;
                if !controls_enabled {
                    self.holds.clear();
                }
            }
            UiUpdate::Layout(layout) => {
                self.layout = layout;
                self.panels.set_vec(panel_items(&self.layout));
                self.select_home(app);
                self.redraw(app);
            }
            UiUpdate::Tiles(tiles) => {
                self.merge_tiles(tiles);
                self.redraw(app);
            }
            UiUpdate::CommandFailed { message, .. } => {
                app.set_error_text(message.into());
                self.cue("error");
            }
            UiUpdate::PinRequired { command_id, .. } => {
                self.pin_command = Some(command_id);
                app.set_pin_visible(true);
            }
            UiUpdate::Alert {
                title,
                body,
                critical,
            } => {
                app.set_alert_title(title.into());
                app.set_alert_body(body.into());
                app.set_alert_visible(true);
                if critical {
                    self.power.critical(Instant::now());
                    app.set_screen_off(false);
                    app.set_alert_bright(true);
                }
            }
            UiUpdate::Error(detail) => app.set_error_text(detail.into()),
        }
    }

    pub fn toggle(&mut self, app: &AppWindow, entity_id: String, on: bool) {
        if !self.accept_touch() {
            return;
        }
        let kind = self
            .tiles
            .iter()
            .find(|tile| tile.entity_id == entity_id)
            .map(|tile| tile.kind.clone())
            .unwrap_or_else(|| "on-off".into());
        self.hold(&entity_id);
        if let Some(tile) = self
            .tiles
            .iter_mut()
            .find(|tile| tile.entity_id == entity_id)
        {
            tile.on = on;
            tile.in_flight = true;
            tile.value = match kind.as_str() {
                "light" if on => format!("ON {}", tile.level.max(1)),
                "light" => "OFF".into(),
                "lock" if on => "LOCKED".into(),
                "lock" => "UNLOCKED".into(),
                _ if on => "ON".into(),
                _ => "OFF".into(),
            };
        }
        self.redraw(app);
        self.cue("tap");
        let intent = if kind == "lock" {
            UiIntent::SetLock {
                entity_id,
                locked: on,
                pin: None,
            }
        } else {
            UiIntent::SetOnOff { entity_id, on }
        };
        self.send(intent);
    }

    pub fn activate_scene(&mut self, app: &AppWindow, entity_id: String) {
        if !self.accept_touch() {
            return;
        }
        self.hold(&entity_id);
        self.redraw(app);
        self.cue("tap");
        self.send(UiIntent::ActivateScene { entity_id });
    }

    pub fn slide(&mut self, app: &AppWindow, entity_id: String, level: i32, release: bool) {
        if !self.accept_touch() {
            return;
        }
        let kind = self
            .tiles
            .iter()
            .find(|tile| tile.entity_id == entity_id)
            .map(|tile| tile.kind.clone())
            .unwrap_or_else(|| "light".into());
        self.hold(&entity_id);
        if let Some(tile) = self
            .tiles
            .iter_mut()
            .find(|tile| tile.entity_id == entity_id)
        {
            tile.level = level;
            tile.on = level > 0;
            tile.in_flight = true;
            tile.value = if kind == "cover" {
                format!("{level}%")
            } else if level > 0 {
                format!("ON {level}")
            } else {
                "OFF".into()
            };
        }
        self.redraw(app);
        let pct = u32::try_from(level.max(0)).unwrap_or(0);
        let intent = if kind == "cover" {
            UiIntent::SetCoverPosition {
                entity_id,
                pct,
                release,
            }
        } else {
            UiIntent::SetBrightness {
                entity_id,
                pct,
                release,
            }
        };
        self.send(intent);
    }

    pub fn submit_pin(&mut self, app: &AppWindow, pin: String) {
        app.set_pin_visible(false);
        if let Some(command_id) = self.pin_command.take() {
            self.send(UiIntent::SubmitPin { command_id, pin });
        }
    }

    pub fn wake(&mut self, app: &AppWindow) {
        let _ = self.power.touch(Instant::now());
        app.set_screen_off(false);
        self.apply_brightness();
    }

    pub fn tick_power(&mut self, app: &AppWindow) {
        let phase = self.power.tick(Instant::now());
        app.set_screen_off(phase == PowerPhase::Off);
        self.apply_brightness();
        self.holds.retain(|_, hold| {
            !(hold.phase == HoldPhase::Waiting && hold.at.elapsed().as_secs() >= 3)
        });
    }

    fn accept_touch(&mut self) -> bool {
        if !self.controls {
            return false;
        }
        self.power.touch(Instant::now()) == TouchEffect::Deliver
    }

    fn hold(&mut self, entity_id: &str) {
        self.holds.insert(
            entity_id.to_string(),
            Hold {
                phase: HoldPhase::Waiting,
                at: Instant::now(),
            },
        );
    }

    fn merge_tiles(&mut self, incoming: Vec<TileModel>) {
        for tile in incoming {
            let decision = match self.holds.get(&tile.entity_id).map(|hold| hold.phase) {
                Some(HoldPhase::Waiting) if !tile.in_flight => Merge::Ignore,
                Some(HoldPhase::Waiting) => Merge::KeepLocal,
                Some(HoldPhase::InFlight) if tile.in_flight => Merge::KeepLocal,
                Some(_) => Merge::Replace,
                None => Merge::Replace,
            };
            if decision == Merge::KeepLocal
                && let Some(hold) = self.holds.get_mut(&tile.entity_id)
            {
                hold.phase = HoldPhase::InFlight;
            }
            if decision == Merge::Replace {
                self.holds.remove(&tile.entity_id);
                upsert(&mut self.tiles, tile);
            }
        }
    }

    fn select_home(&mut self, app: &AppWindow) {
        self.active = self
            .layout
            .panels
            .iter()
            .position(|panel| panel.id == self.layout.home_panel_id)
            .unwrap_or(0);
        app.set_active_panel(i32::try_from(self.active).unwrap_or(0));
        if let Some(panel) = self.layout.panels.get(self.active) {
            app.set_panel_title(panel.title.clone().into());
        }
    }

    pub fn select_panel(&mut self, app: &AppWindow, index: i32) {
        if index < 0 {
            return;
        }
        let index = index as usize;
        if self.layout.panels.get(index).is_none() {
            return;
        }
        self.active = index;
        app.set_active_panel(index as i32);
        if let Some(panel) = self.layout.panels.get(index) {
            app.set_panel_title(panel.title.clone().into());
        }
        self.redraw(app);
    }

    fn redraw(&self, app: &AppWindow) {
        let Some(panel) = self.layout.panels.get(self.active) else {
            self.tile_model.set_vec(Vec::new());
            return;
        };
        let visible: Vec<Tile> = self
            .tiles
            .iter()
            .filter(|tile| panel.entity_ids.iter().any(|id| id == &tile.entity_id))
            .map(to_slint)
            .collect();
        self.tile_model.set_vec(visible);
        let _ = app;
    }

    fn send(&self, intent: UiIntent) {
        if let Some(tx) = &self.intent_tx
            && let Err(error) = tx.try_send(intent)
        {
            tracing::debug!(%error, "intent queue");
        }
    }

    fn cue(&self, name: &str) {
        let quiet = power::in_night(minute_now(), self.night_start, self.night_end);
        audio::cue(
            name,
            playback_volume(self.audio_enabled, self.audio_volume, quiet),
        );
    }

    fn apply_brightness(&self) {
        let quiet = power::in_night(minute_now(), self.night_start, self.night_end);
        power::write_brightness(self.power.brightness(quiet));
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Merge {
    Ignore,
    KeepLocal,
    Replace,
}

fn upsert(tiles: &mut Vec<TileModel>, tile: TileModel) {
    if let Some(existing) = tiles
        .iter_mut()
        .find(|item| item.entity_id == tile.entity_id)
    {
        *existing = tile;
    } else {
        tiles.push(tile);
    }
}

fn panel_items(layout: &ScreenLayout) -> Vec<PanelItem> {
    layout
        .panels
        .iter()
        .map(|panel| PanelItem {
            id: panel.id.clone().into(),
            title: panel.title.clone().into(),
            accent: token_color(&panel.accent),
        })
        .collect()
}

fn to_slint(tile: &TileModel) -> Tile {
    Tile {
        entity_id: tile.entity_id.clone().into(),
        name: tile.name.clone().into(),
        value: tile.value.clone().into(),
        kind: tile.kind.clone().into(),
        level: tile.level,
        on: tile.on,
        available: tile.available,
        in_flight: tile.in_flight,
        pending_visible: tile.pending_visible,
    }
}

fn token_color(token: &str) -> Color {
    let (red, green, blue) = match token {
        "accent-2" => (0xCC, 0x99, 0xCC),
        "accent-3" => (0x99, 0x99, 0xFF),
        "accent-4" => (0xFF, 0xCC, 0x99),
        "alert" => (0xCC, 0x44, 0x44),
        "bg" | "text-on-accent" => (0x00, 0x00, 0x00),
        _ => (0xFF, 0x99, 0x66),
    };
    Color::from_rgb_u8(red, green, blue)
}

fn minute_now() -> u16 {
    let now = chrono::Local::now();
    (now.hour() as u16) * 60 + now.minute() as u16
}
