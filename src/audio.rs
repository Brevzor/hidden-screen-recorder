use crate::{config::AppConfig, debug_log, recorder::RecorderEvent};
use crate::{i18n::Message, msg};
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
        mpsc::Sender,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use wasapi::{DeviceEnumerator, Direction, SampleType, StreamMode, WaveFormat, initialize_mta};
use windows_capture::encoder::{VideoEncoder, VideoEncoderError};

pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: u32 = 2;
const FRAMES_PER_CHUNK: usize = 960;
const BYTES_PER_FRAME: usize = CHANNELS as usize * size_of::<f32>();
const QUEUE_CAPACITY: usize = 24;

struct ComGuard;
impl Drop for ComGuard {
    fn drop(&mut self) {
        wasapi::deinitialize();
    }
}

#[derive(Debug, Clone)]
pub struct MicrophoneDevice {
    pub id: String,
    pub name: String,
}

pub struct MicrophoneScan {
    pub devices: Vec<MicrophoneDevice>,
    pub warnings: Vec<String>,
}

pub fn enumerate_microphones() -> Result<MicrophoneScan, String> {
    initialize_mta().ok().map_err(|error| error.to_string())?;
    let _com = ComGuard;
    let enumerator = DeviceEnumerator::new().map_err(|e| e.to_string())?;
    let devices = enumerator
        .get_device_collection(&Direction::Capture)
        .map_err(|e| e.to_string())?;
    let count = devices.get_nbr_devices().map_err(|e| e.to_string())?;
    let entries = (0..count).map(|index| {
        let device = devices
            .get_device_at_index(index)
            .map_err(|e| e.to_string())?;
        Ok(MicrophoneDevice {
            id: device.get_id().map_err(|e| e.to_string())?,
            name: device.get_friendlyname().map_err(|e| e.to_string())?,
        })
    });
    Ok(collect_microphones(entries))
}

fn collect_microphones(
    entries: impl IntoIterator<Item = Result<MicrophoneDevice, String>>,
) -> MicrophoneScan {
    let mut result = MicrophoneScan {
        devices: Vec::new(),
        warnings: Vec::new(),
    };
    for entry in entries {
        match entry {
            Ok(device) => result.devices.push(device),
            Err(error) => result.warnings.push(error),
        }
    }
    result
}

#[derive(Clone, Copy)]
enum Source {
    System,
    Microphone,
}

#[derive(Default)]
struct SourceBuffer {
    chunks: VecDeque<(u64, Vec<u8>)>,
    enabled: bool,
    failed: bool,
    dropped: u64,
    missing: u64,
}

impl SourceBuffer {
    fn push(&mut self, index: u64, bytes: Vec<u8>) {
        if self.chunks.len() == QUEUE_CAPACITY {
            self.chunks.pop_front();
            self.dropped += 1;
        }
        self.chunks.push_back((index, bytes));
    }

    fn take(&mut self, index: u64) -> Vec<u8> {
        while self.chunks.front().is_some_and(|(i, _)| *i < index) {
            self.chunks.pop_front();
        }
        if self.chunks.front().is_some_and(|(i, _)| *i == index) {
            return self.chunks.pop_front().unwrap().1;
        }
        if self.enabled && !self.failed {
            self.missing += 1;
        }
        vec![0; FRAMES_PER_CHUNK * BYTES_PER_FRAME]
    }
}

pub struct AudioPipeline {
    buffers: [Arc<Mutex<SourceBuffer>>; 2],
    volumes: [f32; 2],
    clock: Arc<OnceLock<Instant>>,
    next_chunk: u64,
    handles: Vec<JoinHandle<()>>,
    stop: Arc<AtomicBool>,
    events: Sender<RecorderEvent>,
    warned: bool,
}

impl AudioPipeline {
    pub fn start(
        config: &AppConfig,
        stop: Arc<AtomicBool>,
        events: Sender<RecorderEvent>,
    ) -> Result<Option<Self>, Message> {
        if !config.system_audio && !config.microphone {
            return Ok(None);
        }
        let mut pipeline = Self {
            buffers: [config.system_audio, config.microphone].map(|enabled| {
                Arc::new(Mutex::new(SourceBuffer {
                    enabled,
                    ..Default::default()
                }))
            }),
            volumes: [config.system_volume, config.microphone_volume],
            clock: Arc::new(OnceLock::new()),
            next_chunk: 0,
            handles: Vec::new(),
            stop,
            events,
            warned: false,
        };
        for (index, source) in [Source::System, Source::Microphone].into_iter().enumerate() {
            if !pipeline.buffers[index]
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .enabled
            {
                continue;
            }
            let buffer = pipeline.buffers[index].clone();
            let clock = pipeline.clock.clone();
            let stop = pipeline.stop.clone();
            let events = pipeline.events.clone();
            let device = if index == 1 {
                config.microphone_device.clone()
            } else {
                None
            };
            let label = if index == 0 {
                msg!("System audio", "Системный звук")
            } else {
                msg!("Microphone", "Микрофон")
            };
            let thread_label = label.clone();
            let handle = thread::Builder::new()
                .name(format!("wasapi-{index}"))
                .spawn(move || {
                    if let Err(error) =
                        capture_loop(source, device.as_deref(), buffer.clone(), clock, stop)
                    {
                        // Status is independent of the bounded sample queue.
                        buffer.lock().unwrap_or_else(|e| e.into_inner()).failed = true;
                        let _ =
                            events.send(RecorderEvent::AudioWarning(thread_label.detail(error)));
                    }
                })
                .map_err(|error| label.detail(error))?;
            pipeline.handles.push(handle);
        }
        Ok(Some(pipeline))
    }

    pub fn begin(&self) {
        let _ = self.clock.set(Instant::now());
    }

    pub fn send_pending(
        &mut self,
        encoder: &mut VideoEncoder,
        finalizing: bool,
    ) -> Result<(), VideoEncoderError> {
        let Some(origin) = self.clock.get() else {
            return Ok(());
        };
        let elapsed = origin.elapsed();
        // A small scheduling allowance lets both sources deliver the same block.
        let duration = if finalizing {
            elapsed
        } else {
            elapsed.saturating_sub(Duration::from_millis(40))
        };
        let target = duration.as_millis() as u64 / 20;
        while self.next_chunk < target {
            let output = self.mix_chunk(self.next_chunk);
            encoder.send_audio_buffer(&output, 0)?;
            self.next_chunk += 1;
        }
        let (dropped, missing) = self.buffers.iter().fold((0, 0), |(d, m), buffer| {
            let buffer = buffer.lock().unwrap_or_else(|e| e.into_inner());
            (d + buffer.dropped, m + buffer.missing)
        });
        if (dropped > 0 || missing > 0) && !self.warned {
            let _ = self.events.send(RecorderEvent::AudioWarning(msg!("Audio is falling behind: missing sections were filled with silence to preserve duration.", "Аудио не успевает за записью: пропуски заполнены тишиной с сохранением длительности.")));
            self.warned = true;
        }
        if finalizing {
            debug_log::write(format!(
                "audio: evicted={dropped}, missing={missing}, chunks={}",
                self.next_chunk
            ));
        }
        Ok(())
    }

    fn mix_chunk(&mut self, index: u64) -> Vec<u8> {
        let a = self.buffers[0]
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take(index);
        let b = self.buffers[1]
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take(index);
        mix_f32(&a, &b, self.volumes[0], self.volumes[1])
    }

    pub fn stop_and_join(&mut self) {
        self.stop.store(true, Ordering::Release);
        for handle in self.handles.drain(..) {
            if handle.join().is_err() {
                let _ = self.events.send(RecorderEvent::AudioWarning(msg!(
                    "An audio thread crashed.",
                    "Поток аудио завершился аварийно."
                )));
            }
        }
    }
}

impl Drop for AudioPipeline {
    fn drop(&mut self) {
        self.stop_and_join();
    }
}

fn capture_loop(
    source: Source,
    device_id: Option<&str>,
    buffer: Arc<Mutex<SourceBuffer>>,
    clock: Arc<OnceLock<Instant>>,
    stop: Arc<AtomicBool>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    initialize_mta().ok()?;
    let _com = ComGuard;
    let direction = match source {
        Source::System => Direction::Render,
        Source::Microphone => Direction::Capture,
    };
    let enumerator = DeviceEnumerator::new()?;
    let device = match device_id {
        Some(id) => enumerator.get_device(id)?,
        None => enumerator.get_default_device(&direction)?,
    };
    let mut client = device.get_iaudioclient()?;
    let format = WaveFormat::new(
        32,
        32,
        &SampleType::Float,
        SAMPLE_RATE as usize,
        CHANNELS as usize,
        None,
    );
    let (_, minimum_period) = client.get_device_period()?;
    let mode = StreamMode::EventsShared {
        autoconvert: true,
        buffer_duration_hns: minimum_period,
    };

    // Direction::Capture on a render endpoint enables loopback. No device is
    // replaced, no driver is installed, and Windows Audio is never restarted.
    client.initialize_client(&format, &Direction::Capture, &mode)?;
    let event = client.set_get_eventhandle()?;
    let capture = client.get_audiocaptureclient()?;
    let mut queue = VecDeque::with_capacity(FRAMES_PER_CHUNK * BYTES_PER_FRAME * 4);
    while clock.get().is_none() {
        if stop.load(Ordering::Acquire) {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(2));
    }
    let origin = *clock.get().unwrap();
    client.start_stream()?;
    let chunk_duration = Duration::from_millis(20);
    let mut next_chunk = origin + chunk_duration;
    let mut index = 0;

    while !stop.load(Ordering::Acquire) {
        capture.read_from_device_to_deque(&mut queue)?;
        let now = Instant::now();
        while now >= next_chunk {
            let mut chunk = vec![0; FRAMES_PER_CHUNK * BYTES_PER_FRAME];
            for byte in &mut chunk {
                *byte = queue.pop_front().unwrap_or(0);
            }
            next_chunk += chunk_duration;
            buffer
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(index, chunk);
            index += 1;
        }
        let _ = event.wait_for_event(10);
    }

    let _ = client.stop_stream();
    Ok(())
}

fn mix_f32(a: &[u8], b: &[u8], a_volume: f32, b_volume: f32) -> Vec<u8> {
    a.as_chunks::<4>()
        .0
        .iter()
        .zip(b.as_chunks::<4>().0.iter())
        .flat_map(|(a, b)| {
            let a = f32::from_le_bytes(*a) * a_volume;
            let b = f32::from_le_bytes(*b) * b_volume;
            float_to_i16(a + b).to_le_bytes()
        })
        .collect()
}

fn float_to_i16(value: f32) -> i16 {
    (value.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overflow_preserves_chunk_indices_and_fills_gaps_with_silence() {
        let mut buffer = SourceBuffer {
            enabled: true,
            ..Default::default()
        };
        let chunk = 0.25_f32
            .to_le_bytes()
            .repeat(FRAMES_PER_CHUNK * CHANNELS as usize);
        for index in 0..30 {
            buffer.push(index, chunk.clone());
        }
        assert_eq!(buffer.chunks.len(), QUEUE_CAPACITY);
        assert_eq!(buffer.dropped, 6);
        let output: Vec<_> = (0..30).map(|index| buffer.take(index)).collect();
        assert_eq!(
            output.iter().map(Vec::len).sum::<usize>(),
            30 * FRAMES_PER_CHUNK * BYTES_PER_FRAME
        );
        assert!(output[..6].iter().flatten().all(|byte| *byte == 0));
        assert_eq!(output[6], chunk);
    }

    #[test]
    fn failed_source_does_not_block_healthy_source_even_with_full_queue() {
        let mut failed = SourceBuffer {
            enabled: true,
            failed: true,
            ..Default::default()
        };
        for index in 0..24 {
            failed.push(index, vec![0; FRAMES_PER_CHUNK * BYTES_PER_FRAME]);
        }
        let mut healthy = SourceBuffer {
            enabled: true,
            ..Default::default()
        };
        healthy.push(
            24,
            0.5_f32
                .to_le_bytes()
                .repeat(FRAMES_PER_CHUNK * CHANNELS as usize),
        );
        let mixed = mix_f32(&failed.take(24), &healthy.take(24), 1.0, 1.0);
        assert_eq!(i16::from_le_bytes([mixed[0], mixed[1]]), 16384);
        assert_eq!(failed.missing, 0);
    }

    #[test]
    fn bad_microphone_does_not_discard_other_devices() {
        let scan = collect_microphones([
            Ok(MicrophoneDevice {
                id: "one".into(),
                name: "First".into(),
            }),
            Err("disconnected".into()),
            Ok(MicrophoneDevice {
                id: "two".into(),
                name: "Second".into(),
            }),
        ]);
        assert_eq!(scan.devices.len(), 2);
        assert_eq!(scan.warnings, ["disconnected"]);
    }

    #[test]
    fn mixes_and_clamps_float_samples() {
        let a = 0.75_f32.to_le_bytes();
        let b = 0.75_f32.to_le_bytes();
        let mixed = mix_f32(&a, &b, 1.0, 1.0);
        assert_eq!(i16::from_le_bytes(mixed.try_into().unwrap()), i16::MAX);
    }
}
