//! Display power phases. The first touch after dim or off only wakes the panel.

use std::time::Instant;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PowerPhase {
    Active,
    Dim,
    Off,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TouchEffect {
    Deliver,
    WakeOnly,
}

#[derive(Clone, Debug)]
pub struct PowerPolicy {
    pub brightness_pct: u32,
    pub idle_dim_s: u64,
    pub idle_off_s: u64,
    pub night_cap_pct: u32,
}

pub struct DisplayPower {
    last_touch: Instant,
    phase: PowerPhase,
    policy: PowerPolicy,
}

impl DisplayPower {
    #[must_use]
    pub fn new(policy: PowerPolicy, now: Instant) -> Self {
        Self {
            last_touch: now,
            phase: PowerPhase::Active,
            policy,
        }
    }

    pub fn touch(&mut self, now: Instant) -> TouchEffect {
        let waking = self.phase != PowerPhase::Active;
        self.last_touch = now;
        self.phase = PowerPhase::Active;
        if waking {
            TouchEffect::WakeOnly
        } else {
            TouchEffect::Deliver
        }
    }

    pub fn critical(&mut self, now: Instant) {
        self.last_touch = now;
        self.phase = PowerPhase::Active;
    }

    pub fn tick(&mut self, now: Instant) -> PowerPhase {
        let idle = now.duration_since(self.last_touch).as_secs();
        self.phase = if idle >= self.policy.idle_off_s {
            PowerPhase::Off
        } else if idle >= self.policy.idle_dim_s {
            PowerPhase::Dim
        } else {
            PowerPhase::Active
        };
        self.phase
    }

    #[cfg(test)]
    fn phase(&self) -> PowerPhase {
        self.phase
    }

    #[must_use]
    pub fn brightness(&self, in_night: bool) -> u32 {
        let base = match self.phase {
            PowerPhase::Active => self.policy.brightness_pct,
            PowerPhase::Dim => 10,
            PowerPhase::Off => 0,
        };
        if in_night {
            base.min(self.policy.night_cap_pct)
        } else {
            base
        }
    }
}

#[must_use]
pub fn in_night(minute_of_day: u16, start: u16, end: u16) -> bool {
    if start == end {
        return false;
    }
    if start < end {
        minute_of_day >= start && minute_of_day < end
    } else {
        minute_of_day >= start || minute_of_day < end
    }
}

pub fn write_brightness(pct: u32) {
    let Some(dir) = backlight_dir() else {
        tracing::info!("no backlight control; night mode keeps the on-screen palette");
        return;
    };
    let max = std::fs::read_to_string(dir.join("max_brightness"))
        .ok()
        .and_then(|text| text.trim().parse::<u32>().ok())
        .unwrap_or(255);
    let value = brightness_level(pct, max);
    if let Err(error) = std::fs::write(dir.join("brightness"), value.to_string()) {
        tracing::info!(%error, path = %dir.display(), "backlight write failed");
        return;
    }
    tracing::info!(pct, value, path = %dir.display(), "panel brightness");
}

/// `LCARS_BACKLIGHT` wins. Otherwise the first sysfs backlight that can be written.
fn backlight_dir() -> Option<std::path::PathBuf> {
    if let Ok(dir) = std::env::var("LCARS_BACKLIGHT")
        && !dir.is_empty()
    {
        return Some(std::path::PathBuf::from(dir));
    }
    let entries = std::fs::read_dir("/sys/class/backlight").ok()?;
    entries
        .flatten()
        .map(|entry| entry.path())
        .find(|path| path.join("brightness").is_file() && path.join("max_brightness").is_file())
}

#[must_use]
pub fn brightness_level(pct: u32, max: u32) -> u32 {
    let max = max.max(1);
    u32::try_from(u64::from(pct.min(100)) * u64::from(max) / 100).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::{DisplayPower, PowerPhase, PowerPolicy, TouchEffect, brightness_level, in_night};
    use std::time::{Duration, Instant};

    fn policy() -> PowerPolicy {
        PowerPolicy {
            brightness_pct: 80,
            idle_dim_s: 300,
            idle_off_s: 360,
            night_cap_pct: 30,
        }
    }

    #[test]
    fn phases_and_wake_touch() {
        let start = Instant::now();
        let mut power = DisplayPower::new(policy(), start);
        assert_eq!(
            power.tick(start + Duration::from_secs(299)),
            PowerPhase::Active
        );
        assert_eq!(
            power.tick(start + Duration::from_secs(300)),
            PowerPhase::Dim
        );
        assert_eq!(power.brightness(false), 10);
        assert_eq!(
            power.touch(start + Duration::from_secs(301)),
            TouchEffect::WakeOnly
        );
        assert_eq!(power.phase(), PowerPhase::Active);
        assert_eq!(
            power.touch(start + Duration::from_secs(302)),
            TouchEffect::Deliver
        );
        power.tick(start + Duration::from_secs(302 + 360));
        assert_eq!(power.phase(), PowerPhase::Off);
        power.critical(start + Duration::from_secs(700));
        assert_eq!(power.phase(), PowerPhase::Active);
        assert_eq!(power.brightness(true), 30);
    }

    #[test]
    fn brightness_level_scales_to_the_panel_max() {
        assert_eq!(brightness_level(100, 255), 255);
        assert_eq!(brightness_level(0, 255), 0);
        assert_eq!(brightness_level(28, 255), 71);
        assert_eq!(brightness_level(150, 100), 100);
    }

    #[test]
    fn night_window_crosses_midnight() {
        assert!(in_night(23 * 60, 22 * 60, 6 * 60));
        assert!(in_night(5 * 60, 22 * 60, 6 * 60));
        assert!(!in_night(12 * 60, 22 * 60, 6 * 60));
    }
}
