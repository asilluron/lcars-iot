//! ALSA capture on its own thread. A missing microphone is reopened in place.

use rtrb::Producer;

use crate::cmd::Cmd;

#[cfg(target_os = "linux")]
use crate::device::select_capture_device;
#[cfg(target_os = "linux")]
use std::thread;
#[cfg(target_os = "linux")]
use std::time::Duration;

#[cfg(target_os = "linux")]
pub fn spawn(requested: String, producer: Producer<i16>, cmd: std::sync::mpsc::Sender<Cmd>) {
    thread::Builder::new()
        .name("voice-capture".into())
        .spawn(move || run(requested, producer, cmd))
        .expect("capture thread");
}

#[cfg(not(target_os = "linux"))]
pub fn spawn(requested: String, mut producer: Producer<i16>, cmd: std::sync::mpsc::Sender<Cmd>) {
    let _ = (requested, &mut producer);
    tracing::error!("microphone capture requires linux");
    let _ = cmd.send(Cmd::MicOffline);
}

#[cfg(target_os = "linux")]
fn run(requested: String, mut producer: Producer<i16>, cmd: std::sync::mpsc::Sender<Cmd>) {
    boost();
    let mut online = false;
    let mut reported_offline = false;
    loop {
        let name = current_device(&requested);
        match capture_until_error(&name, &mut producer, &mut online, &cmd) {
            Ok(()) => reported_offline = false,
            Err(error) => {
                tracing::warn!(device = %name, %error, "capture stopped");
                if online || !reported_offline {
                    online = false;
                    reported_offline = true;
                    let _ = cmd.send(Cmd::MicOffline);
                }
                thread::sleep(Duration::from_millis(200));
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn capture_until_error(
    name: &str,
    producer: &mut Producer<i16>,
    online: &mut bool,
    cmd: &std::sync::mpsc::Sender<Cmd>,
) -> Result<(), String> {
    use alsa::pcm::{Access, Format, HwParams, PCM};
    use alsa::{Direction, ValueOr};

    let pcm = PCM::new(name, Direction::Capture, false).map_err(|error| error.to_string())?;
    let (rate, period) = {
        let hwp = HwParams::any(&pcm).map_err(|error| error.to_string())?;
        hwp.set_channels(1).map_err(|error| error.to_string())?;
        hwp.set_rate(16_000, ValueOr::Nearest)
            .map_err(|error| error.to_string())?;
        hwp.set_format(Format::s16())
            .map_err(|error| error.to_string())?;
        hwp.set_access(Access::RWInterleaved)
            .map_err(|error| error.to_string())?;
        hwp.set_period_size(160, ValueOr::Nearest)
            .map_err(|error| error.to_string())?;
        let period = hwp.get_period_size().map_err(|error| error.to_string())?;
        hwp.set_buffer_size(period.saturating_mul(4))
            .map_err(|error| error.to_string())?;
        pcm.hw_params(&hwp).map_err(|error| error.to_string())?;
        let current = pcm.hw_params_current().map_err(|error| error.to_string())?;
        let rate = current.get_rate().map_err(|error| error.to_string())?;
        let period = current
            .get_period_size()
            .map_err(|error| error.to_string())?;
        (rate, period as usize)
    };
    if rate != 16_000 {
        tracing::warn!(rate, "capture device is not 16 kHz; resampling");
    } else {
        tracing::info!(device = name, rate, period, "capture open");
    }
    let mut resampler = RateFix::new(rate, 16_000, period.max(1))?;
    pcm.start().map_err(|error| error.to_string())?;
    if !*online {
        *online = true;
        let _ = cmd.send(Cmd::MicOnline);
    }
    let io = pcm.io_i16().map_err(|error| error.to_string())?;
    let mut buffer = vec![0i16; period.max(1)];
    loop {
        let read = io.readi(&mut buffer).map_err(|error| error.to_string())?;
        if read == 0 {
            continue;
        }
        let samples = resampler.push(&buffer[..read])?;
        for sample in samples {
            if producer.push(sample).is_err() {
                tracing::debug!("capture ring is full");
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn current_device(requested: &str) -> String {
    let cards = std::fs::read_to_string("/proc/asound/cards").unwrap_or_default();
    select_capture_device(requested, &cards)
}

#[cfg(target_os = "linux")]
fn boost() {
    unsafe {
        let param = libc::sched_param { sched_priority: 50 };
        if libc::sched_setscheduler(0, libc::SCHED_FIFO, &param) != 0 {
            tracing::warn!(
                error = %std::io::Error::last_os_error(),
                "realtime priority unavailable"
            );
        }
    }
}

#[cfg(target_os = "linux")]
use rubato::Resampler;

#[cfg(target_os = "linux")]
struct RateFix {
    inner: Option<rubato::FastFixedIn<f32>>,
    pending: Vec<f32>,
    chunk: usize,
}

#[cfg(target_os = "linux")]
impl RateFix {
    fn new(input: u32, output: u32, chunk: usize) -> Result<Self, String> {
        if input == output {
            return Ok(Self {
                inner: None,
                pending: Vec::new(),
                chunk,
            });
        }
        let ratio = f64::from(output) / f64::from(input);
        let inner =
            rubato::FastFixedIn::<f32>::new(ratio, 1.0, rubato::PolynomialDegree::Cubic, chunk, 1)
                .map_err(|error| error.to_string())?;
        Ok(Self {
            inner: Some(inner),
            pending: Vec::new(),
            chunk,
        })
    }

    fn push(&mut self, samples: &[i16]) -> Result<Vec<i16>, String> {
        let Some(inner) = self.inner.as_mut() else {
            return Ok(samples.to_vec());
        };
        self.pending
            .extend(samples.iter().map(|sample| f32::from(*sample)));
        let mut out = Vec::new();
        while self.pending.len() >= self.chunk {
            let block: Vec<f32> = self.pending.drain(..self.chunk).collect();
            let processed = inner
                .process(&[block], None)
                .map_err(|error| error.to_string())?;
            for sample in &processed[0] {
                let clamped = sample.clamp(f32::from(i16::MIN), f32::from(i16::MAX));
                out.push(clamped as i16);
            }
        }
        Ok(out)
    }
}
