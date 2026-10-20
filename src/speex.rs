//! Minimal owned bindings to the bundled SpeexDSP 1.2.1 preprocessor.
use std::{ffi::c_void, ptr::NonNull};

use crate::audio::{CHANNELS, SAMPLE_RATE};

unsafe extern "C" {
    fn speex_preprocess_state_init(frame_size: i32, sampling_rate: i32) -> *mut c_void;
    fn speex_preprocess_state_destroy(state: *mut c_void);
    fn speex_preprocess_ctl(state: *mut c_void, request: i32, value: *mut c_void) -> i32;
    fn speex_preprocess_run(state: *mut c_void, samples: *mut i16) -> i32;
}

struct State(NonNull<c_void>);

// A state owns all its C allocations. Moving it to the audio worker is safe;
// it is never shared and all processing requires an exclusive mutable borrow.
unsafe impl Send for State {}

impl Drop for State {
    fn drop(&mut self) {
        unsafe { speex_preprocess_state_destroy(self.0.as_ptr()) };
    }
}

pub struct SpeexDenoiser {
    channels: Vec<State>,
}

impl SpeexDenoiser {
    pub const FRAMES: usize = 480;

    pub fn new(level_db: i32) -> std::io::Result<Self> {
        let mut denoiser = Self {
            channels: Vec::new(),
        };
        for _ in 0..CHANNELS {
            let state = NonNull::new(unsafe {
                speex_preprocess_state_init(Self::FRAMES as i32, SAMPLE_RATE as i32)
            })
            .ok_or_else(|| std::io::Error::other("SpeexDSP allocation failed"))?;
            let state = State(state);
            // Denoise on; automatic gain and voice activity gating off. These
            // are separate from the user's volume and microphone threshold.
            // VAD is disabled by default in SpeexDSP 1.2.1.
            for (request, mut value) in [(0, 1_i32), (2, 0), (18, level_db.clamp(-60, 0))] {
                let result = unsafe {
                    speex_preprocess_ctl(state.0.as_ptr(), request, (&mut value as *mut i32).cast())
                };
                if result != 0 {
                    return Err(std::io::Error::other("SpeexDSP configuration failed"));
                }
            }
            denoiser.channels.push(state);
        }
        Ok(denoiser)
    }

    pub fn process(&mut self, data: &[u8]) -> Vec<u8> {
        const STRIDE: usize = CHANNELS as usize * 4;
        let mut output = vec![0; Self::FRAMES * STRIDE];
        for (channel, state) in self.channels.iter_mut().enumerate() {
            let mut samples = [0_i16; Self::FRAMES];
            for (index, frame) in data.as_chunks::<STRIDE>().0.iter().enumerate() {
                let offset = channel * 4;
                let value = f32::from_le_bytes(frame[offset..offset + 4].try_into().unwrap());
                samples[index] = (value.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16;
            }
            unsafe { speex_preprocess_run(state.0.as_ptr(), samples.as_mut_ptr()) };
            for (index, sample) in samples.into_iter().enumerate() {
                let offset = index * STRIDE + channel * 4;
                output[offset..offset + 4]
                    .copy_from_slice(&(sample as f32 / 32768.0).to_le_bytes());
            }
        }
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stronger_setting_reduces_more_noise_without_crossing_channels() {
        let mut mild = SpeexDenoiser::new(-10).unwrap();
        let mut strong = SpeexDenoiser::new(-45).unwrap();
        let mut seed = 17_u32;
        let (mut mild_energy, mut strong_energy) = (0.0_f64, 0.0_f64);
        for block in 0..300 {
            let mut data = Vec::new();
            for _ in 0..SpeexDenoiser::FRAMES {
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                let noise = ((seed >> 8) as f32 / 16_777_215.0 - 0.5) * 0.04;
                data.extend_from_slice(&noise.to_le_bytes());
                data.extend_from_slice(&0.0_f32.to_le_bytes());
            }
            let a = mild.process(&data);
            let b = strong.process(&data);
            for (a, b) in a.as_chunks::<8>().0.iter().zip(b.as_chunks::<8>().0) {
                let a = f32::from_le_bytes(a[..4].try_into().unwrap());
                let left = f32::from_le_bytes(b[..4].try_into().unwrap());
                let right = f32::from_le_bytes(b[4..].try_into().unwrap());
                assert!(left.is_finite() && left.abs() <= 1.0);
                assert_eq!(right, 0.0);
                if block > 100 {
                    mild_energy += f64::from(a).powi(2);
                    strong_energy += f64::from(left).powi(2);
                }
            }
        }
        assert!(
            strong_energy < mild_energy * 0.25,
            "suppression setting should matter: strong={strong_energy}, mild={mild_energy}"
        );
    }
}
