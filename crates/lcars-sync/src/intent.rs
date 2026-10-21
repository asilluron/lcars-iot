//! Messages from the UI thread. The UI never awaits the network.

#[derive(Clone, PartialEq)]
pub enum UiIntent {
    SetOnOff {
        entity_id: String,
        on: bool,
    },
    SetBrightness {
        entity_id: String,
        pct: u32,
        release: bool,
    },
    SetColorTemp {
        entity_id: String,
        kelvin: u32,
        release: bool,
    },
    SetTargetTemp {
        entity_id: String,
        value: f64,
        unit: i32,
        release: bool,
    },
    SetHvacMode {
        entity_id: String,
        mode: i32,
    },
    SetLock {
        entity_id: String,
        locked: bool,
        pin: Option<String>,
    },
    SetCoverPosition {
        entity_id: String,
        pct: u32,
        release: bool,
    },
    ActivateScene {
        entity_id: String,
    },
    SubmitPin {
        command_id: String,
        pin: String,
    },
}

impl UiIntent {
    #[must_use]
    pub fn entity_id(&self) -> Option<&str> {
        match self {
            Self::SetOnOff { entity_id, .. }
            | Self::SetBrightness { entity_id, .. }
            | Self::SetColorTemp { entity_id, .. }
            | Self::SetTargetTemp { entity_id, .. }
            | Self::SetHvacMode { entity_id, .. }
            | Self::SetLock { entity_id, .. }
            | Self::SetCoverPosition { entity_id, .. }
            | Self::ActivateScene { entity_id } => Some(entity_id),
            Self::SubmitPin { .. } => None,
        }
    }

    #[must_use]
    pub fn action_name(&self) -> &'static str {
        match self {
            Self::SetOnOff { .. } => "set_on_off",
            Self::SetBrightness { .. } => "set_brightness",
            Self::SetColorTemp { .. } => "set_color_temp",
            Self::SetTargetTemp { .. } => "set_target_temp",
            Self::SetHvacMode { .. } => "set_hvac_mode",
            Self::SetLock { .. } => "set_lock",
            Self::SetCoverPosition { .. } => "set_cover_position",
            Self::ActivateScene { .. } => "activate_scene",
            Self::SubmitPin { .. } => "submit_pin",
        }
    }

    #[must_use]
    pub fn is_slider(&self) -> bool {
        matches!(
            self,
            Self::SetBrightness { .. }
                | Self::SetColorTemp { .. }
                | Self::SetTargetTemp { .. }
                | Self::SetCoverPosition { .. }
        )
    }

    #[must_use]
    pub fn is_release(&self) -> bool {
        match self {
            Self::SetBrightness { release, .. }
            | Self::SetColorTemp { release, .. }
            | Self::SetTargetTemp { release, .. }
            | Self::SetCoverPosition { release, .. } => *release,
            _ => false,
        }
    }
}

impl std::fmt::Debug for UiIntent {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SetLock {
                entity_id,
                locked,
                pin,
            } => formatter
                .debug_struct("SetLock")
                .field("entity_id", entity_id)
                .field("locked", locked)
                .field("pin", &pin.as_ref().map(|_| "****"))
                .finish(),
            Self::SubmitPin { command_id, .. } => formatter
                .debug_struct("SubmitPin")
                .field("command_id", command_id)
                .field("pin", &"****")
                .finish(),
            other => formatter
                .debug_struct(other.action_name())
                .field("entity_id", &other.entity_id())
                .finish(),
        }
    }
}
