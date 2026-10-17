use crate::{i18n::Message, msg};
use std::{
    fs::{self, OpenOptions},
    path::Path,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::{Duration, Instant},
};

use chrono::Local;
use windows::Win32::{
    Graphics::Gdi::{
        BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CAPTUREBLT, CreateCompatibleDC,
        CreateDIBSection, DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, GetMonitorInfoW, HBITMAP,
        HDC, HGDIOBJ, HMONITOR, MONITORINFO, ROP_CODE, ReleaseDC, SRCCOPY, SelectObject,
    },
    UI::WindowsAndMessaging::{
        CURSOR_SHOWING, CURSORINFO, DI_NORMAL, DrawIconEx, GetCursorInfo, GetIconInfo, HICON,
        ICONINFO,
    },
};
use windows_capture::{
    capture::{Context, GraphicsCaptureApiHandler},
    encoder::{
        AudioSettingsBuilder, ContainerSettingsBuilder, VideoEncoder, VideoSettingsBuilder,
        VideoSettingsSubType,
    },
    frame::Frame,
    graphics_capture_api::InternalCaptureControl,
    monitor::Monitor,
    settings::{
        ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
        GraphicsCaptureItemType, MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
    },
};

use crate::{
    audio::{AudioPipeline, CHANNELS, SAMPLE_RATE},
    config::AppConfig,
    debug_log,
};

#[derive(Debug, Clone)]
pub enum RecorderEvent {
    Started(PathBuf),
    AudioWarning(Message),
    Finished(PathBuf),
    Failed(Message),
}

#[derive(Default)]
pub struct RecorderManager {
    stop: Option<Arc<AtomicBool>>,
    worker: Option<thread::JoinHandle<()>>,
    relay: Option<thread::JoinHandle<()>>,
}

impl RecorderManager {
    pub fn start(
        &mut self,
        config: AppConfig,
        stop: Arc<AtomicBool>,
        wake: Arc<dyn Fn() + Send + Sync>,
    ) -> Receiver<RecorderEvent> {
        self.shutdown();
        debug_log::write("RecorderManager::start called");
        let (worker_sender, worker_receiver) = mpsc::channel();
        let (event_sender, event_receiver) = mpsc::channel();
        self.stop = Some(stop.clone());

        let failure_sender = event_sender.clone();
        let failure_wake = wake.clone();
        match thread::Builder::new()
            .name("recorder-events".into())
            .spawn(move || {
                while let Ok(event) = worker_receiver.recv() {
                    if event_sender.send(event).is_err() {
                        break;
                    }
                    wake();
                }
            }) {
            Ok(relay) => self.relay = Some(relay),
            Err(error) => {
                let _ = failure_sender.send(RecorderEvent::Failed(msg!(
                    "Could not start the recording event handler: {error}",
                    "Не удалось запустить обработчик записи: {error}"
                )));
                failure_wake();
                return event_receiver;
            }
        }
        let panic_sender = worker_sender.clone();
        match thread::Builder::new()
            .name("screen-recorder".into())
            .spawn(move || {
                if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run(config, stop, worker_sender)
                }))
                .is_err()
                {
                    let _ = panic_sender.send(RecorderEvent::Failed(msg!(
                        "The recording thread crashed.",
                        "Поток записи завершился аварийно."
                    )));
                }
            }) {
            Ok(worker) => self.worker = Some(worker),
            Err(error) => {
                let _ = failure_sender.send(RecorderEvent::Failed(msg!(
                    "Could not start recording: {error}",
                    "Не удалось запустить запись: {error}"
                )));
                failure_wake();
            }
        }

        event_receiver
    }

    pub fn stop(&self) {
        debug_log::write("RecorderManager::stop called");
        if let Some(stop) = &self.stop {
            stop.store(true, Ordering::Release);
        }
    }

    pub fn clear(&mut self) {
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        if let Some(relay) = self.relay.take() {
            let _ = relay.join();
        }
        self.stop = None;
    }
}

impl RecorderManager {
    pub fn shutdown(&mut self) {
        self.stop();
        self.clear();
    }
}
impl Drop for RecorderManager {
    fn drop(&mut self) {
        self.shutdown();
    }
}

// create_new arbitrates between instances; the empty file reserves the name.
// Close its handle before WinRT opens the file with exclusive sharing.
fn reserve_output(directory: &Path, stem: &str) -> std::io::Result<PathBuf> {
    for suffix in 0..u64::MAX {
        let name = if suffix == 0 {
            format!("{stem}.mp4")
        } else {
            format!("{stem}_{suffix}.mp4")
        };
        let path = directory.join(name);
        match OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(_) => return Ok(path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::other(
        "Could not reserve a recording filename",
    ))
}

fn create_encoder(
    config: &AppConfig,
    width: u32,
    height: u32,
    audio_enabled: bool,
    output: &Path,
) -> Result<VideoEncoder, windows_capture::encoder::VideoEncoderError> {
    VideoEncoder::new(
        VideoSettingsBuilder::new(width, height)
            .sub_type(VideoSettingsSubType::H264)
            .bitrate(config.bitrate_mbps * 1_000_000)
            .frame_rate(config.fps),
        AudioSettingsBuilder::new()
            .disabled(!audio_enabled)
            .sample_rate(SAMPLE_RATE)
            .channel_count(CHANNELS)
            .bit_per_sample(16),
        ContainerSettingsBuilder::new(),
        output,
    )
}

struct CaptureFlags {
    config: AppConfig,
    output: PathBuf,
    width: u32,
    height: u32,
    stop: Arc<AtomicBool>,
    events: Sender<RecorderEvent>,
    audio: Option<AudioPipeline>,
    finalization_error: Arc<Mutex<Option<String>>>,
}

pub(crate) type SharedEncoder = Arc<Mutex<Option<VideoEncoder>>>;

struct Capture {
    encoder: SharedEncoder,
    stop: Arc<AtomicBool>,
    frame_interval: Duration,
    last_frame: Option<Instant>,
    audio: Option<AudioPipeline>,
    finalization_error: Arc<Mutex<Option<String>>>,
}

impl GraphicsCaptureApiHandler for Capture {
    type Flags = CaptureFlags;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        debug_log::write(format!(
            "capture handler initializing encoder: {}x{}, fps={}, output={}",
            ctx.flags.width,
            ctx.flags.height,
            ctx.flags.config.fps,
            ctx.flags.output.display()
        ));
        let audio_enabled = ctx.flags.audio.is_some();
        let encoder = create_encoder(
            &ctx.flags.config,
            ctx.flags.width,
            ctx.flags.height,
            audio_enabled,
            &ctx.flags.output,
        )?;

        let mut capture = Self {
            encoder: Arc::new(Mutex::new(Some(encoder))),
            stop: ctx.flags.stop,
            frame_interval: Duration::from_secs_f64(1.0 / ctx.flags.config.fps.max(1) as f64),
            last_frame: None,
            audio: ctx.flags.audio,
            finalization_error: ctx.flags.finalization_error,
        };
        if let Some(audio) = &mut capture.audio {
            audio.begin(capture.encoder.clone())?;
        }
        let _ = ctx
            .flags
            .events
            .send(RecorderEvent::Started(ctx.flags.output));
        debug_log::write("encoder created; Started event sent");
        Ok(capture)
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        capture_control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        if self.stop.load(Ordering::Acquire) {
            debug_log::write("frame handler observed stop signal");
            capture_control.stop();
            return Ok(());
        }

        let now = Instant::now();
        if self
            .last_frame
            .is_some_and(|last| now.duration_since(last) < self.frame_interval)
        {
            return Ok(());
        }
        self.encoder
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_mut()
            .expect("encoder is available")
            .send_frame(frame)?;
        self.last_frame = Some(now);
        Ok(())
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        debug_log::write("capture handler dropped; finalizing encoder");
        if let Some(audio) = &mut self.audio
            && let Err(error) = audio.stop_and_join()
        {
            *self
                .finalization_error
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some(error);
        }
        if let Some(encoder) = self
            .encoder
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            && let Err(error) = encoder.finish()
        {
            debug_log::write(format!("encoder finalization failed: {error}"));
            *self.finalization_error.lock().unwrap() = Some(error.to_string());
        } else {
            debug_log::write("encoder finalization completed");
        }
    }
}

struct GdiCapture {
    screen_dc: HDC,
    memory_dc: HDC,
    bitmap: HBITMAP,
    previous_object: HGDIOBJ,
    pixels: *mut u8,
    left: i32,
    top: i32,
    width: u32,
    height: u32,
}

impl GdiCapture {
    fn new(monitor: Monitor, width: u32, height: u32) -> Result<Self, String> {
        let mut monitor_info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..MONITORINFO::default()
        };
        let hmonitor = HMONITOR(monitor.as_raw_hmonitor());
        if !unsafe { GetMonitorInfoW(hmonitor, &mut monitor_info) }.as_bool() {
            return Err(format!(
                "GetMonitorInfoW: {}",
                std::io::Error::last_os_error()
            ));
        }

        let screen_dc = unsafe { GetDC(None) };
        if screen_dc.0.is_null() {
            return Err(format!("GetDC: {}", std::io::Error::last_os_error()));
        }
        let memory_dc = unsafe { CreateCompatibleDC(Some(screen_dc)) };
        if memory_dc.0.is_null() {
            unsafe { ReleaseDC(None, screen_dc) };
            return Err(format!(
                "CreateCompatibleDC: {}",
                std::io::Error::last_os_error()
            ));
        }

        let bitmap_info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width as i32,
                // Positive height intentionally produces the bottom-up BGRA
                // layout expected by VideoEncoder::send_frame_buffer.
                biHeight: height as i32,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                biSizeImage: width.saturating_mul(height).saturating_mul(4),
                ..BITMAPINFOHEADER::default()
            },
            ..BITMAPINFO::default()
        };
        let mut pixels = std::ptr::null_mut();
        let bitmap = match unsafe {
            CreateDIBSection(
                Some(screen_dc),
                &bitmap_info,
                DIB_RGB_COLORS,
                &mut pixels,
                None,
                0,
            )
        } {
            Ok(bitmap) => bitmap,
            Err(error) => {
                unsafe {
                    let _ = DeleteDC(memory_dc);
                    ReleaseDC(None, screen_dc);
                }
                return Err(format!("CreateDIBSection: {error}"));
            }
        };
        let previous_object = unsafe { SelectObject(memory_dc, HGDIOBJ(bitmap.0)) };

        Ok(Self {
            screen_dc,
            memory_dc,
            bitmap,
            previous_object,
            pixels: pixels.cast(),
            left: monitor_info.rcMonitor.left,
            top: monitor_info.rcMonitor.top,
            width,
            height,
        })
    }

    fn grab(&mut self, include_cursor: bool) -> Result<&[u8], String> {
        unsafe {
            BitBlt(
                self.memory_dc,
                0,
                0,
                self.width as i32,
                self.height as i32,
                Some(self.screen_dc),
                self.left,
                self.top,
                ROP_CODE(SRCCOPY.0 | CAPTUREBLT.0),
            )
            .map_err(|error| format!("BitBlt: {error}"))?;
        }

        if include_cursor {
            self.draw_cursor();
        }

        let length = self.width as usize * self.height as usize * 4;
        Ok(unsafe { std::slice::from_raw_parts(self.pixels, length) })
    }

    fn draw_cursor(&self) {
        let mut cursor = CURSORINFO {
            cbSize: std::mem::size_of::<CURSORINFO>() as u32,
            ..CURSORINFO::default()
        };
        if unsafe { GetCursorInfo(&mut cursor) }.is_err()
            || cursor.flags != CURSOR_SHOWING
            || cursor.hCursor.0.is_null()
        {
            return;
        }

        let icon_handle = HICON(cursor.hCursor.0);
        let mut icon = ICONINFO::default();
        let (hotspot_x, hotspot_y) = if unsafe { GetIconInfo(icon_handle, &mut icon) }.is_ok() {
            let hotspot = (icon.xHotspot as i32, icon.yHotspot as i32);
            unsafe {
                if !icon.hbmMask.0.is_null() {
                    let _ = DeleteObject(HGDIOBJ(icon.hbmMask.0));
                }
                if !icon.hbmColor.0.is_null() {
                    let _ = DeleteObject(HGDIOBJ(icon.hbmColor.0));
                }
            }
            hotspot
        } else {
            (0, 0)
        };

        let x = cursor.ptScreenPos.x - self.left - hotspot_x;
        let y = cursor.ptScreenPos.y - self.top - hotspot_y;
        let _ = unsafe { DrawIconEx(self.memory_dc, x, y, icon_handle, 0, 0, 0, None, DI_NORMAL) };
    }
}

impl Drop for GdiCapture {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.memory_dc, self.previous_object);
            let _ = DeleteObject(HGDIOBJ(self.bitmap.0));
            let _ = DeleteDC(self.memory_dc);
            ReleaseDC(None, self.screen_dc);
        }
    }
}

fn run(config: AppConfig, stop: Arc<AtomicBool>, events: Sender<RecorderEvent>) {
    debug_log::write(format!(
        "recorder thread entered; monitor={}, output_dir={}",
        config.monitor_index,
        config.output_dir.display()
    ));
    if let Err(error) = fs::create_dir_all(&config.output_dir) {
        let _ = events.send(RecorderEvent::Failed(msg!(
            "Could not create the recording folder: {error}",
            "Не удалось создать папку записи: {error}"
        )));
        return;
    }

    let monitor = match Monitor::from_index(config.monitor_index) {
        Ok(monitor) => monitor,
        Err(error) => {
            let _ = events.send(RecorderEvent::Failed(msg!(
                "Monitor unavailable: {error}",
                "Монитор недоступен: {error}"
            )));
            return;
        }
    };
    let (width, height) = match (monitor.width(), monitor.height()) {
        (Ok(width), Ok(height)) => (width, height),
        (Err(error), _) | (_, Err(error)) => {
            let _ = events.send(RecorderEvent::Failed(error.to_string().into()));
            return;
        }
    };
    let output = match reserve_output(
        &config.output_dir,
        &crate::filename::render(
            &config.filename_template,
            Local::now().naive_local(),
            config.monitor_index,
        ),
    ) {
        Ok(reserved) => reserved,
        Err(error) => {
            let _ = events.send(RecorderEvent::Failed(msg!(
                "Could not create the recording file: {error}",
                "Не удалось создать файл записи: {error}"
            )));
            return;
        }
    };
    let capture_probe: Result<GraphicsCaptureItemType, windows::core::Error> = monitor.try_into();
    let audio = match AudioPipeline::start(&config, stop.clone(), events.clone()) {
        Ok(audio) => audio,
        Err(error) => {
            let _ = events.send(RecorderEvent::Failed(error));
            return;
        }
    };
    let finalization_error = Arc::new(Mutex::new(None));
    let flags = CaptureFlags {
        config: config.clone(),
        output: output.clone(),
        width,
        height,
        stop: stop.clone(),
        events: events.clone(),
        audio,
        finalization_error: finalization_error.clone(),
    };

    match capture_probe {
        Ok(_) => {}
        Err(error) => {
            debug_log::write(format!(
                "Windows Graphics Capture unavailable: code={:?}, error={error}; using GDI fallback; stop={}",
                error.code(),
                stop.load(Ordering::Acquire)
            ));
            let _ = events.send(RecorderEvent::AudioWarning(msg!(
                "Windows Graphics Capture is unavailable; using GDI compatibility mode.",
                "Windows Graphics Capture недоступен; используется совместимый режим GDI."
            )));
            run_gdi(monitor, flags);
            return;
        }
    }

    let settings = Settings::new(
        monitor,
        if config.show_cursor {
            CursorCaptureSettings::Default
        } else {
            CursorCaptureSettings::WithoutCursor
        },
        DrawBorderSettings::WithoutBorder,
        SecondaryWindowSettings::Default,
        MinimumUpdateIntervalSettings::Default,
        DirtyRegionSettings::Default,
        ColorFormat::Bgra8,
        flags,
    );

    let control = match Capture::start_free_threaded(settings) {
        Ok(control) => control,
        Err(error) => {
            debug_log::write(format!("capture startup failed: {error}"));
            stop.store(true, Ordering::Release);
            let _ = events.send(RecorderEvent::Failed(msg!(
                "Recording failed: {error}",
                "Ошибка записи: {error}"
            )));
            return;
        }
    };
    debug_log::write("capture control created; entering stop loop");

    let deadline = (config.duration_minutes > 0)
        .then(|| Instant::now() + Duration::from_secs(u64::from(config.duration_minutes) * 60));
    let capture_result = loop {
        if control.is_finished() {
            debug_log::write("capture control finished on its own");
            break control.wait();
        }
        if stop.load(Ordering::Acquire)
            || deadline.is_some_and(|deadline| Instant::now() >= deadline)
        {
            debug_log::write("stop loop observed stop/deadline; calling CaptureControl::stop");
            stop.store(true, Ordering::Release);
            break control.stop();
        }
        thread::sleep(Duration::from_millis(20));
    };

    stop.store(true, Ordering::Release);
    if let Err(error) = capture_result {
        debug_log::write(format!("CaptureControl stop/wait failed: {error}"));
        let _ = events.send(RecorderEvent::Failed(msg!(
            "Could not stop recording: {error}",
            "Ошибка остановки записи: {error}"
        )));
        return;
    }

    let error = finalization_error.lock().unwrap().take();
    if let Some(error) = error {
        debug_log::write(format!("recording failed during finalization: {error}"));
        let _ = events.send(RecorderEvent::Failed(msg!(
            "Could not finalize MP4: {error}",
            "Не удалось завершить MP4: {error}"
        )));
    } else {
        debug_log::write(format!(
            "recording finished successfully: {}",
            output.display()
        ));
        let _ = events.send(RecorderEvent::Finished(output));
    }
}

fn run_gdi(monitor: Monitor, flags: CaptureFlags) {
    let CaptureFlags {
        config,
        output,
        width,
        height,
        stop,
        events,
        mut audio,
        ..
    } = flags;
    debug_log::write(format!("initializing GDI fallback: {width}x{height}"));
    let mut capture = match GdiCapture::new(monitor, width, height) {
        Ok(capture) => capture,
        Err(error) => {
            stop.store(true, Ordering::Release);
            debug_log::write(format!("GDI initialization failed: {error}"));
            let _ = events.send(RecorderEvent::Failed(msg!(
                "Could not start compatible capture: {error}",
                "Не удалось запустить совместимый захват: {error}"
            )));
            return;
        }
    };

    let encoder = match create_encoder(&config, width, height, audio.is_some(), &output) {
        Ok(encoder) => encoder,
        Err(error) => {
            stop.store(true, Ordering::Release);
            debug_log::write(format!("GDI encoder initialization failed: {error}"));
            let _ = events.send(RecorderEvent::Failed(msg!(
                "Could not start the encoder: {error}",
                "Не удалось запустить кодировщик: {error}"
            )));
            return;
        }
    };

    let encoder = Arc::new(Mutex::new(Some(encoder)));
    if let Some(audio) = &mut audio
        && let Err(error) = audio.begin(encoder.clone())
    {
        let _ = audio.stop_and_join();
        if let Some(encoder) = encoder.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = encoder.finish();
        }
        let _ = events.send(RecorderEvent::Failed(msg!(
            "Could not start audio encoding: {error}",
            "Не удалось запустить обработку звука: {error}"
        )));
        return;
    }
    let _ = events.send(RecorderEvent::Started(output.clone()));
    debug_log::write("GDI encoder and independent audio writer started");
    let started_at = Instant::now();
    let frame_interval = Duration::from_secs_f64(1.0 / config.fps.max(1) as f64);
    let deadline = (config.duration_minutes > 0)
        .then(|| started_at + Duration::from_secs(u64::from(config.duration_minutes) * 60));
    let mut next_frame = started_at;

    let capture_result: Result<(), String> = loop {
        if stop.load(Ordering::Acquire)
            || deadline.is_some_and(|deadline| Instant::now() >= deadline)
        {
            debug_log::write(format!(
                "GDI loop observed stop/deadline; stop={}",
                stop.load(Ordering::Acquire)
            ));
            break Ok(());
        }

        let now = Instant::now();
        if now < next_frame {
            thread::sleep((next_frame - now).min(Duration::from_millis(5)));
            continue;
        }

        let buffer = match capture.grab(config.show_cursor) {
            Ok(buffer) => buffer,
            Err(error) => {
                debug_log::write(format!("GDI frame grab failed: {error}"));
                break Err(error);
            }
        };
        let timestamp = i64::try_from(started_at.elapsed().as_nanos() / 100).unwrap_or(i64::MAX);
        if let Err(error) = encoder
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_mut()
            .expect("encoder is available")
            .send_frame_buffer(buffer, timestamp)
        {
            let error = format!("frame encoding failed: {error}");
            debug_log::write(&error);
            break Err(error);
        }

        next_frame += frame_interval;
        if next_frame < Instant::now() {
            next_frame = Instant::now() + frame_interval;
        }
    };

    stop.store(true, Ordering::Release);
    let audio_result = if let Some(audio) = &mut audio {
        audio.stop_and_join()
    } else {
        Ok(())
    };
    drop(audio);
    debug_log::write("finalizing GDI encoder");
    let finish_result = encoder
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
        .expect("encoder is available")
        .finish()
        .map_err(|error| error.to_string());

    let result = match capture_result {
        Err(error) => Err(error),
        Ok(()) => audio_result.and(finish_result),
    };
    if let Err(error) = result {
        debug_log::write(format!("GDI recording failed: {error}"));
        let _ = events.send(RecorderEvent::Failed(msg!(
            "Recording failed: {error}",
            "Ошибка записи: {error}"
        )));
    } else {
        debug_log::write(format!(
            "GDI recording finished successfully: {}",
            output.display()
        ));
        let _ = events.send(RecorderEvent::Finished(output));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mp4_boxes(mut bytes: &[u8]) -> Vec<(&[u8], &[u8])> {
        let mut boxes = Vec::new();
        while bytes.len() >= 8 {
            let length = u32::from_be_bytes(bytes[..4].try_into().unwrap());
            let (length, header) = match length {
                0 => (bytes.len(), 8),
                1 if bytes.len() >= 16 => (
                    u64::from_be_bytes(bytes[8..16].try_into().unwrap()) as usize,
                    16,
                ),
                length => (length as usize, 8),
            };
            assert!(
                length >= header && length <= bytes.len(),
                "invalid MP4 box length"
            );
            boxes.push((&bytes[4..8], &bytes[header..length]));
            bytes = &bytes[length..];
        }
        boxes
    }

    fn track_durations(bytes: &[u8]) -> (f64, f64) {
        let movie = mp4_boxes(bytes)
            .into_iter()
            .find(|(kind, _)| *kind == b"moov")
            .expect("finalized MP4 movie atom")
            .1;
        let (mut video, mut audio) = (None, None);
        for (_, track) in mp4_boxes(movie)
            .into_iter()
            .filter(|(kind, _)| *kind == b"trak")
        {
            let media = mp4_boxes(track)
                .into_iter()
                .find(|(kind, _)| *kind == b"mdia")
                .unwrap()
                .1;
            let children = mp4_boxes(media);
            let header = children
                .iter()
                .find(|(kind, _)| *kind == b"mdhd")
                .unwrap()
                .1;
            let handler = children
                .iter()
                .find(|(kind, _)| *kind == b"hdlr")
                .unwrap()
                .1;
            let (scale, duration) = if header[0] == 1 {
                (
                    u32::from_be_bytes(header[20..24].try_into().unwrap()),
                    u64::from_be_bytes(header[24..32].try_into().unwrap()),
                )
            } else {
                (
                    u32::from_be_bytes(header[12..16].try_into().unwrap()),
                    u64::from(u32::from_be_bytes(header[16..20].try_into().unwrap())),
                )
            };
            let seconds = duration as f64 / f64::from(scale);
            match &handler[8..12] {
                b"vide" => video = Some(seconds),
                b"soun" => audio = Some(seconds),
                _ => {}
            }
        }
        (video.expect("video track"), audio.expect("audio track"))
    }

    #[test]
    fn output_reservation_never_truncates_existing_recordings() {
        let directory = tempfile::tempdir().unwrap();
        let existing = directory.path().join("same.mp4");
        fs::write(&existing, b"previous recording").unwrap();
        let first = reserve_output(directory.path(), "same").unwrap();
        let second = reserve_output(directory.path(), "same").unwrap();
        assert_ne!(first, second);
        assert_eq!(fs::read(existing).unwrap(), b"previous recording");
        assert!(first.exists());
    }

    #[test]
    fn concurrent_instances_reserve_distinct_outputs() {
        let directory = tempfile::tempdir().unwrap();
        let workers: Vec<_> = (0..8)
            .map(|_| {
                let path = directory.path().to_path_buf();
                thread::spawn(move || reserve_output(&path, "same").unwrap())
            })
            .collect();
        let reservations: Vec<_> = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect();
        let paths: std::collections::HashSet<_> = reservations.iter().collect();
        assert_eq!(paths.len(), 8);
    }

    #[test]
    fn shutdown_waits_for_worker_cleanup() {
        let stop = Arc::new(AtomicBool::new(false));
        let cleanup_complete = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let worker_complete = cleanup_complete.clone();
        let worker = thread::spawn(move || {
            while !worker_stop.load(Ordering::Acquire) {
                thread::yield_now();
            }
            worker_complete.store(true, Ordering::Release);
        });
        let mut manager = RecorderManager {
            stop: Some(stop),
            worker: Some(worker),
            relay: None,
        };
        manager.shutdown();
        assert!(cleanup_complete.load(Ordering::Acquire));
        assert!(manager.worker.is_none());
    }

    #[test]
    #[ignore = "captures the live desktop for the recorder integration test"]
    fn live_capture_starts_and_stops_from_atomic_signal() {
        let output_dir = std::env::current_dir()
            .expect("working directory")
            .join("target")
            .join("capture-self-test");
        let monitor = Monitor::enumerate()
            .expect("monitor enumeration")
            .first()
            .copied()
            .expect("at least one monitor");
        let monitor_index = monitor.index().expect("monitor index");
        let config = AppConfig {
            monitor_index,
            system_audio: true,
            microphone: false,
            output_dir,
            ..AppConfig::default()
        };
        let stop = Arc::new(AtomicBool::new(false));
        let mut manager = RecorderManager::default();
        let events = manager.start(config, stop, Arc::new(|| {}));

        let started = loop {
            match events
                .recv_timeout(Duration::from_secs(20))
                .expect("recorder start event")
            {
                RecorderEvent::Started(path) => break path,
                RecorderEvent::AudioWarning(message) => assert_ne!(
                    message.render(crate::i18n::Language::En),
                    "Audio dropouts detected"
                ),
                RecorderEvent::Failed(error) => panic!("recorder failed to start: {error}"),
                RecorderEvent::Finished(path) => {
                    panic!("recorder finished before stop: {}", path.display())
                }
            }
        };

        thread::sleep(Duration::from_secs(2));
        manager.stop();

        loop {
            match events
                .recv_timeout(Duration::from_secs(20))
                .expect("recorder finish event")
            {
                RecorderEvent::Finished(path) => {
                    assert_eq!(path, started);
                    assert!(path.exists(), "output file was not created");
                    assert!(fs::metadata(&path).expect("output metadata").len() > 0);
                    let (video, audio) = track_durations(&fs::read(&path).expect("recorded MP4"));
                    println!(
                        "MP4 duration: video={video:.3}s, audio={audio:.3}s, delta={:.3}s",
                        (audio - video).abs()
                    );
                    assert!(video > 1.5 && audio > 1.5);
                    assert!((audio - video).abs() < 0.2, "audio/video duration mismatch");
                    if std::env::var_os("HSR_KEEP_TEST_VIDEO").is_none() {
                        let _ = fs::remove_file(path);
                    } else {
                        println!("Test recording retained: {}", path.display());
                    }
                    break;
                }
                RecorderEvent::AudioWarning(message) => assert_ne!(
                    message.render(crate::i18n::Language::En),
                    "Audio dropouts detected"
                ),
                RecorderEvent::Started(_) => {}
                RecorderEvent::Failed(error) => panic!("recorder failed to stop: {error}"),
            }
        }
    }
}
