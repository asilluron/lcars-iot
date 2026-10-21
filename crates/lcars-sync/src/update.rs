//! Snapshots the sync task hands to the UI thread.

use home_proto::v1::ScreenLayout;

use crate::project::TileModel;

#[derive(Clone, Debug)]
pub enum UiUpdate {
    Link {
        label: &'static str,
        controls_enabled: bool,
        values_dimmed: bool,
    },
    Layout(ScreenLayout),
    Tiles(Vec<TileModel>),
    CommandFailed {
        entity_id: String,
        message: String,
    },
    PinRequired {
        entity_id: String,
        command_id: String,
    },
    Alert {
        title: String,
        body: String,
        critical: bool,
    },
    Error(String),
}
