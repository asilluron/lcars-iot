//! NATS sync for one screen. This crate does not depend on Slint.

mod config;
mod intent;
mod layout;
mod nats;
mod policy;
mod project;
mod store;
pub mod subjects;
mod update;

pub use config::SyncConfig;
pub use intent::UiIntent;
pub use layout::{COLOR_TOKENS, LayoutError, baked_layout, parse_layout_toml, validate_layout};
pub use nats::run;
pub use policy::{INTENT_QUEUE, LinkPhase, link_phase, reconnect_delay};
pub use project::{TileModel, project_layout};
pub use store::StateStore;
pub use update::UiUpdate;
