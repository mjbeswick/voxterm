//! Acoustic echo cancellation with WebRTC's AudioProcessing module.
//! The reference is what the speaker actually plays. The mic is cleaned against it at 16 kHz.

use crate::audio::Resampler;
use webrtc_audio_processing::Processor;
use webrtc_audio_processing_config::{Config, EchoCanceller};

const FRAME: usize = 160; // 10 ms at 16 kHz

pub struct Aec {
    proc: Option<Processor>,
    ref_rs: Resampler,
    cap_rs: Resampler,
    refs: Vec<f32>,
    caps: Vec<f32>,
}

// Only the audio input callback thread touches the processor after construction.
unsafe impl Send for Aec {}

impl Aec {
    /// `out_rate` and `in_rate` are the device rates. Falls back to pass-through if WebRTC fails to start.
    pub fn new(out_rate: u32, in_rate: u32) -> Self {
        let proc = Processor::new(16_000).ok().map(|p| {
            p.set_config(Config {
                echo_canceller: Some(EchoCanceller::default()),
                ..Default::default()
            });
            p
        });
        Self {
            proc,
            ref_rs: Resampler::new(out_rate, 16_000),
            cap_rs: Resampler::new(in_rate, 16_000),
            refs: Vec::new(),
            caps: Vec::new(),
        }
    }

    pub fn active(&self) -> bool {
        self.proc.is_some()
    }

    /// `reference`: mono device-rate samples the speaker played since the last call.
    /// `mic`: mono device-rate mic samples. Returns echo-reduced mic at 16 kHz.
    pub fn process(&mut self, reference: &[f32], mic: &[f32]) -> Vec<f32> {
        self.refs.extend(self.ref_rs.process(reference));
        self.caps.extend(self.cap_rs.process(mic));
        let mut out = Vec::with_capacity(self.caps.len());
        while self.caps.len() >= FRAME {
            let mut c: Vec<f32> = self.caps.drain(..FRAME).collect();
            if let Some(p) = &self.proc {
                // Feed all reference played so far, then clean this mic frame.
                while self.refs.len() >= FRAME {
                    let mut r: Vec<f32> = self.refs.drain(..FRAME).collect();
                    let _ = p.process_render_frame([r.as_mut_slice()]);
                }
                let _ = p.process_capture_frame([c.as_mut_slice()]);
            } else {
                self.refs.clear();
            }
            out.extend(c);
        }
        out
    }
}
