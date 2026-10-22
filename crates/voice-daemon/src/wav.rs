//! Minimal 16 kHz mono S16LE WAV reader and writer.

use std::fs::File;
use std::io::{self, Read, Write};
use std::path::Path;

use voice_proto::SAMPLE_RATE;

pub fn write_wav(path: &Path, samples: &[i16]) -> io::Result<()> {
    let mut file = File::create(path)?;
    let data_bytes = samples.len() * 2;
    let riff_size = 36 + data_bytes;
    file.write_all(b"RIFF")?;
    file.write_all(&(riff_size as u32).to_le_bytes())?;
    file.write_all(b"WAVEfmt ")?;
    file.write_all(&16u32.to_le_bytes())?;
    file.write_all(&1u16.to_le_bytes())?;
    file.write_all(&1u16.to_le_bytes())?;
    file.write_all(&SAMPLE_RATE.to_le_bytes())?;
    file.write_all(&(SAMPLE_RATE * 2).to_le_bytes())?;
    file.write_all(&2u16.to_le_bytes())?;
    file.write_all(&16u16.to_le_bytes())?;
    file.write_all(b"data")?;
    file.write_all(&(data_bytes as u32).to_le_bytes())?;
    for sample in samples {
        file.write_all(&sample.to_le_bytes())?;
    }
    Ok(())
}

pub fn read_wav(path: &Path) -> io::Result<Vec<i16>> {
    let mut bytes = Vec::new();
    File::open(path)?.read_to_end(&mut bytes)?;
    parse_wav(&bytes).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

pub fn parse_wav(bytes: &[u8]) -> Result<Vec<i16>, String> {
    if bytes.len() < 44 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err("not a wav file".into());
    }
    let mut offset = 12;
    let mut format = None;
    let mut data = None;
    while offset + 8 <= bytes.len() {
        let id = &bytes[offset..offset + 4];
        let size = u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().unwrap()) as usize;
        let start = offset + 8;
        let end = start + size;
        if end > bytes.len() {
            return Err("wav chunk overruns the file".into());
        }
        if id == b"fmt " {
            if size < 16 {
                return Err("wav fmt chunk is short".into());
            }
            let audio_format = u16::from_le_bytes(bytes[start..start + 2].try_into().unwrap());
            let channels = u16::from_le_bytes(bytes[start + 2..start + 4].try_into().unwrap());
            let rate = u32::from_le_bytes(bytes[start + 4..start + 8].try_into().unwrap());
            let bits = u16::from_le_bytes(bytes[start + 14..start + 16].try_into().unwrap());
            format = Some((audio_format, channels, rate, bits));
        } else if id == b"data" {
            data = Some(&bytes[start..end]);
        }
        offset = end + (size % 2);
    }
    let Some((audio_format, channels, rate, bits)) = format else {
        return Err("wav file has no fmt chunk".into());
    };
    if audio_format != 1 || channels != 1 || bits != 16 || rate != SAMPLE_RATE {
        return Err(format!(
            "wav must be 16 kHz mono 16-bit pcm, got format {audio_format} channels {channels} rate {rate} bits {bits}"
        ));
    }
    let Some(data) = data else {
        return Err("wav file has no data chunk".into());
    };
    if data.len() % 2 != 0 {
        return Err("wav data is not a whole number of samples".into());
    }
    Ok(data
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| i16::from_le_bytes(*pair))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::{parse_wav, write_wav};

    #[test]
    fn wav_round_trip() {
        let dir = std::env::temp_dir().join(format!("voice-wav-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("clip.wav");
        let samples = vec![0, 1, -2, 32_000, -32_000];
        write_wav(&path, &samples).unwrap();
        let read = parse_wav(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(read, samples);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
