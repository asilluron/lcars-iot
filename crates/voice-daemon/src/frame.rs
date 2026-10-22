//! 20 ms S16LE frames. The pre-roll is the last 1.5 s, padded with silence.

use voice_proto::{FRAME_SAMPLES, PREROLL_FRAMES, PREROLL_SAMPLES};

#[must_use]
pub fn pack_frame(samples: &[i16]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(samples.len() * 2);
    for sample in samples {
        bytes.extend_from_slice(&sample.to_le_bytes());
    }
    bytes
}

/// Last 1.5 s as 75 frames. A short buffer is padded with leading silence.
#[must_use]
pub fn preroll_frames(samples: &[i16]) -> Vec<Vec<u8>> {
    let tail = if samples.len() > PREROLL_SAMPLES {
        &samples[samples.len() - PREROLL_SAMPLES..]
    } else {
        samples
    };
    let mut padded = vec![0i16; PREROLL_SAMPLES - tail.len()];
    padded.extend_from_slice(tail);
    let frames: Vec<Vec<u8>> = padded.chunks(FRAME_SAMPLES).map(pack_frame).collect();
    debug_assert_eq!(frames.len(), PREROLL_FRAMES);
    frames
}

#[cfg(test)]
mod tests {
    use super::preroll_frames;
    use voice_proto::{FRAME_BYTES, FRAME_SAMPLES, PREROLL_FRAMES, PREROLL_SAMPLES};

    #[test]
    fn full_buffer_is_seventy_five_frames_of_the_tail() {
        let samples: Vec<i16> = (0..40_000).map(|n| (n % 97) as i16).collect();
        let frames = preroll_frames(&samples);
        assert_eq!(frames.len(), PREROLL_FRAMES);
        assert!(frames.iter().all(|frame| frame.len() == FRAME_BYTES));
        let start = samples.len() - PREROLL_SAMPLES;
        let first = i16::from_le_bytes([frames[0][0], frames[0][1]]);
        assert_eq!(first, samples[start]);
        let last = frames.last().unwrap();
        let end = i16::from_le_bytes([last[last.len() - 2], last[last.len() - 1]]);
        assert_eq!(end, *samples.last().unwrap());
        assert_eq!(FRAME_SAMPLES * PREROLL_FRAMES, PREROLL_SAMPLES);
    }

    #[test]
    fn a_short_buffer_pads_the_front_with_silence() {
        let frames = preroll_frames(&[7, 8, 9]);
        assert_eq!(frames.len(), PREROLL_FRAMES);
        assert_eq!(frames[0], vec![0; FRAME_BYTES]);
        let last = frames.last().unwrap();
        let n = last.len();
        assert_eq!(i16::from_le_bytes([last[n - 6], last[n - 5]]), 7);
        assert_eq!(i16::from_le_bytes([last[n - 4], last[n - 3]]), 8);
        assert_eq!(i16::from_le_bytes([last[n - 2], last[n - 1]]), 9);
    }
}
