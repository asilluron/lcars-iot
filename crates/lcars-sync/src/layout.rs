//! Screen layout validation. The baked file is the offline fallback.

use home_proto::v1::{Panel, PanelKind, ScreenLayout};
use serde::Deserialize;
use thiserror::Error;

pub const MAX_PANELS: usize = 12;
pub const MAX_ENTITIES_PER_PANEL: usize = 48;

/// Color tokens a panel accent is allowed to name.
pub const COLOR_TOKENS: &[&str] = &[
    "bg",
    "accent-1",
    "accent-2",
    "accent-3",
    "accent-4",
    "alert",
    "text-on-accent",
];

const BAKED: &str = include_str!("../../../assets/default_layout.toml");

#[derive(Debug, Error, PartialEq, Eq)]
pub enum LayoutError {
    #[error("layout toml: {0}")]
    Toml(String),
    #[error("unknown panel kind `{0}`")]
    UnknownKind(String),
    #[error("schema version {0} is not supported")]
    Schema(u32),
    #[error("too many panels: {0}")]
    TooManyPanels(usize),
    #[error("panel `{id}` has {count} entities")]
    TooManyEntities { id: String, count: usize },
    #[error("duplicate panel id `{0}`")]
    DuplicatePanel(String),
    #[error("panel `{0}` is missing an id")]
    MissingPanelId(String),
    #[error("panel `{0}` has an empty entity id")]
    EmptyEntity(String),
    #[error("panel `{id}` accent `{accent}` is not a theme token")]
    Accent { id: String, accent: String },
    #[error("home panel `{0}` does not exist")]
    HomePanel(String),
    #[error("layout has no panels")]
    Empty,
}

#[derive(Debug, Deserialize)]
struct LayoutFile {
    schema_version: u32,
    #[serde(default)]
    home_panel_id: String,
    #[serde(default)]
    panels: Vec<PanelFile>,
}

#[derive(Debug, Deserialize)]
struct PanelFile {
    id: String,
    title: String,
    kind: String,
    #[serde(default)]
    entity_ids: Vec<String>,
    accent: String,
}

#[must_use]
pub fn baked_layout() -> ScreenLayout {
    parse_layout_toml(BAKED).expect("baked default layout")
}

pub fn parse_layout_toml(text: &str) -> Result<ScreenLayout, LayoutError> {
    let file: LayoutFile =
        toml::from_str(text).map_err(|err| LayoutError::Toml(err.to_string()))?;
    let mut home_panel_id = file.home_panel_id;
    if home_panel_id.is_empty() {
        home_panel_id = file
            .panels
            .first()
            .map(|panel| panel.id.clone())
            .unwrap_or_default();
    }
    let panels = file
        .panels
        .into_iter()
        .map(|panel| {
            Ok(Panel {
                id: panel.id,
                title: panel.title,
                kind: parse_kind(&panel.kind)?,
                entity_ids: panel.entity_ids,
                accent: panel.accent,
            })
        })
        .collect::<Result<Vec<_>, LayoutError>>()?;
    let layout = ScreenLayout {
        schema_version: file.schema_version,
        panels,
        home_panel_id,
    };
    validate_layout(&layout)?;
    Ok(layout)
}

pub fn validate_layout(layout: &ScreenLayout) -> Result<(), LayoutError> {
    if layout.schema_version == 0 {
        return Err(LayoutError::Schema(layout.schema_version));
    }
    if layout.panels.is_empty() {
        return Err(LayoutError::Empty);
    }
    if layout.panels.len() > MAX_PANELS {
        return Err(LayoutError::TooManyPanels(layout.panels.len()));
    }
    let mut seen = std::collections::HashSet::new();
    for panel in &layout.panels {
        if panel.id.is_empty() {
            return Err(LayoutError::MissingPanelId(panel.title.clone()));
        }
        if !seen.insert(panel.id.clone()) {
            return Err(LayoutError::DuplicatePanel(panel.id.clone()));
        }
        if PanelKind::try_from(panel.kind).ok() == Some(PanelKind::Unspecified)
            || PanelKind::try_from(panel.kind).is_err()
        {
            return Err(LayoutError::UnknownKind(panel.kind.to_string()));
        }
        if panel.entity_ids.len() > MAX_ENTITIES_PER_PANEL {
            return Err(LayoutError::TooManyEntities {
                id: panel.id.clone(),
                count: panel.entity_ids.len(),
            });
        }
        if panel.entity_ids.iter().any(|id| id.is_empty()) {
            return Err(LayoutError::EmptyEntity(panel.id.clone()));
        }
        if !COLOR_TOKENS.contains(&panel.accent.as_str()) {
            return Err(LayoutError::Accent {
                id: panel.id.clone(),
                accent: panel.accent.clone(),
            });
        }
    }
    if !layout
        .panels
        .iter()
        .any(|panel| panel.id == layout.home_panel_id)
    {
        return Err(LayoutError::HomePanel(layout.home_panel_id.clone()));
    }
    Ok(())
}

fn parse_kind(kind: &str) -> Result<i32, LayoutError> {
    let parsed = match kind {
        "lights" => PanelKind::Lights,
        "climate" => PanelKind::Climate,
        "security" => PanelKind::Security,
        "sensors" => PanelKind::Sensors,
        "scenes" => PanelKind::Scenes,
        "alerts" => PanelKind::Alerts,
        other => return Err(LayoutError::UnknownKind(other.to_string())),
    };
    Ok(parsed as i32)
}

#[cfg(test)]
mod tests {
    use super::{LayoutError, MAX_PANELS, baked_layout, parse_layout_toml, validate_layout};

    #[test]
    fn baked_layout_is_valid() {
        let layout = baked_layout();
        assert_eq!(layout.home_panel_id, "lights");
        assert!(validate_layout(&layout).is_ok());
        assert!(layout.panels.len() <= MAX_PANELS);
    }

    #[test]
    fn rejects_unknown_kind_accent_and_size() {
        let err = parse_layout_toml(
            r#"
            schema_version = 1
            home_panel_id = "lights"
            [[panels]]
            id = "lights"
            title = "LIGHTS"
            kind = "plasma"
            accent = "accent-1"
            "#,
        );
        assert!(matches!(err, Err(LayoutError::UnknownKind(_))));

        let err = parse_layout_toml(
            r#"
            schema_version = 1
            home_panel_id = "lights"
            [[panels]]
            id = "lights"
            title = "LIGHTS"
            kind = "lights"
            accent = "neon"
            "#,
        );
        assert!(matches!(err, Err(LayoutError::Accent { .. })));
    }

    #[test]
    fn rejects_too_many_panels() {
        let mut text = String::from("schema_version = 1\nhome_panel_id = \"p0\"\n");
        for index in 0..=MAX_PANELS {
            text.push_str(&format!(
                "[[panels]]\nid = \"p{index}\"\ntitle = \"P\"\nkind = \"lights\"\naccent = \"accent-1\"\n"
            ));
        }
        let err = parse_layout_toml(&text).unwrap_err();
        assert!(matches!(err, LayoutError::TooManyPanels(_)));
    }
}
