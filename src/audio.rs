//! Audio thread. cpal streams are not Send, so one thread owns them all.
//! Mic: device rate f32 -> 16 kHz i16 chunks to Live. Dropping the stream releases the mic.
//! Speaker: 24 kHz i16 from Live -> device rate f32, queued and flushed on interrupt.

use crate::live::LiveCmd;
use anyhow::{Result, anyhow};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex, mpsc},
    time::{Duration, Instant},
};
use tokio::sync::mpsc::UnboundedSender;

pub enum AudioCmd {
    Mic(bool),
    Play(Vec<i16>),
    Flush,
}

#[derive(Default)]
struct Shared {
    queue: VecDeque<f32>,
    last_active: Option<Instant>,
}

pub fn rms(x: &[f32]) -> f32 {
    if x.is_empty() {
        return 0.0;
    }
    (x.iter().map(|s| s * s).sum::<f32>() / x.len() as f32).sqrt()
}

/// Streaming linear resampler. Keeps state across chunks.
pub struct Resampler {
    step: f64,
    pos: f64,
    prev: f32,
}

impl Resampler {
    pub fn new(from: u32, to: u32) -> Self {
        // ponytail: linear interpolation, skipped: sinc filter, add when speech artifacts are audible.
        Self {
            step: from as f64 / to as f64,
            pos: 0.0,
            prev: 0.0,
        }
    }

    pub fn process(&mut self, input: &[f32]) -> Vec<f32> {
        let n = input.len() as isize;
        let prev = self.prev;
        let mut out = Vec::new();
        let mut pos = self.pos;
        while (pos.floor() as isize) + 1 < n {
            let i = pos.floor() as isize;
            let f = (pos - i as f64) as f32;
            let a = if i < 0 { prev } else { input[i as usize] };
            let b = input[(i + 1) as usize];
            out.push(a + (b - a) * f);
            pos += self.step;
        }
        self.pos = pos - n as f64;
        if n > 0 {
            self.prev = input[(n - 1) as usize];
        }
        out
    }
}

pub fn spawn(
    live: UnboundedSender<LiveCmd>,
    errs: UnboundedSender<String>,
    gate: Option<Duration>,
    min_rms: f32,
) -> mpsc::Sender<AudioCmd> {
    let (tx, rx) = mpsc::channel::<AudioCmd>();
    std::thread::spawn(move || {
        let host = cpal::default_host();
        let shared = Arc::new(Mutex::new(Shared::default()));
        let (_out, out_rate) = match output(&host, shared.clone(), errs.clone()) {
            Ok(v) => v,
            Err(e) => {
                let _ = errs.send(format!("speaker: {e}"));
                return;
            }
        };
        let mut play_rs = Resampler::new(24_000, out_rate);
        let mut mic: Option<cpal::Stream> = None;
        while let Ok(cmd) = rx.recv() {
            match cmd {
                AudioCmd::Mic(true) => {
                    if mic.is_none() {
                        match input(
                            &host,
                            live.clone(),
                            errs.clone(),
                            shared.clone(),
                            gate,
                            min_rms,
                        ) {
                            Ok(s) => {
                                crate::dbg("audio: mic stream open");
                                mic = Some(s)
                            }
                            Err(e) => {
                                let _ = errs.send(format!("mic: {e}"));
                            }
                        }
                    }
                }
                AudioCmd::Mic(false) => {
                    crate::dbg("audio: mic stream dropped");
                    mic = None
                }
                AudioCmd::Play(samples) => {
                    let f: Vec<f32> = samples.iter().map(|x| *x as f32 / 32768.0).collect();
                    crate::dbg(format!("audio: play {} samples", f.len()));
                    let out = play_rs.process(&f);
                    shared.lock().unwrap().queue.extend(out);
                }
                AudioCmd::Flush => shared.lock().unwrap().queue.clear(),
            }
        }
    });
    tx
}

fn output(
    host: &cpal::Host,
    shared: Arc<Mutex<Shared>>,
    errs: UnboundedSender<String>,
) -> Result<(cpal::Stream, u32)> {
    let dev = host
        .default_output_device()
        .ok_or_else(|| anyhow!("no output device"))?;
    let cfg = dev.default_output_config()?;
    if cfg.sample_format() != cpal::SampleFormat::F32 {
        return Err(anyhow!(
            "unsupported output format {:?}",
            cfg.sample_format()
        ));
    }
    let rate = cfg.sample_rate();
    let ch = cfg.channels() as usize;
    let stream = dev.build_output_stream(
        cfg.config(),
        move |data: &mut [f32], _| {
            let mut sh = shared.lock().unwrap();
            for frame in data.chunks_mut(ch) {
                let s = match sh.queue.pop_front() {
                    Some(s) => {
                        sh.last_active = Some(Instant::now());
                        s
                    }
                    None => 0.0,
                };
                frame.iter_mut().for_each(|c| *c = s);
            }
        },
        move |e| {
            let _ = errs.send(format!("speaker: {e}"));
        },
        None,
    )?;
    stream.play()?;
    crate::dbg(format!("audio: speaker open at {rate} Hz, {ch} ch"));
    Ok((stream, rate))
}

fn input(
    host: &cpal::Host,
    live: UnboundedSender<LiveCmd>,
    errs: UnboundedSender<String>,
    shared: Arc<Mutex<Shared>>,
    gate: Option<Duration>,
    min_rms: f32,
) -> Result<cpal::Stream> {
    let dev = host
        .default_input_device()
        .ok_or_else(|| anyhow!("no input device"))?;
    let cfg = dev.default_input_config()?;
    if cfg.sample_format() != cpal::SampleFormat::F32 {
        return Err(anyhow!(
            "unsupported input format {:?}",
            cfg.sample_format()
        ));
    }
    let ch = cfg.channels() as usize;
    let mut rs = Resampler::new(cfg.sample_rate(), 16_000);
    let mut buf: Vec<i16> = Vec::new();
    let mut silent_since: Option<Instant> = None;
    let mut warned = false;
    let errs_e = errs.clone();
    let stream = dev.build_input_stream(
        cfg.config(),
        move |data: &[f32], _| {
            let mono: Vec<f32> = data
                .chunks(ch)
                .map(|f| f.iter().sum::<f32>() / ch as f32)
                .collect();
            if mono.iter().all(|s| *s == 0.0) {
                let since = *silent_since.get_or_insert_with(Instant::now);
                if !warned && since.elapsed() > Duration::from_secs(3) {
                    warned = true;
                    let _ = errs.send(
                        "mic is silent: allow microphone access for your terminal app".into(),
                    );
                }
            } else {
                silent_since = None;
                warned = false;
            }
            let out = rs.process(&mono);
            // Echo gate: while the assistant speaks, do not send the speaker's sound back as input.
            let busy = {
                let sh = shared.lock().unwrap();
                !sh.queue.is_empty()
                    || gate.is_some_and(|g| sh.last_active.is_some_and(|t| t.elapsed() < g))
            };
            if busy {
                buf.clear();
                return;
            }
            // Mic level gate: drop chunks quieter than mic_min_rms. Tune with VOXTERM_LOG output.
            let level = rms(&out);
            crate::dbg(format!("mic rms {level:.4}"));
            if level < min_rms {
                buf.clear();
                return;
            }
            buf.extend(out.iter().map(|s| (s.clamp(-1.0, 1.0) * 32767.0) as i16));
            if buf.len() >= 1600 {
                let _ = live.send(LiveCmd::Audio(std::mem::take(&mut buf)));
            }
        },
        move |e| {
            let _ = errs_e.send(format!("mic: {e}"));
        },
        None,
    )?;
    stream.play()?;
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rms_of_constant_and_silence() {
        assert!((rms(&[0.5; 100]) - 0.5).abs() < 1e-6);
        assert_eq!(rms(&[]), 0.0);
    }

    #[test]
    fn resample_48k_to_16k_length_and_level() {
        let mut r = Resampler::new(48_000, 16_000);
        let a = r.process(&vec![0.5; 4800]);
        let b = r.process(&vec![0.5; 4800]);
        let total = a.len() + b.len();
        assert!((3198..=3202).contains(&total), "got {total}");
        assert!(a.iter().chain(b.iter()).all(|s| (s - 0.5).abs() < 1e-6));
    }
}
