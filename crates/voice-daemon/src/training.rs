//! One-second PCM batches for `lcars.training.put`.

use voice_proto::SAMPLE_RATE;

/// Samples in one training request: one second at 16 kHz.
pub const SAMPLES: usize = SAMPLE_RATE as usize;

/// How many one-second chunks may wait for the hub before the newest is dropped.
pub const QUEUE_DEPTH: usize = 4;

/// Accumulates captured samples and emits a chunk once a second of them is ready.
#[derive(Default)]
pub struct Batch {
    pending: Vec<i16>,
}

impl Batch {
    /// Adds one sample. Returns a chunk when a full second has been captured
    /// while `enabled` is true. Turning the flag off discards a partial second.
    pub fn push(&mut self, sample: i16, enabled: bool) -> Option<Vec<u8>> {
        if !enabled {
            self.pending.clear();
            return None;
        }
        self.pending.push(sample);
        if self.pending.len() < SAMPLES {
            return None;
        }
        let chunk: Vec<i16> = self.pending.drain(..SAMPLES).collect();
        Some(pack(&chunk))
    }

    pub fn clear(&mut self) {
        self.pending.clear();
    }
}

fn pack(samples: &[i16]) -> Vec<u8> {
    let mut pcm = Vec::with_capacity(samples.len() * 2);
    for sample in samples {
        pcm.extend_from_slice(&sample.to_le_bytes());
    }
    pcm
}

#[cfg(test)]
mod tests {
    use super::{Batch, SAMPLES};

    #[test]
    fn one_chunk_per_second_only_while_enabled() {
        let mut batch = Batch::default();
        for _ in 0..SAMPLES * 3 {
            assert!(batch.push(1, false).is_none());
        }

        let mut chunks = Vec::new();
        for i in 0..SAMPLES * 2 {
            let sample = if i == 0 { 0x1234 } else { 1 };
            if let Some(pcm) = batch.push(sample, true) {
                chunks.push(pcm);
            }
        }
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].len(), SAMPLES * 2);
        assert_eq!(&chunks[0][..2], &0x1234_i16.to_le_bytes());
        assert_eq!(&chunks[1][..2], &1_i16.to_le_bytes());

        // A partial second is dropped when the flag turns off, so the next
        // second starts clean.
        for _ in 0..100 {
            assert!(batch.push(7, true).is_none());
        }
        assert!(batch.push(7, false).is_none());
        let mut emitted = false;
        for i in 0..SAMPLES {
            let sample = if i == 0 { 9 } else { 1 };
            if let Some(pcm) = batch.push(sample, true) {
                assert_eq!(&pcm[..2], &9_i16.to_le_bytes());
                emitted = true;
            }
        }
        assert!(emitted);
    }
}
