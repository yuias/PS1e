//! Audio output: a cpal stream fed from a shared sample queue.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

pub type SampleQueue = Arc<Mutex<VecDeque<i16>>>;

pub struct Audio {
    // Held so the stream keeps playing; dropped with the app.
    _stream: cpal::Stream,
    pub queue: SampleQueue,
}

impl Audio {
    /// Open the default output device. Returns None (with a log) when no
    /// device is available so the emulator still runs silent.
    pub fn new() -> Option<Self> {
        let host = cpal::default_host();
        let device = host.default_output_device()?;
        let config = device.default_output_config().ok()?;
        let sample_rate = config.sample_rate().0;
        let channels = config.channels() as usize;
        let queue: SampleQueue = Arc::new(Mutex::new(VecDeque::new()));
        let q = queue.clone();

        // The SPU produces 44100 Hz stereo; resample by simple duplication
        // ratio if the device rate differs (good enough until a proper
        // resampler is warranted).
        let step = 44_100.0 / sample_rate as f64;
        let mut pos = 0.0f64;
        let mut last = (0i16, 0i16);

        let stream = device
            .build_output_stream(
                &config.into(),
                move |data: &mut [f32], _| {
                    let mut q = q.lock().unwrap();
                    for frame in data.chunks_mut(channels) {
                        pos += step;
                        while pos >= 1.0 {
                            pos -= 1.0;
                            if q.len() >= 2 {
                                last = (q.pop_front().unwrap(), q.pop_front().unwrap());
                            }
                        }
                        let l = last.0 as f32 / 32768.0;
                        let r = last.1 as f32 / 32768.0;
                        for (i, s) in frame.iter_mut().enumerate() {
                            *s = if i % 2 == 0 { l } else { r };
                        }
                    }
                },
                |e| tracing::warn!("audio stream error: {e}"),
                None,
            )
            .ok()?;
        stream.play().ok()?;
        tracing::info!("audio output at {sample_rate} Hz, {channels} ch");
        Some(Self {
            _stream: stream,
            queue,
        })
    }

    /// Queued stereo frames waiting to be played.
    pub fn buffered_frames(&self) -> usize {
        self.queue.lock().unwrap().len() / 2
    }

    pub fn push_samples(&self, samples: &[i16]) {
        let mut q = self.queue.lock().unwrap();
        // Cap ~500ms so a paused UI doesn't accumulate unbounded latency
        const CAP: usize = 44_100;
        q.extend(samples.iter().copied());
        while q.len() > CAP * 2 {
            q.pop_front();
        }
    }
}
