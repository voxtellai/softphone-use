//! `Mode::Human`: the call plays on this computer's speaker and takes its microphone.
//!
//! cpal streams are not `Send` on every platform, so one thread owns them for the call.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::call::Call;

const RATE: f64 = 8000.0;
/// Frames the microphone sends to the call: 20 ms at 8 kHz.
const FRAME: usize = 160;

pub fn start(c: &Arc<Call>) {
    let stop = {
        let mut g = c.lock();
        if g.human_stop.is_some() {
            return;
        }
        let stop = Arc::new(AtomicBool::new(false));
        g.human_stop = Some(stop.clone());
        stop
    };
    let call = c.clone();
    std::thread::Builder::new()
        .name("softphone-audio".into())
        .spawn(move || match run(&call, &stop) {
            Ok(()) => call.event("local audio disconnected"),
            Err(e) => {
                call.event(format!("local audio failed: {e}"));
                call.lock().human_stop = None;
            }
        })
        .expect("spawn audio thread");
}

fn run(call: &Arc<Call>, stop: &AtomicBool) -> anyhow::Result<()> {
    let host = cpal::default_host();
    let input = host.default_input_device().ok_or_else(|| anyhow::anyhow!("no microphone"))?;
    let output = host.default_output_device().ok_or_else(|| anyhow::anyhow!("no speaker"))?;
    let in_cfg = input.default_input_config()?;
    let out_cfg = output.default_output_config()?;
    if in_cfg.sample_format() != cpal::SampleFormat::F32 || out_cfg.sample_format() != cpal::SampleFormat::F32 {
        anyhow::bail!("unsupported sample format {:?}/{:?}", in_cfg.sample_format(), out_cfg.sample_format());
    }

    // Microphone -> mono -> 8 kHz frames -> the call.
    let mic = {
        let (call, channels, step) = (call.clone(), in_cfg.channels() as usize, in_cfg.sample_rate().0 as f64 / RATE);
        let (mut pos, mut prev, mut frame) = (0.0f64, 0.0f32, Vec::with_capacity(FRAME));
        input.build_input_stream(
            &in_cfg.config(),
            move |data: &[f32], _| {
                let mono: Vec<f32> = data.chunks(channels).map(|c| c.iter().sum::<f32>() / channels as f32).collect();
                while pos < mono.len() as f64 {
                    let i = pos.floor() as usize;
                    let f = (pos - i as f64) as f32;
                    let a = if i == 0 { prev } else { mono[i - 1] };
                    let v = a + (mono[i] - a) * f;
                    frame.push((v.clamp(-1.0, 1.0) * 32767.0) as i16);
                    if frame.len() == FRAME {
                        if call.muted.load(Ordering::Relaxed) {
                            frame.fill(0);
                        }
                        call.human_out.push(&frame);
                        frame.clear();
                    }
                    pos += step;
                }
                pos -= mono.len() as f64;
                prev = *mono.last().unwrap_or(&prev);
            },
            |e| tracing::warn!(error = %e, "Microphone stream error"),
            None,
        )?
    };

    // The call -> device rate -> every speaker channel. Keeps under 0.5 s of backlog.
    let speaker = {
        let (sub, mut frames) = call.inbound.subscribe();
        let channels = out_cfg.channels() as usize;
        let step = RATE / out_cfg.sample_rate().0 as f64;
        let (mut queue, mut pos) = (VecDeque::<f32>::new(), 0.0f64);
        let stream = output.build_output_stream(
            &out_cfg.config(),
            move |out: &mut [f32], _| {
                while let Ok(f) = frames.try_recv() {
                    queue.extend(f.iter().map(|&s| s as f32 / 32768.0));
                }
                if queue.len() > 4000 {
                    let drop = queue.len() - 1600;
                    queue.drain(..drop);
                }
                for frame in out.chunks_mut(channels) {
                    let v = if queue.len() < 2 {
                        0.0
                    } else {
                        let i = pos.floor() as usize;
                        let f = (pos - i as f64) as f32;
                        let v = queue[i] + (queue[(i + 1).min(queue.len() - 1)] - queue[i]) * f;
                        pos += step;
                        let used = (pos.floor() as usize).min(queue.len() - 1);
                        queue.drain(..used);
                        pos -= used as f64;
                        v
                    };
                    frame.fill(v);
                }
            },
            |e| tracing::warn!(error = %e, "Speaker stream error"),
            None,
        )?;
        (stream, sub)
    };

    mic.play()?;
    speaker.0.play()?;
    call.event("local audio connected");
    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(50));
    }
    call.inbound.unsubscribe(speaker.1);
    call.human_out.clear();
    Ok(())
}
