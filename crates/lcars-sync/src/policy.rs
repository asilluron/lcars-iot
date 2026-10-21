//! Timing rules that do not touch the network: backoff, link phase, and coalescing.

use std::collections::HashMap;
use std::time::Duration;

use crate::intent::UiIntent;

pub const INTENT_QUEUE: usize = 256;
pub const FRAME_MS: u64 = 16;
pub const SLIDER_COALESCE_MS: u64 = 100;
pub const LINK_LOST_AFTER: Duration = Duration::from_secs(3);
pub const VALUES_DIM_AFTER: Duration = Duration::from_secs(30);
pub const BACKEND_STALE_MS: i64 = 15_000;
pub const RECONNECT_BASE_MS: u64 = 250;
pub const RECONNECT_CAP_MS: u64 = 5_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkPhase {
    Connecting,
    Online,
    NoLink,
    LinkLost,
    ValuesDimmed,
    BackendOffline,
}

impl LinkPhase {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Connecting => "CONNECTING",
            Self::Online => "ONLINE",
            Self::NoLink => "NO LINK",
            Self::LinkLost | Self::ValuesDimmed => "LINK LOST",
            Self::BackendOffline => "BACKEND OFFLINE",
        }
    }

    #[must_use]
    pub fn controls_enabled(self) -> bool {
        matches!(self, Self::Online)
    }

    #[must_use]
    pub fn values_dimmed(self) -> bool {
        matches!(self, Self::ValuesDimmed)
    }
}

/// A short disconnect keeps the last online phase so a blip does not flash the strip.
#[must_use]
pub fn link_phase(
    ever_connected: bool,
    connected: bool,
    disconnected_for: Duration,
    backend_stale: bool,
) -> LinkPhase {
    if !connected {
        if !ever_connected {
            return LinkPhase::NoLink;
        }
        if disconnected_for >= VALUES_DIM_AFTER {
            return LinkPhase::ValuesDimmed;
        }
        if disconnected_for >= LINK_LOST_AFTER {
            return LinkPhase::LinkLost;
        }
        return LinkPhase::Online;
    }
    if backend_stale {
        LinkPhase::BackendOffline
    } else {
        LinkPhase::Online
    }
}

/// Exponential backoff from 250 ms to a 5 s base, then up to 20% extra jitter.
/// `jitter_unit` is in `0.0..=1.0`.
#[must_use]
pub fn reconnect_delay(attempt: u32, jitter_unit: f64) -> Duration {
    let shift = attempt.min(16);
    let base_ms = RECONNECT_BASE_MS
        .saturating_mul(1_u64 << shift)
        .min(RECONNECT_CAP_MS);
    let jitter = jitter_unit.clamp(0.0, 1.0);
    let ms = (base_ms as f64 * (1.0 + 0.2 * jitter)).round() as u64;
    Duration::from_millis(ms)
}

#[must_use]
pub fn jitter_unit() -> f64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.subsec_nanos())
        .unwrap_or(1);
    f64::from(nanos % 10_000) / 10_000.0
}

/// Last write per key wins. The batch is released once per frame.
#[derive(Debug)]
pub struct FrameCoalescer<T> {
    pending: HashMap<String, T>,
    started_ms: Option<u64>,
    frame_ms: u64,
}

impl<T> FrameCoalescer<T> {
    #[must_use]
    pub fn new(frame_ms: u64) -> Self {
        Self {
            pending: HashMap::new(),
            started_ms: None,
            frame_ms,
        }
    }

    pub fn push(&mut self, key: impl Into<String>, value: T, now_ms: u64) {
        if self.started_ms.is_none() {
            self.started_ms = Some(now_ms);
        }
        self.pending.insert(key.into(), value);
    }

    pub fn take_due(&mut self, now_ms: u64) -> Option<Vec<(String, T)>> {
        let started = self.started_ms?;
        if now_ms.saturating_sub(started) < self.frame_ms || self.pending.is_empty() {
            return None;
        }
        self.started_ms = None;
        Some(std::mem::take(&mut self.pending).into_iter().collect())
    }
}

/// At most one slider command per entity per 100 ms. Release always sends.
#[derive(Debug, Default)]
pub struct DragCoalescer {
    last_sent_ms: HashMap<String, u64>,
    held: HashMap<String, UiIntent>,
}

impl DragCoalescer {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the intent to send now.
    pub fn push(&mut self, intent: UiIntent, now_ms: u64) -> Option<UiIntent> {
        let Some(entity_id) = intent.entity_id().map(str::to_string) else {
            return Some(intent);
        };
        if intent.is_release() {
            self.held.remove(&entity_id);
            self.last_sent_ms.insert(entity_id, now_ms);
            return Some(intent);
        }
        if !intent.is_slider() {
            return Some(intent);
        }
        let due = self
            .last_sent_ms
            .get(&entity_id)
            .is_none_or(|last| now_ms.saturating_sub(*last) >= SLIDER_COALESCE_MS);
        if due {
            self.held.remove(&entity_id);
            self.last_sent_ms.insert(entity_id, now_ms);
            Some(intent)
        } else {
            self.held.insert(entity_id, intent);
            None
        }
    }

    pub fn flush_due(&mut self, now_ms: u64) -> Vec<UiIntent> {
        let ready: Vec<String> = self
            .held
            .keys()
            .filter(|entity_id| {
                let last = self.last_sent_ms.get(*entity_id).copied().unwrap_or(0);
                now_ms.saturating_sub(last) >= SLIDER_COALESCE_MS
            })
            .cloned()
            .collect();
        ready
            .into_iter()
            .filter_map(|entity_id| {
                let intent = self.held.remove(&entity_id)?;
                self.last_sent_ms.insert(entity_id, now_ms);
                Some(intent)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DragCoalescer, FrameCoalescer, LINK_LOST_AFTER, LinkPhase, RECONNECT_CAP_MS,
        VALUES_DIM_AFTER, link_phase, reconnect_delay,
    };
    use crate::intent::UiIntent;
    use std::time::Duration;

    #[test]
    fn link_phases_follow_the_failure_table() {
        assert_eq!(
            link_phase(false, false, Duration::ZERO, false),
            LinkPhase::NoLink
        );
        assert_eq!(
            link_phase(true, false, Duration::from_secs(1), false),
            LinkPhase::Online
        );
        assert_eq!(
            link_phase(true, false, LINK_LOST_AFTER, false),
            LinkPhase::LinkLost
        );
        assert_eq!(
            link_phase(true, false, VALUES_DIM_AFTER, false),
            LinkPhase::ValuesDimmed
        );
        assert!(!LinkPhase::LinkLost.controls_enabled());
        assert!(LinkPhase::ValuesDimmed.values_dimmed());
        assert_eq!(
            link_phase(true, true, Duration::ZERO, true),
            LinkPhase::BackendOffline
        );
        assert_eq!(LinkPhase::BackendOffline.label(), "BACKEND OFFLINE");
        assert!(LinkPhase::Online.controls_enabled());
    }

    #[test]
    fn backoff_doubles_and_caps() {
        assert_eq!(reconnect_delay(0, 0.0), Duration::from_millis(250));
        assert_eq!(reconnect_delay(1, 0.0), Duration::from_millis(500));
        assert_eq!(reconnect_delay(4, 0.0), Duration::from_millis(4_000));
        assert_eq!(
            reconnect_delay(8, 0.0),
            Duration::from_millis(RECONNECT_CAP_MS)
        );
        let jittered = reconnect_delay(0, 1.0);
        assert_eq!(jittered, Duration::from_millis(300));
    }

    #[test]
    fn frame_coalescer_keeps_the_latest_value_per_key() {
        let mut coalescer = FrameCoalescer::new(16);
        coalescer.push("light.kitchen", 1, 0);
        coalescer.push("light.kitchen", 2, 5);
        coalescer.push("light.dining", 9, 5);
        assert!(coalescer.take_due(15).is_none());
        let batch = coalescer.take_due(16).unwrap();
        assert_eq!(batch.len(), 2);
        let kitchen = batch
            .iter()
            .find(|(key, _)| key == "light.kitchen")
            .map(|(_, value)| *value);
        assert_eq!(kitchen, Some(2));
        assert!(coalescer.take_due(40).is_none());
    }

    #[test]
    fn slider_holds_then_releases() {
        let mut drags = DragCoalescer::new();
        let drag = UiIntent::SetBrightness {
            entity_id: "light.kitchen".into(),
            pct: 10,
            release: false,
        };
        assert!(drags.push(drag, 0).is_some());
        let held = UiIntent::SetBrightness {
            entity_id: "light.kitchen".into(),
            pct: 40,
            release: false,
        };
        assert!(drags.push(held, 40).is_none());
        let flushed = drags.flush_due(100);
        assert_eq!(flushed.len(), 1);
        match &flushed[0] {
            UiIntent::SetBrightness { pct, .. } => assert_eq!(*pct, 40),
            _ => panic!("expected brightness"),
        }
        let release = UiIntent::SetBrightness {
            entity_id: "light.kitchen".into(),
            pct: 80,
            release: true,
        };
        assert!(drags.push(release, 110).is_some());
    }
}
