//! Silence before the estimated onset of "computer".
//!
//! The trigger is not enough: the word also shows up mid-sentence. A detection
//! opens a session only when the 400 ms before the onset was non-speech.

/// `true` when `[onset - silence, onset)` is covered by VAD marks and none of
/// them are speech. `onset` is `trigger_ms` minus the median length of the word.
#[must_use]
pub fn silence_before_onset(
    marks: &[(u64, bool)],
    trigger_ms: u64,
    onset_lead_ms: u64,
    silence_ms: u64,
) -> bool {
    if trigger_ms < onset_lead_ms {
        return false;
    }
    let onset = trigger_ms - onset_lead_ms;
    let start = onset.saturating_sub(silence_ms);
    if marks.first().is_none_or(|(time, _)| *time > start) {
        return false;
    }
    let mut saw = false;
    for (time, speech) in marks {
        if *time < start || *time >= onset {
            continue;
        }
        saw = true;
        if *speech {
            return false;
        }
    }
    saw
}

#[cfg(test)]
mod tests {
    use super::silence_before_onset;

    fn quiet(from: u64, to: u64) -> Vec<(u64, bool)> {
        (from..to).step_by(32).map(|time| (time, false)).collect()
    }

    #[test]
    fn silence_in_the_window_accepts() {
        let marks = quiet(0, 800);
        assert!(silence_before_onset(&marks, 800, 650, 400));
    }

    #[test]
    fn speech_in_the_window_rejects() {
        let mut marks = quiet(0, 800);
        marks.push((96, true));
        marks.sort_by_key(|(time, _)| *time);
        assert!(!silence_before_onset(&marks, 800, 650, 400));
    }

    #[test]
    fn missing_history_rejects() {
        assert!(!silence_before_onset(&[], 800, 650, 400));
        assert!(!silence_before_onset(&[(700, false)], 800, 650, 400));
        assert!(!silence_before_onset(&[(0, false)], 100, 650, 400));
    }
}
