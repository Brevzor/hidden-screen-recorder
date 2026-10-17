use crate::recorder::SharedEncoder;
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
use wasapi::{BufferInfo, DeviceEnumerator, Direction, SampleType, WaveFormat, initialize_mta};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::{
    Win32::{
        Foundation::{CloseHandle, HANDLE},
        Media::Audio::{
            AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
            AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK,
            AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, IAudioCaptureClient, IAudioClient,
            IMMDeviceEnumerator, MMDeviceEnumerator, eCapture, eConsole, eRender,
        },
        System::{
            Com::{CLSCTX_ALL, CoCreateInstance},
            Threading::{
                AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, CreateEventW,
                WaitForSingleObject,
            },
        },
    },
    core::{HSTRING, w},
};

pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: u32 = 2;
const FRAMES_PER_CHUNK: usize = 960;
const BYTES_PER_FRAME: usize = CHANNELS as usize * size_of::<f32>();
const MAX_BUFFER_FRAMES: usize = SAMPLE_RATE as usize * 2;
const DEFAULT_MIX_LATENCY: Duration = Duration::from_millis(250);
const TIMESTAMP_SMOOTHING_FRAMES: u64 = SAMPLE_RATE as u64 * 70 / 1000;
const LOSS_WARNING_FRAMES: u64 = SAMPLE_RATE as u64 / 10;

struct ComGuard;
impl Drop for ComGuard {
    fn drop(&mut self) {
        wasapi::deinitialize();
    }
}

struct AudioScheduling(Option<HANDLE>);

impl AudioScheduling {
    fn enter() -> Self {
        let mut task = 0;
        match unsafe { AvSetMmThreadCharacteristicsW(w!("Audio"), &mut task) } {
            Ok(handle) => Self(Some(handle)),
            Err(error) => {
                debug_log::write(format!("MMCSS audio scheduling unavailable: {error}"));
                Self(None)
            }
        }
    }
}

impl Drop for AudioScheduling {
    fn drop(&mut self) {
        if let Some(handle) = self.0 {
            let _ = unsafe { AvRevertMmThreadCharacteristics(handle) };
        }
    }
}

struct AudioEvent(HANDLE);
impl Drop for AudioEvent {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
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

struct AudioPacket {
    start: u64,
    data: Vec<u8>,
}

#[derive(Default)]
struct PacketTimeline {
    next: Option<u64>,
}

impl PacketTimeline {
    fn place(&mut self, timestamp: u64, frames: usize, discontinuity: bool) -> u64 {
        // Follow sample counts between packets, as OBS does for small timestamp
        // variations. Placing every packet at its raw QPC timestamp creates
        // repeated holes/overlaps even when all samples arrived successfully.
        let start = match self.next {
            Some(next)
                if !discontinuity && next.abs_diff(timestamp) < TIMESTAMP_SMOOTHING_FRAMES =>
            {
                next
            }
            _ => timestamp,
        };
        self.next = Some(start + frames as u64);
        start
    }
}

impl AudioPacket {
    fn frames(&self) -> usize {
        self.data.len() / BYTES_PER_FRAME
    }
    fn end(&self) -> u64 {
        self.start + self.frames() as u64
    }
}

#[derive(Default)]
struct SourceBuffer {
    packets: VecDeque<AudioPacket>,
    queued_frames: usize,
    consumed: u64,
    enabled: bool,
    ready: bool,
    failed: bool,
    dropped_frames: u64,
    late_frames: u64,
    discontinuities: u64,
    received_packets: u64,
    received_frames: u64,
}

impl SourceBuffer {
    fn push(&mut self, mut packet: AudioPacket) {
        self.received_packets += 1;
        self.received_frames += packet.frames() as u64;
        // Count discarded samples, never an absence of packets: WASAPI loopback
        // legitimately produces no packets while the output device is silent.
        if packet.start < self.consumed {
            let late = (self.consumed - packet.start).min(packet.frames() as u64) as usize;
            self.late_frames += late as u64;
            packet.data.drain(..late * BYTES_PER_FRAME);
            packet.start += late as u64;
        }
        if packet.data.is_empty() {
            return;
        }
        self.queued_frames += packet.frames();
        self.packets.push_back(packet);
        while self.queued_frames > MAX_BUFFER_FRAMES {
            let packet = self.packets.pop_front().unwrap();
            self.queued_frames -= packet.frames();
            self.dropped_frames += packet.frames() as u64;
        }
    }

    fn take(&mut self, start: u64, frames: usize) -> Vec<u8> {
        let end = start + frames as u64;
        let mut output = vec![0; frames * BYTES_PER_FRAME];
        for packet in &self.packets {
            let from = start.max(packet.start);
            let to = end.min(packet.end());
            if from < to {
                let source = (from - packet.start) as usize * BYTES_PER_FRAME;
                let target = (from - start) as usize * BYTES_PER_FRAME;
                let length = (to - from) as usize * BYTES_PER_FRAME;
                output[target..target + length]
                    .copy_from_slice(&packet.data[source..source + length]);
            }
        }
        while self
            .packets
            .front()
            .is_some_and(|packet| packet.end() <= end)
        {
            self.queued_frames -= self.packets.pop_front().unwrap().frames();
        }
        self.consumed = end;
        output
    }
}

struct ClockStart {
    instant: Instant,
    qpc_100ns: u64,
}

struct AudioClock {
    start: OnceLock<ClockStart>,
    end: OnceLock<Instant>,
    latency: Duration,
}

impl Default for AudioClock {
    fn default() -> Self {
        Self {
            start: OnceLock::new(),
            end: OnceLock::new(),
            latency: DEFAULT_MIX_LATENCY,
        }
    }
}

impl AudioClock {
    fn stop(&self) {
        self.end.get_or_init(Instant::now);
    }

    fn frames(&self, finalizing: bool) -> u64 {
        let Some(start) = self.start.get() else {
            return 0;
        };
        let until = self.end.get().copied().unwrap_or_else(Instant::now);
        let duration = until.saturating_duration_since(start.instant);
        let duration = if finalizing {
            duration
        } else {
            duration.saturating_sub(self.latency)
        };
        let frames = (duration.as_nanos() * u128::from(SAMPLE_RATE) / 1_000_000_000) as u64;
        if finalizing {
            frames
        } else {
            frames / FRAMES_PER_CHUNK as u64 * FRAMES_PER_CHUNK as u64
        }
    }
}

fn qpc_100ns() -> Result<u64, windows::core::Error> {
    let mut counter = 0;
    let mut frequency = 0;
    unsafe {
        QueryPerformanceFrequency(&mut frequency)?;
        QueryPerformanceCounter(&mut counter)?;
    }
    Ok((counter as u128 * 10_000_000 / frequency as u128) as u64)
}

type SourceBuffers = [Arc<Mutex<SourceBuffer>>; 2];

struct AudioMixer {
    buffers: SourceBuffers,
    volumes: [f32; 2],
    position: u64,
    events: Sender<RecorderEvent>,
    warned: bool,
}

impl AudioMixer {
    fn drain_to(
        &mut self,
        target: u64,
        send: &mut impl FnMut(&[u8]) -> Result<(), String>,
    ) -> Result<(), String> {
        while self.position < target {
            let frames = (target - self.position).min(FRAMES_PER_CHUNK as u64) as usize;
            let a = self.buffers[0]
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take(self.position, frames);
            let b = self.buffers[1]
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take(self.position, frames);
            send(&mix_f32(&a, &b, self.volumes[0], self.volumes[1]))?;
            self.position += frames as u64;
        }
        self.report(false);
        Ok(())
    }

    fn report(&mut self, finalizing: bool) {
        let (dropped, late, discontinuities) =
            self.buffers.iter().fold((0, 0, 0), |(d, l, g), buffer| {
                let buffer = buffer.lock().unwrap_or_else(|e| e.into_inner());
                (
                    d + buffer.dropped_frames,
                    l + buffer.late_frames,
                    g + buffer.discontinuities,
                )
            });
        // Do not turn a single scheduling hiccup into a permanent UI warning.
        // Detailed frame counters remain available in the diagnostic log.
        if dropped + late >= LOSS_WARNING_FRAMES && !self.warned {
            let _ = self.events.send(RecorderEvent::AudioWarning(msg!(
                "Audio dropouts detected",
                "Обнаружены пропуски звука"
            )));
            self.warned = true;
        }
        if finalizing {
            debug_log::write(format!(
                "audio: dropped_frames={dropped}, late_frames={late}, device_discontinuities={discontinuities}, encoded_frames={}",
                self.position
            ));
        }
    }
}

pub struct AudioPipeline {
    buffers: SourceBuffers,
    volumes: [f32; 2],
    clock: Arc<AudioClock>,
    handles: Vec<JoinHandle<()>>,
    writer: Option<JoinHandle<Result<(), String>>>,
    sources_done: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    events: Sender<RecorderEvent>,
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
            clock: Arc::new(AudioClock {
                latency: Duration::from_millis(u64::from(config.audio_buffer_ms.clamp(100, 1000))),
                ..Default::default()
            }),
            handles: Vec::new(),
            writer: None,
            sources_done: Arc::new(AtomicBool::new(false)),
            stop,
            events,
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
            let device_timestamps = config.audio_device_timestamps;
            let handle = thread::Builder::new()
                .name(format!("wasapi-{index}"))
                .spawn(move || {
                    if let Err(error) = capture_loop(
                        source,
                        device.as_deref(),
                        buffer.clone(),
                        clock,
                        stop,
                        device_timestamps,
                    ) {
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

    pub fn begin(&mut self, encoder: SharedEncoder) -> std::io::Result<()> {
        self.begin_with_sink(move |data| {
            encoder
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_mut()
                .ok_or_else(|| "Encoder already finalized".to_owned())?
                .send_audio_buffer(data, 0)
                .map_err(|e| e.to_string())
        })
    }

    fn begin_with_sink(
        &mut self,
        mut send: impl FnMut(&[u8]) -> Result<(), String> + Send + 'static,
    ) -> std::io::Result<()> {
        // Device initialization must not consume the first blocks of the recording.
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline && !self.stop.load(Ordering::Acquire) {
            if self.buffers.iter().all(|buffer| {
                let buffer = buffer.lock().unwrap_or_else(|e| e.into_inner());
                !buffer.enabled || buffer.ready || buffer.failed
            }) {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        let clock = self.clock.clone();
        let stop = self.stop.clone();
        let done = self.sources_done.clone();
        let mut mixer = AudioMixer {
            buffers: self.buffers.clone(),
            volumes: self.volumes,
            position: 0,
            events: self.events.clone(),
            warned: false,
        };
        let qpc = qpc_100ns().map_err(std::io::Error::other)?;
        self.clock
            .start
            .set(ClockStart {
                instant: Instant::now(),
                qpc_100ns: qpc,
            })
            .map_err(|_| std::io::Error::other("Audio already started"))?;
        self.writer = Some(thread::Builder::new().name("audio-encoder".into()).spawn(
            move || {
                let result = (|| {
                    initialize_mta().ok().map_err(|e| e.to_string())?;
                    let _com = ComGuard;
                    let _scheduling = AudioScheduling::enter();
                    while !stop.load(Ordering::Acquire) {
                        mixer.drain_to(clock.frames(false), &mut send)?;
                        thread::sleep(Duration::from_millis(5));
                    }
                    clock.stop();
                    // Capture threads finish before the final partial block is read.
                    while !done.load(Ordering::Acquire) {
                        thread::sleep(Duration::from_millis(2));
                    }
                    mixer.drain_to(clock.frames(true), &mut send)?;
                    mixer.report(true);
                    Ok(())
                })();
                if result.is_err() {
                    clock.stop();
                    stop.store(true, Ordering::Release);
                }
                result
            },
        )?);
        Ok(())
    }

    pub fn stop_and_join(&mut self) -> Result<(), String> {
        self.clock.stop();
        self.stop.store(true, Ordering::Release);
        for handle in self.handles.drain(..) {
            if handle.join().is_err() {
                let _ = self.events.send(RecorderEvent::AudioWarning(msg!(
                    "An audio thread crashed.",
                    "Поток аудио завершился аварийно."
                )));
            }
        }
        self.sources_done.store(true, Ordering::Release);
        if let Some(writer) = self.writer.take() {
            writer
                .join()
                .map_err(|_| "Audio encoder thread crashed".to_owned())??;
        }
        Ok(())
    }
}

impl Drop for AudioPipeline {
    fn drop(&mut self) {
        let _ = self.stop_and_join();
    }
}

fn capture_loop(
    source: Source,
    device_id: Option<&str>,
    buffer: Arc<Mutex<SourceBuffer>>,
    clock: Arc<AudioClock>,
    stop: Arc<AtomicBool>,
    device_timestamps: bool,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    initialize_mta().ok()?;
    let _com = ComGuard;
    let _scheduling = AudioScheduling::enter();
    let enumerator: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };
    let device = match device_id {
        Some(id) => unsafe { enumerator.GetDevice(&HSTRING::from(id))? },
        None => unsafe {
            enumerator.GetDefaultAudioEndpoint(
                match source {
                    Source::System => eRender,
                    Source::Microphone => eCapture,
                },
                eConsole,
            )?
        },
    };
    let client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None)? };
    let format = WaveFormat::new(
        32,
        32,
        &SampleType::Float,
        SAMPLE_RATE as usize,
        CHANNELS as usize,
        None,
    );
    let mut flags = AUDCLNT_STREAMFLAGS_EVENTCALLBACK
        | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
        | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
    if matches!(source, Source::System) {
        flags |= AUDCLNT_STREAMFLAGS_LOOPBACK;
    }
    // Keep 100 ms at the endpoint as well as the mixer scheduling allowance.
    unsafe {
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            flags,
            1_000_000,
            0,
            format.as_waveformatex_ref(),
            None,
        )?;
    }
    let event = AudioEvent(unsafe { CreateEventW(None, false, false, None)? });
    unsafe {
        client.SetEventHandle(event.0)?;
    }
    let capture: IAudioCaptureClient = unsafe { client.GetService()? };
    buffer.lock().unwrap_or_else(|e| e.into_inner()).ready = true;
    while clock.start.get().is_none() {
        if stop.load(Ordering::Acquire) {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(2));
    }
    let origin = clock.start.get().unwrap();
    unsafe {
        client.Start()?;
    }
    let mut timeline = PacketTimeline::default();
    let mut seen_packet = false;
    let result = (|| -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        loop {
            // An event may cover several packets. Drain all of them before waiting.
            while unsafe { capture.GetNextPacketSize()? } > 0 {
                let (mut data, info) = read_capture_packet(&capture)?;
                let frames = data.len() / BYTES_PER_FRAME;
                if frames == 0 {
                    continue;
                }
                let start = if !device_timestamps || info.flags.timestamp_error {
                    clock.frames(true).saturating_sub(frames as u64)
                } else {
                    let relative = i128::from(info.timestamp) - i128::from(origin.qpc_100ns);
                    let frame = relative * i128::from(SAMPLE_RATE) / 10_000_000;
                    if frame < 0 {
                        let trim = (-frame).min(frames as i128) as usize;
                        data.drain(..trim * BYTES_PER_FRAME);
                    }
                    frame.max(0) as u64
                };
                let start = timeline.place(
                    start,
                    data.len() / BYTES_PER_FRAME,
                    info.flags.data_discontinuity,
                );
                let mut buffer = buffer.lock().unwrap_or_else(|e| e.into_inner());
                if seen_packet && info.flags.data_discontinuity {
                    buffer.discontinuities += 1;
                }
                buffer.push(AudioPacket { start, data });
                seen_packet = true;
            }
            if stop.load(Ordering::Acquire) {
                break;
            }
            unsafe {
                WaitForSingleObject(event.0, 10);
            }
        }
        Ok(())
    })();
    let _ = unsafe { client.Stop() };
    result
}

fn read_capture_packet(
    capture: &IAudioCaptureClient,
) -> Result<(Vec<u8>, BufferInfo), Box<dyn std::error::Error + Send + Sync>> {
    let mut data = std::ptr::null_mut();
    let mut frames = 0;
    let mut flags = 0;
    let mut position = 0;
    let mut timestamp = 0;
    unsafe {
        capture.GetBuffer(
            &mut data,
            &mut frames,
            &mut flags,
            Some(&mut position),
            Some(&mut timestamp),
        )?;
    }
    let info = BufferInfo::new(flags, position, timestamp);
    // A SILENT packet may have a null data pointer. Never form a slice from it.
    let result = if frames == 0 || info.flags.silent {
        Ok(vec![0; frames as usize * BYTES_PER_FRAME])
    } else if data.is_null() {
        Err(std::io::Error::other("WASAPI returned a null audio packet"))
    } else {
        // GetBuffer owns this memory until ReleaseBuffer on this same thread.
        Ok(unsafe { std::slice::from_raw_parts(data, frames as usize * BYTES_PER_FRAME) }.to_vec())
    };
    if frames != 0 {
        unsafe {
            capture.ReleaseBuffer(frames)?;
        }
    }
    Ok((result?, info))
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

    fn signal(frames: usize) -> Vec<u8> {
        0.5_f32.to_le_bytes().repeat(frames * CHANNELS as usize)
    }

    #[test]
    fn timestamp_jitter_does_not_cut_continuous_audio_into_silence() {
        let mut timeline = PacketTimeline::default();
        let mut buffer = SourceBuffer::default();
        for packet in 0..100_u64 {
            let jitter = if packet == 0 {
                0
            } else if packet % 2 == 0 {
                -120
            } else {
                240
            };
            let timestamp = (packet as i64 * 480 + jitter) as u64;
            let start = timeline.place(timestamp, 480, false);
            buffer.push(AudioPacket {
                start,
                data: signal(480),
            });
            assert_eq!(
                buffer.take(packet * 480, 480),
                signal(480),
                "gap in packet {packet}"
            );
        }
        assert_eq!(buffer.late_frames, 0);
    }

    #[test]
    fn timestamp_smoothing_preserves_real_pauses_and_device_discontinuities() {
        let mut timeline = PacketTimeline::default();
        assert_eq!(timeline.place(240, 480, false), 240);
        assert_eq!(timeline.place(730, 480, false), 720);
        assert_eq!(timeline.place(1440, 480, true), 1440);
        assert_eq!(timeline.place(9600, 480, false), 9600);
    }

    #[test]
    #[ignore = "requires a continuous test tone playing on the default output device"]
    fn live_loopback_tone_has_no_internal_silence() {
        let config = AppConfig {
            system_audio: true,
            microphone: false,
            ..Default::default()
        };
        let (events, receiver) = std::sync::mpsc::channel();
        let mut pipeline = AudioPipeline::start(&config, Arc::new(AtomicBool::new(false)), events)
            .unwrap()
            .unwrap();
        let samples = Arc::new(Mutex::new(Vec::new()));
        let output = samples.clone();
        pipeline
            .begin_with_sink(move |bytes| {
                output.lock().unwrap().extend_from_slice(bytes);
                Ok(())
            })
            .unwrap();
        thread::sleep(Duration::from_secs(5));
        pipeline.stop_and_join().unwrap();
        let samples = samples.lock().unwrap();
        let frames: Vec<_> = samples
            .as_chunks::<4>()
            .0
            .iter()
            .map(|f| i16::from_le_bytes([f[0], f[1]]))
            .collect();
        let middle = &frames[SAMPLE_RATE as usize..SAMPLE_RATE as usize * 4];
        let mut silent_windows = 0;
        for block in middle.as_chunks::<480>().0 {
            let energy =
                block.iter().map(|s| f64::from(*s).powi(2)).sum::<f64>() / block.len() as f64;
            if energy.sqrt() < 100.0 {
                silent_windows += 1;
            }
        }
        println!(
            "loopback: {} frames, {} silent 10-ms windows in continuous tone",
            frames.len(),
            silent_windows
        );
        assert_eq!(silent_windows, 0, "continuous tone contains dropouts");
        let source = pipeline.buffers[0].lock().unwrap();
        println!(
            "packets={}, late={}, dropped={}, discontinuities={}",
            source.received_packets,
            source.late_frames,
            source.dropped_frames,
            source.discontinuities
        );
        assert_eq!(source.dropped_frames, 0);
        assert!(source.late_frames < LOSS_WARNING_FRAMES);
        assert!(receiver.try_iter().collect::<Vec<_>>().is_empty());
    }

    #[test]
    #[ignore = "captures live system audio and the default microphone, discarding samples"]
    fn live_audio_capture_has_no_false_dropouts_without_video_callbacks() {
        let devices = enumerate_microphones().expect("microphone enumeration");
        let config = AppConfig {
            system_audio: true,
            microphone: !devices.devices.is_empty(),
            ..Default::default()
        };
        let (events, receiver) = std::sync::mpsc::channel();
        let mut pipeline = AudioPipeline::start(&config, Arc::new(AtomicBool::new(false)), events)
            .unwrap()
            .unwrap();
        let samples = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let writer_samples = samples.clone();
        pipeline
            .begin_with_sink(move |data| {
                writer_samples
                    .fetch_add(data.len() as u64 / (CHANNELS as u64 * 2), Ordering::Relaxed);
                Ok(())
            })
            .unwrap();
        thread::sleep(Duration::from_secs(3));
        pipeline.stop_and_join().unwrap();
        assert_eq!(samples.load(Ordering::Relaxed), pipeline.clock.frames(true));
        for (index, source) in pipeline.buffers.iter().enumerate() {
            let source = source.lock().unwrap();
            println!(
                "source={index}, enabled={}, packets={}, frames={}, late={}, dropped={}, discontinuities={}",
                source.enabled,
                source.received_packets,
                source.received_frames,
                source.late_frames,
                source.dropped_frames,
                source.discontinuities
            );
            assert!(!source.failed, "audio source failed");
            assert_eq!(source.dropped_frames, 0);
            assert!(source.late_frames < LOSS_WARNING_FRAMES);
            if index == 1 && source.enabled {
                assert!(
                    source.received_packets > 0,
                    "microphone supplied no packets"
                );
            }
        }
        let warnings: Vec<_> = receiver.try_iter().collect();
        assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
    }

    fn mixer() -> (AudioMixer, std::sync::mpsc::Receiver<RecorderEvent>) {
        let (events, receiver) = std::sync::mpsc::channel();
        (
            AudioMixer {
                buffers: [true, false].map(|enabled| {
                    Arc::new(Mutex::new(SourceBuffer {
                        enabled,
                        ready: true,
                        ..Default::default()
                    }))
                }),
                volumes: [1.0, 1.0],
                position: 0,
                events,
                warned: false,
            },
            receiver,
        )
    }

    #[test]
    fn silence_and_partial_shutdown_tail_are_not_audio_loss() {
        let (mut mixer, warnings) = mixer();
        let mut output = Vec::new();
        mixer
            .drain_to(1007, &mut |bytes| {
                output.extend_from_slice(bytes);
                Ok(())
            })
            .unwrap();
        assert_eq!(output.len(), 1007 * CHANNELS as usize * 2);
        assert!(output.iter().all(|byte| *byte == 0));
        assert_eq!(mixer.buffers[0].lock().unwrap().late_frames, 0);
        assert!(warnings.try_recv().is_err());
    }

    #[test]
    fn packet_timestamps_keep_silence_and_signals_at_their_positions() {
        let mut buffer = SourceBuffer::default();
        buffer.push(AudioPacket {
            start: 240,
            data: signal(480),
        });
        let a = buffer.take(0, 480);
        let b = buffer.take(480, 480);
        assert!(a[..240 * BYTES_PER_FRAME].iter().all(|byte| *byte == 0));
        assert_eq!(&a[240 * BYTES_PER_FRAME..], signal(240));
        assert_eq!(&b[..240 * BYTES_PER_FRAME], signal(240));
        assert!(b[240 * BYTES_PER_FRAME..].iter().all(|byte| *byte == 0));
        assert_eq!(buffer.late_frames, 0);
    }

    #[test]
    fn real_overflow_is_bounded_reported_and_preserves_duration() {
        let (mut mixer, warnings) = mixer();
        for i in 0..110 {
            mixer.buffers[0].lock().unwrap().push(AudioPacket {
                start: i * FRAMES_PER_CHUNK as u64,
                data: signal(FRAMES_PER_CHUNK),
            });
        }
        assert!(mixer.buffers[0].lock().unwrap().queued_frames <= MAX_BUFFER_FRAMES);
        let mut bytes = 0;
        mixer
            .drain_to(110 * FRAMES_PER_CHUNK as u64, &mut |data| {
                bytes += data.len();
                Ok(())
            })
            .unwrap();
        assert_eq!(bytes, 110 * FRAMES_PER_CHUNK * CHANNELS as usize * 2);
        assert!(matches!(
            warnings.try_recv(),
            Ok(RecorderEvent::AudioWarning(_))
        ));
        assert!(warnings.try_recv().is_err());
    }

    #[test]
    fn late_packets_only_count_samples_that_have_already_been_output() {
        let mut buffer = SourceBuffer::default();
        buffer.take(0, 480);
        buffer.push(AudioPacket {
            start: 240,
            data: signal(480),
        });
        assert_eq!(buffer.late_frames, 240);
        assert_eq!(buffer.take(480, 240), signal(240));
    }

    #[test]
    fn failed_source_does_not_block_healthy_source() {
        let (mut mixer, warnings) = mixer();
        mixer.buffers[0].lock().unwrap().failed = true;
        mixer.buffers[1].lock().unwrap().push(AudioPacket {
            start: 0,
            data: signal(960),
        });
        let mut output = Vec::new();
        mixer
            .drain_to(960, &mut |data| {
                output.extend_from_slice(data);
                Ok(())
            })
            .unwrap();
        assert_eq!(i16::from_le_bytes([output[0], output[1]]), 16384);
        assert!(warnings.try_recv().is_err());
    }

    #[test]
    fn audio_writer_runs_without_any_video_callbacks() {
        let (mixer, warnings) = mixer();
        let stop = Arc::new(AtomicBool::new(false));
        let mut pipeline = AudioPipeline {
            buffers: mixer.buffers,
            volumes: mixer.volumes,
            clock: Arc::new(AudioClock::default()),
            handles: Vec::new(),
            writer: None,
            sources_done: Arc::new(AtomicBool::new(false)),
            stop,
            events: mixer.events,
        };
        let (sender, receiver) = std::sync::mpsc::channel();
        pipeline
            .begin_with_sink(move |data| sender.send(data.len()).map_err(|e| e.to_string()))
            .unwrap();
        assert!(receiver.recv_timeout(Duration::from_secs(2)).unwrap() > 0);
        pipeline.stop_and_join().unwrap();
        assert!(warnings.try_recv().is_err());
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
        let sample = 0.75_f32.to_le_bytes();
        assert_eq!(
            i16::from_le_bytes(mix_f32(&sample, &sample, 1.0, 1.0).try_into().unwrap()),
            i16::MAX
        );
    }
}
