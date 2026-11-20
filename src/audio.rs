//! Audio primitives: answer modes, the test tone, levels, WAV files, PCM queues and resampling.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

/// What the softphone sends once a call is answered.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Silent,
    /// A steady 440 Hz tone.
    Tone,
    /// What it receives, ~20 ms later.
    Echo,
    /// The person at the softphone: the local microphone and speaker.
    Human,
    /// An xAI realtime voice agent.
    Ai,
}

impl Mode {
    pub const ALL: [Mode; 5] = [Mode::Human, Mode::Ai, Mode::Echo, Mode::Tone, Mode::Silent];

    pub fn parse(s: &str) -> anyhow::Result<Mode> {
        Ok(match s {
            "silent" => Mode::Silent,
            "tone" => Mode::Tone,
            "echo" => Mode::Echo,
            "human" => Mode::Human,
            "ai" => Mode::Ai,
            _ => anyhow::bail!("unknown mode {s:?} (want silent, tone, echo, human or ai)"),
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Silent => "silent",
            Mode::Tone => "tone",
            Mode::Echo => "echo",
            Mode::Human => "human",
            Mode::Ai => "ai",
        }
    }

    /// The UI's name for the mode.
    pub fn label(self) -> &'static str {
        match self {
            Mode::Human => "Me",
            Mode::Ai => "Agent",
            Mode::Silent => "Silent",
            Mode::Tone => "Tone",
            Mode::Echo => "Echo",
        }
    }
}

const TONE_HZ: f64 = 440.0;

/// 16-bit PCM for a sine wave, continuous across frames.
pub struct ToneGenerator {
    sample_rate: f64,
    phase: f64,
}

impl ToneGenerator {
    pub fn new(sample_rate: u32) -> Self {
        ToneGenerator { sample_rate: sample_rate as f64, phase: 0.0 }
    }

    pub fn fill(&mut self, pcm: &mut [i16]) {
        let step = 2.0 * std::f64::consts::PI * TONE_HZ / self.sample_rate;
        for s in pcm {
            *s = (self.phase.sin() * 8000.0) as i16;
            self.phase += step;
            if self.phase > 2.0 * std::f64::consts::PI {
                self.phase -= 2.0 * std::f64::consts::PI;
            }
        }
    }
}

/// The RMS level of 16-bit PCM, or None for no samples. Digital silence is reported as -120.
pub fn dbfs(sum_squares: f64, n: usize) -> Option<f64> {
    if n == 0 {
        return None;
    }
    let rms = (sum_squares / n as f64).sqrt();
    let v = if rms > 0.0 { (20.0 * (rms / 32768.0).log10()).max(-120.0) } else { -120.0 };
    Some((v * 10.0).round() / 10.0)
}

pub fn i16s_from_le(b: &[u8]) -> Vec<i16> {
    b.as_chunks::<2>().0.iter().map(|c| i16::from_le_bytes([c[0], c[1]])).collect()
}

pub fn i16s_to_le(pcm: &[i16], out: &mut Vec<u8>) {
    out.clear();
    for s in pcm {
        out.extend_from_slice(&s.to_le_bytes());
    }
}

/// Writes 16-bit mono PCM and patches the header sizes on close.
pub struct WavWriter {
    file: File,
    path: PathBuf,
    sample_rate: u32,
    data_bytes: u32,
}

impl WavWriter {
    pub fn create(path: &Path, sample_rate: u32) -> std::io::Result<Self> {
        let mut w = WavWriter { file: File::create(path)?, path: path.to_path_buf(), sample_rate, data_bytes: 0 };
        let header = w.header();
        w.file.write_all(&header)?;
        Ok(w)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn write(&mut self, pcm_le: &[u8]) -> std::io::Result<()> {
        self.file.write_all(pcm_le)?;
        self.data_bytes += pcm_le.len() as u32;
        Ok(())
    }

    pub fn close(mut self) -> std::io::Result<()> {
        let header = self.header();
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(&header)?;
        self.file.flush()
    }

    fn header(&self) -> Vec<u8> {
        let mut h = Vec::with_capacity(44);
        h.extend_from_slice(b"RIFF");
        h.extend_from_slice(&(36 + self.data_bytes).to_le_bytes());
        h.extend_from_slice(b"WAVEfmt ");
        h.extend_from_slice(&16u32.to_le_bytes());
        h.extend_from_slice(&1u16.to_le_bytes()); // PCM
        h.extend_from_slice(&1u16.to_le_bytes()); // mono
        h.extend_from_slice(&self.sample_rate.to_le_bytes());
        h.extend_from_slice(&(self.sample_rate * 2).to_le_bytes());
        h.extend_from_slice(&2u16.to_le_bytes());
        h.extend_from_slice(&16u16.to_le_bytes());
        h.extend_from_slice(b"data");
        h.extend_from_slice(&self.data_bytes.to_le_bytes());
        h
    }
}

/// Buffers outbound PCM from an asynchronous source (the microphone, xAI) for the 20 ms RTP
/// clock. It holds at most `max` samples and drops the oldest beyond that, so latency stays bounded.
pub struct PcmQueue {
    buf: Mutex<std::collections::VecDeque<i16>>,
    max: usize,
}

impl PcmQueue {
    pub fn new(max: usize) -> Self {
        PcmQueue { buf: Mutex::new(Default::default()), max }
    }

    pub fn push(&self, samples: &[i16]) {
        let mut b = self.buf.lock().unwrap();
        b.extend(samples.iter().copied());
        let over = b.len().saturating_sub(self.max);
        b.drain(..over);
    }

    /// Fills `out` with queued samples and zero-pads when the queue runs dry.
    pub fn pop(&self, out: &mut [i16]) {
        let mut b = self.buf.lock().unwrap();
        for s in out.iter_mut() {
            *s = b.pop_front().unwrap_or(0);
        }
    }

    pub fn clear(&self) {
        self.buf.lock().unwrap().clear();
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.buf.lock().unwrap().len()
    }
}

/// 8 kHz to 24 kHz by linear interpolation. `prev` is the last sample of the previous frame, so
/// frames join without a click.
pub fn upsample3(input: &[i16], prev: i16) -> Vec<i16> {
    let mut out = Vec::with_capacity(input.len() * 3);
    let mut a = prev as i32;
    for &s in input {
        let b = s as i32;
        out.extend([(a + (b - a) / 3) as i16, (a + 2 * (b - a) / 3) as i16, b as i16]);
        a = b;
    }
    out
}

/// 24 kHz to 8 kHz by averaging each group of three. A trailing partial group is returned as
/// the rest, for the next call.
pub fn downsample3(input: &[i16]) -> (Vec<i16>, Vec<i16>) {
    let n = input.len() / 3;
    let out = (0..n).map(|i| ((input[3 * i] as i32 + input[3 * i + 1] as i32 + input[3 * i + 2] as i32) / 3) as i16).collect();
    (out, input[n * 3..].to_vec())
}

/// Fans a call's inbound PCM out to any number of consumers. A slow consumer loses frames
/// instead of stalling the RTP reader.
#[derive(Default)]
pub struct Listeners {
    inner: Mutex<(u64, HashMap<u64, mpsc::Sender<Vec<i16>>>)>,
}

pub const LISTENER_BUFFER: usize = 50;

impl Listeners {
    pub fn subscribe(&self) -> (u64, mpsc::Receiver<Vec<i16>>) {
        let mut g = self.inner.lock().unwrap();
        g.0 += 1;
        let (tx, rx) = mpsc::channel(LISTENER_BUFFER);
        let id = g.0;
        g.1.insert(id, tx);
        (id, rx)
    }

    pub fn unsubscribe(&self, id: u64) {
        self.inner.lock().unwrap().1.remove(&id);
    }

    pub fn publish(&self, frame: &[i16]) {
        let g = self.inner.lock().unwrap();
        for tx in g.1.values() {
            let _ = tx.try_send(frame.to_vec());
        }
    }

    pub fn close_all(&self) {
        self.inner.lock().unwrap().1.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dbfs_reports_silence_and_full_scale() {
        assert_eq!(dbfs(0.0, 0), None);
        assert_eq!(dbfs(0.0, 160), Some(-120.0));
        let full = 32767f64 * 32767.0 * 160.0;
        let v = dbfs(full, 160).unwrap();
        assert!((-0.1..=0.0).contains(&v), "full scale = {v}");
    }

    #[test]
    fn tone_is_audible_and_continuous() {
        let mut g = ToneGenerator::new(8000);
        let (mut a, mut b) = ([0i16; 160], [0i16; 160]);
        g.fill(&mut a);
        g.fill(&mut b);
        let sum: f64 = a.iter().map(|&s| s as f64 * s as f64).sum();
        let v = dbfs(sum, a.len()).unwrap();
        assert!((-16.0..=-14.0).contains(&v), "tone level {v} dBFS, want about -15");
        assert!(!(b[0] == 0 && a[0] == 0 && b[1] == a[1]), "tone restarted between frames");
    }

    #[test]
    fn wav_writer_patches_sizes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.wav");
        let mut w = WavWriter::create(&path, 8000).unwrap();
        w.write(&[0u8; 320]).unwrap();
        w.close().unwrap();
        let b = std::fs::read(&path).unwrap();
        assert_eq!(b.len(), 44 + 320);
        assert_eq!(&b[..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(b[40..44].try_into().unwrap()), 320);
        assert_eq!(u32::from_le_bytes(b[24..28].try_into().unwrap()), 8000);
    }

    #[test]
    fn pcm_queue_bounds_latency_and_pads_when_dry() {
        let q = PcmQueue::new(4);
        q.push(&[1, 2, 3]);
        q.push(&[4, 5, 6]);
        assert_eq!(q.len(), 4, "the queue holds the newest 4");
        let mut out = [0i16; 6];
        q.pop(&mut out);
        assert_eq!(out, [3, 4, 5, 6, 0, 0]);
        q.push(&[7]);
        q.clear();
        assert_eq!(q.len(), 0);
    }

    #[test]
    fn resample_round_trip() {
        let input = [0i16, 300, 600, 900];
        let up = upsample3(&input, 0);
        assert_eq!(up.len(), 12);
        assert_eq!((up[2], up[3], up[5], up[11]), (0, 100, 300, 900));
        let mut with_extra = up.clone();
        with_extra.push(5);
        let (down, rest) = downsample3(&with_extra);
        assert_eq!(down.len(), 4);
        assert_eq!(rest, vec![5]);
        for (v, want) in down.iter().zip(input) {
            let d = *v as i32 - want as i32;
            assert!((-200..=0).contains(&d), "sample drifted: {v} vs {want}");
        }
    }

    #[test]
    fn listeners_drop_for_slow_consumers() {
        let l = Listeners::default();
        let (id, mut rx) = l.subscribe();
        for i in 0..60 {
            l.publish(&[i as i16]);
        }
        let mut n = 0;
        while rx.try_recv().is_ok() {
            n += 1;
        }
        assert_eq!(n, LISTENER_BUFFER, "the rest dropped");
        l.unsubscribe(id);
        l.publish(&[1]);
    }

    #[test]
    fn modes_round_trip() {
        for m in Mode::ALL {
            assert_eq!(Mode::parse(m.as_str()).unwrap(), m);
        }
        assert!(Mode::parse("robot").is_err());
    }
}
