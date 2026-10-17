use crate::{
    i18n::{Language, Message},
    msg,
};
use std::{
    path::PathBuf,
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver},
    },
    time::{Duration, Instant},
};

use eframe::egui::{
    self, Align, Button, Color32, ComboBox, Frame, Layout, Margin, Pos2, RichText, Slider, Stroke,
    Vec2, ViewportCommand, WindowLevel,
};
use windows::Win32::Foundation::HWND;
use windows::core::w;
use windows_capture::monitor::Monitor;

use crate::{
    audio::{self, MicrophoneDevice},
    config::AppConfig,
    debug_log,
    hotkeys::{self, HotkeyController, HotkeyEvent, Shortcut},
    recorder::{RecorderEvent, RecorderManager},
};

pub const WINDOW_TITLE: &str = "Hidden Screen Recorder";
pub const WINDOW_SIZE: Vec2 = Vec2::new(1100.0, 584.0);
pub const MIN_WINDOW_SIZE: Vec2 = Vec2::new(1000.0, 584.0);

type MicrophoneScan = Result<audio::MicrophoneScan, String>;

#[derive(Debug, Clone)]
struct MonitorInfo {
    index: usize,
    label: String,
}

#[derive(Debug)]
enum AppState {
    Idle,
    Preparing(Instant),
    Starting,
    Recording,
    Stopping,
}

impl AppState {
    fn capture_started(&mut self) {
        if matches!(self, Self::Starting) {
            *self = Self::Recording;
        }
    }
}

#[cfg(test)]
mod state_tests {
    use super::AppState;

    #[test]
    fn late_started_cannot_cancel_stopping() {
        let mut state = AppState::Starting;
        state.capture_started();
        assert!(matches!(state, AppState::Recording));
        state = AppState::Stopping;
        state.capture_started();
        assert!(matches!(state, AppState::Stopping));
        state = AppState::Idle;
        state.capture_started();
        assert!(matches!(state, AppState::Idle));
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum HotkeyTarget {
    Start,
    Stop,
}

pub struct HiddenScreenRecorderApp {
    config: AppConfig,
    monitors: Vec<MonitorInfo>,
    microphones: Vec<MicrophoneDevice>,
    microphones_pending: Option<Receiver<MicrophoneScan>>,
    recorder: RecorderManager,
    recorder_events: Option<Receiver<RecorderEvent>>,
    hotkey_events: Receiver<HotkeyEvent>,
    state: AppState,
    warning: Option<Message>,
    last_output: Option<PathBuf>,
    window: Option<HWND>,
    close_pending: bool,
    restore_position: Option<Pos2>,
    hotkey_worker: Option<std::thread::JoinHandle<()>>,
    stop_hotkey_available: bool,
    stop_signal: Arc<AtomicBool>,
    hotkey_controller: Arc<HotkeyController>,
    editing_hotkey: Option<HotkeyTarget>,
    hotkey_error: Option<Message>,
}

impl HiddenScreenRecorderApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        mut config: AppConfig,
        initial_warning: Option<Message>,
    ) -> Self {
        debug_log::write("creating application UI");
        configure_style(&cc.egui_ctx);
        let monitors = enumerate_monitors(config.language());
        if !monitors
            .iter()
            .any(|monitor| monitor.index == config.monitor_index)
        {
            config.monitor_index = monitors[0].index;
        }
        let (hotkey_tx, hotkey_rx) = mpsc::channel();
        let stop_signal = Arc::new(AtomicBool::new(false));
        let repaint = cc.egui_ctx.clone();
        let hotkey_controller = Arc::new(HotkeyController::new(config.shortcuts));
        hotkey_controller.set_stop_signal(stop_signal.clone());
        let hotkey_worker = hotkeys::spawn(hotkey_tx, hotkey_controller.clone(), move || {
            repaint.request_repaint();
        });

        let (hotkey_worker, hotkey_warning) = match hotkey_worker {
            Ok(worker) => (Some(worker), None),
            Err(error) => (
                None,
                Some(msg!(
                    "Could not start global shortcuts: {error}",
                    "Не удалось запустить горячие клавиши: {error}"
                )),
            ),
        };
        let (window, window_warning) = match crate::window::native_handle(cc) {
            Ok(window) => (
                Some(window),
                crate::window::exclude_from_capture(window).err(),
            ),
            Err(error) => (None, Some(error)),
        };
        let warning = Message::join(
            [initial_warning, hotkey_warning, window_warning]
                .into_iter()
                .flatten(),
        );
        let mut app = Self {
            config,
            monitors,
            microphones: Vec::new(),
            microphones_pending: None,
            recorder: RecorderManager::default(),
            recorder_events: None,
            hotkey_events: hotkey_rx,
            state: AppState::Idle,
            warning,
            last_output: None,
            window,
            close_pending: false,
            restore_position: None,
            hotkey_worker,
            stop_hotkey_available: false,
            stop_signal,
            hotkey_controller,
            editing_hotkey: None,
            hotkey_error: None,
        };
        app.refresh_microphones(&cc.egui_ctx);
        app
    }

    fn refresh_microphones(&mut self, ctx: &egui::Context) {
        if self.microphones_pending.is_some() {
            return;
        }
        let (sender, receiver) = mpsc::channel();
        let repaint = ctx.clone();
        match std::thread::Builder::new()
            .name("microphone-list".into())
            .spawn(move || {
                let _ = sender.send(audio::enumerate_microphones());
                repaint.request_repaint();
            }) {
            Ok(_) => self.microphones_pending = Some(receiver),
            Err(error) => {
                self.warning = Some(msg!(
                    "Could not list microphones: {error}",
                    "Не удалось получить список микрофонов: {error}"
                ))
            }
        }
    }

    fn poll_microphones(&mut self) {
        let Some(receiver) = &self.microphones_pending else {
            return;
        };
        let result = match receiver.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => Err("The device-list thread exited.".into()),
        };
        self.microphones_pending = None;
        match result {
            Ok(scan) => {
                self.microphones = scan.devices;
                if !scan.warnings.is_empty() {
                    self.warning = Some(msg!(
                        "Some microphones are unavailable: {}",
                        "Некоторые микрофоны недоступны: {}",
                        scan.warnings.join("; ")
                    ));
                }
            }
            Err(error) => {
                self.warning = Some(msg!(
                    "Could not list microphones: {error}",
                    "Не удалось получить список микрофонов: {error}"
                ))
            }
        }
    }

    fn render_microphone_selector(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let language = self.config.language();
        let selected = match self.config.microphone_device.as_deref() {
            None => language.text("System default", "Системный по умолчанию"),
            Some(id) => self
                .microphones
                .iter()
                .find(|device| device.id == id)
                .map_or(
                    language.text(
                        "Selected microphone unavailable",
                        "Выбранный микрофон недоступен",
                    ),
                    |device| device.name.as_str(),
                ),
        }
        .to_owned();
        let previous = self.config.microphone_device.clone();
        let mut refresh = false;
        row(ui, language.text("Device", "Устройство"), |ui| {
            ComboBox::from_id_salt("microphone_device")
                .selected_text(ellipsis(&selected, 30))
                .width(230.0)
                .show_ui(ui, |ui| {
                    ui.selectable_value(
                        &mut self.config.microphone_device,
                        None,
                        language.text("System default", "Системный по умолчанию"),
                    );
                    for device in &self.microphones {
                        ui.selectable_value(
                            &mut self.config.microphone_device,
                            Some(device.id.clone()),
                            &device.name,
                        );
                    }
                    ui.separator();
                    if ui
                        .add_enabled(
                            self.microphones_pending.is_none(),
                            Button::new(language.text("Refresh devices", "Обновить список")),
                        )
                        .clicked()
                    {
                        refresh = true;
                        ui.close();
                    }
                })
                .response
                .on_hover_text(&selected);
        });
        if refresh {
            self.refresh_microphones(ctx);
        }
        if previous != self.config.microphone_device {
            self.save_config();
        }
    }

    fn begin_recording(&mut self, ctx: &egui::Context) {
        debug_log::write(format!("begin_recording requested; state={:?}", self.state));
        if !matches!(self.state, AppState::Idle)
            || self.close_pending
            || self.editing_hotkey.is_some()
            || self.hotkey_error.is_some()
        {
            debug_log::write("start ignored because app is not idle");
            return;
        }
        if !self.stop_hotkey_available {
            debug_log::write("start rejected because stop hotkey is unavailable");
            self.warning = Some(msg!(
                "The stop shortcut is unavailable. Restart Hidden Screen Recorder.",
                "Клавиша остановки недоступна. Перезапустите Hidden Screen Recorder."
            ));
            return;
        }
        self.stop_signal = Arc::new(AtomicBool::new(false));
        self.hotkey_controller
            .set_stop_signal(self.stop_signal.clone());
        self.warning = None;
        self.save_config();
        self.restore_position = ctx.input(|input| input.viewport().outer_rect.map(|rect| rect.min));
        if self.config.hide_window {
            self.park_window(ctx);
        }
        self.state = AppState::Preparing(Instant::now() + Duration::from_millis(450));
        debug_log::write("UI parked off-screen; state changed to Preparing");
    }

    fn start_capture_if_ready(&mut self, ctx: &egui::Context) {
        let AppState::Preparing(start_at) = self.state else {
            return;
        };
        if self.stop_signal.load(Ordering::Acquire) {
            debug_log::write("capture start cancelled by stop signal");
            self.state = AppState::Idle;
            self.show_window(ctx);
            return;
        }
        if Instant::now() < start_at {
            return;
        }
        let repaint = ctx.clone();
        let wake: Arc<dyn Fn() + Send + Sync> = Arc::new(move || repaint.request_repaint());
        self.recorder_events = Some(self.recorder.start(
            self.config.clone(),
            self.stop_signal.clone(),
            wake,
        ));
        self.state = AppState::Starting;
        debug_log::write("recorder worker started; waiting for encoder startup");
    }

    fn request_stop(&mut self, ctx: &egui::Context) {
        debug_log::write(format!("request_stop received; state={:?}", self.state));
        self.stop_signal.store(true, Ordering::Release);
        match self.state {
            AppState::Preparing(_) => {
                self.state = AppState::Idle;
                self.show_window(ctx);
                debug_log::write("recording preparation cancelled");
            }
            AppState::Starting | AppState::Recording => {
                self.recorder.stop();
                self.state = AppState::Stopping;
                self.show_status_bar(ctx);
                debug_log::write("state changed to Stopping; status bar requested");
            }
            AppState::Idle | AppState::Stopping => {
                debug_log::write("stop UI event ignored for current state");
            }
        }
    }

    fn handle_event(&mut self, event: RecorderEvent, ctx: &egui::Context) {
        debug_log::write(format!("recorder event received: {event:?}"));
        match event {
            RecorderEvent::Started(path) => {
                self.last_output = Some(path);
                // A late Started must not undo a pending stop.
                self.state.capture_started();
            }
            RecorderEvent::AudioWarning(message) => {
                self.warning = Some(message);
            }
            RecorderEvent::Finished(path) => {
                self.last_output = Some(path);
                self.recorder.clear();
                self.recorder_events = None;
                self.state = AppState::Idle;
                self.show_window(ctx);
            }
            RecorderEvent::Failed(message) => {
                // Keep a finalization failure visible even if close was requested.
                self.close_pending = false;
                self.warning = Some(message);
                self.recorder.clear();
                self.recorder_events = None;
                self.state = AppState::Idle;
                self.show_window(ctx);
            }
        }
    }

    fn show_window(&self, ctx: &egui::Context) {
        if let Some(window) = self.window {
            crate::window::set_taskbar_visible(window, true);
        }
        ctx.send_viewport_cmd(ViewportCommand::MousePassthrough(false));
        ctx.send_viewport_cmd(ViewportCommand::Transparent(false));
        ctx.send_viewport_cmd(ViewportCommand::Decorations(true));
        ctx.send_viewport_cmd(ViewportCommand::Resizable(true));
        ctx.send_viewport_cmd(ViewportCommand::WindowLevel(WindowLevel::Normal));
        ctx.send_viewport_cmd(ViewportCommand::MaxInnerSize(Vec2::new(10_000.0, 10_000.0)));
        ctx.send_viewport_cmd(ViewportCommand::MinInnerSize(MIN_WINDOW_SIZE));
        ctx.send_viewport_cmd(ViewportCommand::InnerSize(WINDOW_SIZE));
        ctx.send_viewport_cmd(ViewportCommand::Visible(true));
        if let Some(position) = self.restore_position {
            ctx.send_viewport_cmd(ViewportCommand::OuterPosition(position));
        } else {
            center_window(ctx, WINDOW_SIZE);
        }
        ctx.send_viewport_cmd(ViewportCommand::Focus);
    }

    fn park_window(&self, ctx: &egui::Context) {
        // Invisible windows are suspended by winit/eframe on Windows. Keep a
        // 1x1 visible viewport far outside the virtual desktop instead, so the
        // app continues processing hotkeys and recorder completion events.
        if let Some(window) = self.window {
            crate::window::set_taskbar_visible(window, false);
        }
        ctx.send_viewport_cmd(ViewportCommand::Decorations(false));
        ctx.send_viewport_cmd(ViewportCommand::Resizable(false));
        ctx.send_viewport_cmd(ViewportCommand::MousePassthrough(true));
        ctx.send_viewport_cmd(ViewportCommand::MinInnerSize(Vec2::new(1.0, 1.0)));
        ctx.send_viewport_cmd(ViewportCommand::MaxInnerSize(Vec2::new(1.0, 1.0)));
        ctx.send_viewport_cmd(ViewportCommand::InnerSize(Vec2::new(1.0, 1.0)));
        ctx.send_viewport_cmd(ViewportCommand::OuterPosition(Pos2::new(
            -32_000.0, -32_000.0,
        )));
        ctx.send_viewport_cmd(ViewportCommand::Visible(true));
    }

    fn show_status_bar(&self, ctx: &egui::Context) {
        // Reuse the root viewport and its capture-affinity setting.
        ctx.send_viewport_cmd(ViewportCommand::MousePassthrough(false));
        ctx.send_viewport_cmd(ViewportCommand::Transparent(false));
        ctx.send_viewport_cmd(ViewportCommand::MaxInnerSize(Vec2::new(420.0, 96.0)));
        ctx.send_viewport_cmd(ViewportCommand::MinInnerSize(Vec2::new(420.0, 96.0)));
        ctx.send_viewport_cmd(ViewportCommand::InnerSize(Vec2::new(420.0, 96.0)));
        ctx.send_viewport_cmd(ViewportCommand::Resizable(false));
        ctx.send_viewport_cmd(ViewportCommand::Decorations(false));
        ctx.send_viewport_cmd(ViewportCommand::WindowLevel(WindowLevel::AlwaysOnTop));
        ctx.send_viewport_cmd(ViewportCommand::Visible(true));
        if let Some(position) = self.restore_position {
            ctx.send_viewport_cmd(ViewportCommand::OuterPosition(
                position + (WINDOW_SIZE - Vec2::new(420.0, 96.0)) / 2.0,
            ));
        } else {
            center_window(ctx, Vec2::new(420.0, 96.0));
        }
    }

    fn poll_background(&mut self, ctx: &egui::Context, allow_hotkey_start: bool) {
        self.poll_microphones();
        while let Ok(event) = self.hotkey_events.try_recv() {
            match event {
                HotkeyEvent::Start if allow_hotkey_start => self.begin_recording(ctx),
                HotkeyEvent::Start => {}
                HotkeyEvent::Stop => self.request_stop(ctx),
                HotkeyEvent::Availability { start, stop } => {
                    debug_log::write(format!(
                        "hotkey availability received: start={start}, stop={stop}"
                    ));
                    self.stop_hotkey_available = stop;
                    if !stop {
                        self.warning = Some(msg!(
                            "The stop shortcut is unavailable. Restart Hidden Screen Recorder.",
                            "Клавиша остановки недоступна. Перезапустите Hidden Screen Recorder."
                        ));
                    } else if !start {
                        self.warning = Some(msg!(
                            "The start shortcut is unavailable. Use the record button.",
                            "Клавиша старта недоступна. Запускайте запись кнопкой."
                        ));
                    }
                }
            }
        }

        let recorder_events = self
            .recorder_events
            .as_ref()
            .map(|receiver| receiver.try_iter().collect::<Vec<_>>())
            .unwrap_or_default();
        for event in recorder_events {
            self.handle_event(event, ctx);
        }
        self.start_capture_if_ready(ctx);
    }

    fn save_config(&mut self) {
        if let Err(error) = self.config.save() {
            debug_log::write(error.to_string());
            self.warning = Some(error);
        }
    }

    fn render_hotkey_settings(&mut self, ui: &mut egui::Ui) {
        let language = self.config.language();
        if let Some(error) = &self.hotkey_error {
            ui.colored_label(ERROR, error.render(language));
        }
        for (target, label, shortcut) in [
            (
                HotkeyTarget::Start,
                language.text("Start recording", "Начать запись"),
                self.config.shortcuts.start,
            ),
            (
                HotkeyTarget::Stop,
                language.text("Stop recording", "Остановить запись"),
                self.config.shortcuts.stop,
            ),
        ] {
            row(ui, label, |ui| {
                let editing = self.editing_hotkey == Some(target);
                let text = if editing {
                    language
                        .text("Press a shortcut…", "Нажмите сочетание…")
                        .into()
                } else {
                    shortcut.label()
                };
                let mut button = Button::new(text).selected(editing);
                if editing && self.hotkey_error.is_some() {
                    button = button
                        .fill(Color32::from_rgb(77, 30, 38))
                        .stroke(Stroke::new(1.0_f32, ERROR));
                }
                let response = ui
                    .add_sized([200.0, 28.0], button)
                    .on_hover_text(language.text(
                        "Click to change this shortcut",
                        "Нажмите, чтобы изменить сочетание клавиш",
                    ));
                if response.clicked() {
                    self.editing_hotkey = Some(target);
                    self.hotkey_error = None;
                    self.hotkey_controller.suspend(true);
                    response.surrender_focus();
                }
            });
        }
    }

    fn cancel_hotkey_edit(&mut self) {
        self.editing_hotkey = None;
        self.hotkey_error = None;
        self.hotkey_controller.suspend(false);
    }

    fn capture_hotkey(&mut self, ctx: &egui::Context) {
        let Some(target) = self.editing_hotkey else {
            return;
        };
        // Consume input before rendering, so assigning Enter/Space cannot
        // simultaneously activate a focused control.
        let events = ctx.input_mut(|input| {
            let events = input.events.clone();
            input
                .events
                .retain(|event| !matches!(event, egui::Event::Key { .. } | egui::Event::Text(_)));
            events
        });
        for event in events {
            let egui::Event::Key {
                key,
                physical_key,
                pressed: true,
                repeat: false,
                modifiers,
            } = event
            else {
                continue;
            };
            if key == egui::Key::Escape {
                self.cancel_hotkey_edit();
                return;
            }
            let Some(shortcut) = Shortcut::from_input(physical_key.unwrap_or(key), modifiers)
            else {
                self.hotkey_error = Some(msg!(
                    "Use a modifier and a key, or F1–F24.",
                    "Используйте модификатор и клавишу или F1–F24."
                ));
                continue;
            };
            if !hotkeys::input_matches_polling(shortcut) {
                self.hotkey_error = Some(msg!(
                    "Unsupported key: use the main keyboard and check your layout.",
                    "Клавиша не поддерживается: используйте основной блок клавиатуры; проверьте раскладку."
                ));
                continue;
            }
            let mut shortcuts = self.config.shortcuts;
            match target {
                HotkeyTarget::Start => shortcuts.start = shortcut,
                HotkeyTarget::Stop => shortcuts.stop = shortcut,
            }
            if !shortcuts.is_valid() {
                self.hotkey_error = Some(msg!(
                    "Start and stop shortcuts must be different.",
                    "Для старта и остановки нужны разные сочетания."
                ));
                continue;
            }
            self.config.shortcuts = shortcuts;
            self.hotkey_controller.update(shortcuts);
            self.save_config();
            self.cancel_hotkey_edit();
            return;
        }
    }

    fn render_audio_settings(&mut self, ui: &mut egui::Ui) {
        let language = self.config.language();
        let button = ui.button(language.text("Audio settings…", "Настройки звука…"));
        let open_id = button.id.with("audio_settings_open");
        let mut open = ui
            .ctx()
            .data(|data| data.get_temp::<bool>(open_id).unwrap_or(false));
        if button.clicked() {
            open = !open;
        }
        // egui has a single memory-managed popup slot. Keep this panel's state
        // separately so opening the ComboBox cannot replace its parent popup.
        let close_behavior = if egui::Popup::is_any_open(ui.ctx()) {
            egui::PopupCloseBehavior::IgnoreClicks
        } else {
            egui::PopupCloseBehavior::CloseOnClickOutside
        };
        egui::Popup::menu(&button)
            .open_bool(&mut open)
            .close_behavior(close_behavior)
            .show(|ui| {
                ui.set_min_width(270.0);
                ui.label(language.text("48 kHz · stereo", "48 кГц · стерео"));
                ui.horizontal(|ui| {
                    ui.label(language.text("Buffer", "Буфер"));
                    ComboBox::from_id_salt("audio_buffer")
                        .selected_text(format!("{} {}", self.config.audio_buffer_ms, language.text("ms", "мс")))
                        .show_ui(ui, |ui| {
                            for (ms, en, ru) in [(100, "100 ms · low latency", "100 мс · малая задержка"), (250, "250 ms · recommended", "250 мс · рекомендуется"), (500, "500 ms · extra buffering", "500 мс · увеличенный буфер")] {
                                ui.selectable_value(&mut self.config.audio_buffer_ms, ms, language.text(en, ru));
                            }
                        })
                        .response.on_hover_text(language.text("More buffering tolerates scheduling delays without shifting audio in the recording.", "Больший буфер помогает при нагрузке и не сдвигает звук в записи."));
                });
                ui.checkbox(
                    &mut self.config.audio_device_timestamps,
                    language.text("Use device timestamps", "Метки времени устройства"),
                )
                .on_hover_text(language.text(
                    "Turn off if your audio driver supplies unstable timestamps.",
                    "Отключите, если драйвер звука передаёт нестабильные метки времени.",
                ));
            });
        ui.ctx().data_mut(|data| data.insert_temp(open_id, open));
    }

    fn render_idle(&mut self, ctx: &egui::Context) {
        let language = self.config.language();
        let idle = matches!(self.state, AppState::Idle);
        egui::TopBottomPanel::bottom("actions")
            .exact_height(if self.warning.is_some() { 114.0 } else { 82.0 })
            .frame(
                Frame::new()
                    .fill(BACKGROUND)
                    .inner_margin(Margin::symmetric(28, 18)),
            )
            .show(ctx, |ui| {
                if let Some(warning) = &self.warning {
                    let dismiss = warning_box(ui, warning.render(language), language);
                    if dismiss {
                        self.warning = None;
                    }
                    ui.add_space(6.0);
                }
                let (label, color) = if idle {
                    (language.text("Start recording", "Начать запись"), ACCENT)
                } else {
                    (
                        language.text("Stop recording", "Остановить запись"),
                        Color32::from_rgb(210, 65, 73),
                    )
                };
                let available_width = ui.available_width();
                let enabled = !idle
                    || (self.stop_hotkey_available
                        && self.editing_hotkey.is_none()
                        && self.hotkey_error.is_none());
                if ui
                    .add_enabled_ui(enabled, |ui| {
                        // add_sized centers the button contents in both axes.
                        ui.add_sized(
                            [available_width, 46.0],
                            Button::new(RichText::new(label).size(16.0).strong())
                                .fill(color)
                                .corner_radius(10.0),
                        )
                    })
                    .inner
                    .clicked()
                {
                    if idle {
                        self.begin_recording(ctx);
                    } else {
                        self.request_stop(ctx);
                    }
                }
            });
        egui::CentralPanel::default()
            .frame(Frame::new().fill(BACKGROUND).inner_margin(Margin::symmetric(28, 22)))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.label(RichText::new(WINDOW_TITLE).size(28.0).strong().color(TEXT));
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        github_button(ui);
                        if ui.add_sized([66.0, 28.0], Button::new(language.text("EN / RU", "RU / EN")))
                            .on_hover_text(language.text("Switch to Russian", "Переключить на английский")).clicked() {
                            self.config.language = Some(language.toggled());
                            self.monitors = enumerate_monitors(self.config.language());
                            self.save_config();
                            ctx.request_repaint();
                        }
                        if let Some(path) = &self.last_output
                            && ui.link(language.text("Last recording", "Последняя запись")).clicked() {
                            let _ = Command::new("explorer.exe")
                                .arg(format!("/select,{}", path.display())).spawn();
                        }
                    });
                });
                ui.add_space(20.0);
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    ui.set_min_width(ui.available_width());
                    ui.add_enabled_ui(idle, |ui| {
                        let card_height = (ui.available_height() - 24.0).clamp(0.0, 368.0);
                        ui.columns(2, |columns| {
                        let ui = &mut columns[0];
                        settings_card(ui, card_height, |ui| {
                            ui.spacing_mut().item_spacing.y = 4.0;
                            ui.spacing_mut().button_padding.y = 5.0;
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(language.text("Video & audio", "Видео и звук")).size(14.0).strong().color(TEXT));
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    if self.config.system_audio || self.config.microphone {
                                        self.render_audio_settings(ui);
                                    }
                                });
                            });
                            ui.add_space(4.0);
                            row(ui, language.text("Display", "Экран"), |ui| {
                                let selected = self.monitors.iter()
                                    .find(|monitor| monitor.index == self.config.monitor_index)
                                    .map(|monitor| monitor.label.as_str()).unwrap_or(language.text("Primary display", "Основной экран"));
                                ComboBox::from_id_salt("monitor").selected_text(selected).width(230.0)
                                    .show_ui(ui, |ui| {
                                        for monitor in &self.monitors {
                                            ui.selectable_value(&mut self.config.monitor_index, monitor.index, &monitor.label);
                                        }
                                    });
                            });
                            row(ui, language.text("Frame rate", "Частота кадров"), |ui| {
                                for fps in [15, 30, 60] {
                                    ui.selectable_value(&mut self.config.fps, fps, format!("{fps} FPS"));
                                }
                                ui.add(egui::DragValue::new(&mut self.config.fps).range(1..=240).suffix(" FPS"));
                            });
                            row(ui, language.text("Quality", "Качество"), |ui| {
                                ui.add(Slider::new(&mut self.config.bitrate_mbps, 1..=100).suffix(language.text(" Mbps", " Мбит/с")));
                            });
                            toggle_row(ui, language.text("Record cursor", "Курсор в записи"), &mut self.config.show_cursor);
                            ui.add_space(6.0);
                            ui.separator();
                            ui.add_space(6.0);
                            toggle_row(ui, language.text("System audio", "Системный звук"), &mut self.config.system_audio);
                            if self.config.system_audio {
                                row(ui, language.text("System volume", "Громкость системы"), |ui| {
                                    ui.add(Slider::new(&mut self.config.system_volume, 0.0..=1.5)
                                        .custom_formatter(|value, _| format!("{:.0}%", value * 100.0)));
                                });
                            }
                            toggle_row(ui, language.text("Microphone", "Микрофон"), &mut self.config.microphone);
                            if self.config.microphone {
                                self.render_microphone_selector(ui, ctx);
                                row(ui, language.text("Microphone volume", "Громкость микрофона"), |ui| {
                                    ui.add(Slider::new(&mut self.config.microphone_volume, 0.0..=1.5)
                                        .custom_formatter(|value, _| format!("{:.0}%", value * 100.0)));
                                });
                            }

                        });
                        let ui = &mut columns[1];
                        let invalid_shortcut = self.hotkey_error.is_some();
                        settings_card_state(ui, invalid_shortcut, card_height, |ui| {
                            ui.spacing_mut().item_spacing.y = 4.0;
                            ui.spacing_mut().button_padding.y = 5.0;
                            section_title(ui, language.text("App & storage", "Приложение и сохранение"));
                            ui.label(RichText::new(language.text("Duration", "Длительность")).size(13.0).color(MUTED));
                            ui.scope(|ui| {
                                let value_width = 140.0;
                                ui.spacing_mut().slider_width = (ui.available_width() - value_width - ui.spacing().item_spacing.x).max(100.0);
                                ui.spacing_mut().interact_size = Vec2::new(value_width, 28.0);
                                ui.add(Slider::new(&mut self.config.duration_minutes, 0..=1440)
                                    .custom_formatter(|value, _| {
                                        if value < 0.5 { language.text("Unlimited", "Без ограничения").into() }
                                        else { format!("{} {}", value.round() as u32, language.text("min", "мин")) }
                                    }));
                            });
                            ui.add_space(10.0);
                            ui.label(RichText::new(language.text("Save location", "Хранилище")).size(13.0).color(MUTED));
                            ui.horizontal(|ui| {
                                let path = self.config.output_dir.to_string_lossy();
                                ui.label(RichText::new(ellipsis(&path, 34)).size(12.0).color(MUTED))
                                    .on_hover_text(path.as_ref());
                                if ui.button(language.text("Browse…", "Выбрать…")).clicked()
                                    && let Some(folder) = rfd::FileDialog::new()
                                        .set_directory(&self.config.output_dir).pick_folder() {
                                    self.config.output_dir = folder;
                                }
                            });
                            ui.horizontal(|ui| {
                                ui.label(language.text("Filename", "Имя файла"));
                                let width = (ui.available_width() - 44.0).max(80.0);
                                let now = chrono::Local::now().naive_local();
                                let example = format!("{}.mp4", crate::filename::render(&self.config.filename_template, now, self.config.monitor_index));
                                let field = ui.add(egui::TextEdit::singleline(&mut self.config.filename_template)
                                    .desired_width(width).char_limit(120)
                                    .hint_text(crate::filename::DEFAULT_TEMPLATE))
                                    .on_hover_text(format!("{}: {example}", language.text("Example", "Пример")));
                                let (help_rect, help) = ui.allocate_exact_size(
                                    Vec2::new(22.0, field.rect.height()), egui::Sense::hover());
                                ui.painter().text(
                                    egui::pos2(help_rect.center().x, field.rect.center().y),
                                    egui::Align2::CENTER_CENTER, "?", egui::FontId::proportional(13.0),
                                    if help.hovered() { TEXT } else { MUTED });
                                help.on_hover_ui(|ui| {
                                        ui.set_max_width(380.0);
                                        ui.strong(language.text("Filename variables", "Переменные имени файла"));
                                        egui::Grid::new("filename_variables").num_columns(3).show(ui, |ui| {
                                            for (tag, format, en, ru) in crate::filename::VARIABLES {
                                                ui.monospace(tag);
                                                ui.label(language.text(en, ru));
                                                ui.monospace(now.format(format).to_string());
                                                ui.end_row();
                                            }
                                            ui.monospace("{monitor}");
                                            ui.label(language.text("Display number", "Номер экрана"));
                                            ui.monospace(self.config.monitor_index.to_string());
                                            ui.end_row();
                                        });
                                        ui.separator();
                                        ui.label(language.text("Local time at recording start. The .mp4 extension is added automatically; duplicate names get _1, _2, … . Unsupported characters become _; unknown tags stay as text.", "Местное время начала записи. Расширение .mp4 добавляется автоматически; при совпадении имён — _1, _2, … . Недопустимые символы заменяются на _; неизвестные теги остаются текстом."));
                                        ui.label(format!("{}: {example}", language.text("Example", "Пример")));
                                    });
                            });
                            ui.add_space(5.0);
                            ui.separator();
                            ui.add_space(5.0);
                            toggle_row(ui, language.text("Hide window while recording", "Скрывать окно при записи"), &mut self.config.hide_window)
                                .on_hover_text(language.text("Hide the window and taskbar icon while recording.", "Скрывает окно и значок на панели задач во время записи."));
                            ui.add_space(3.0);
                            if ui.checkbox(&mut self.config.logging_enabled, language.text("Write diagnostic logs", "Записывать диагностические логи"))
                                .on_hover_text(language.text("Write hidden-screen-recorder-debug.log next to the app. Disabled by default.", "Файл hidden-screen-recorder-debug.log рядом с программой. По умолчанию выключено.")).changed() {
                                debug_log::set_enabled(self.config.logging_enabled);
                                debug_log::write("diagnostic logging enabled");
                            }
                            ui.add_space(8.0);
                            ui.separator();
                            ui.add_space(6.0);
                            self.render_hotkey_settings(ui);
                        });
                        });
                    });
                });
            });
    }

    fn render_status_bar(&self, ctx: &egui::Context) {
        let language = self.config.language();
        egui::CentralPanel::default()
            .frame(
                Frame::new()
                    .fill(SURFACE)
                    .stroke(Stroke::new(1.0_f32, BORDER))
                    .corner_radius(14.0)
                    .inner_margin(Margin::symmetric(18, 16)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.add(egui::Spinner::new().size(26.0).color(ACCENT));
                    ui.add_space(8.0);
                    ui.vertical(|ui| {
                        ui.label(
                            RichText::new(language.text("Saving recording…", "Сохраняю запись…"))
                                .size(17.0)
                                .strong()
                                .color(Color32::from_rgb(237, 241, 252)),
                        );
                        ui.label(
                            RichText::new(
                                language.text(
                                    "Please wait, finalizing MP4.",
                                    "Подождите, завершаю MP4.",
                                ),
                            )
                            .size(12.0)
                            .color(MUTED),
                        );
                    });
                });
            });
    }
}

impl eframe::App for HiddenScreenRecorderApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if ctx.input(|input| input.viewport().close_requested()) {
            self.close_pending = true;
            if !matches!(self.state, AppState::Idle) {
                ctx.send_viewport_cmd(ViewportCommand::CancelClose);
                self.request_stop(ctx);
            } else {
                self.save_config();
            }
        }
        let allow_hotkey_start = self.editing_hotkey.is_none() && !self.close_pending;
        self.capture_hotkey(ctx);
        self.poll_background(ctx, allow_hotkey_start);

        if self.close_pending && matches!(self.state, AppState::Idle) {
            self.save_config();
            ctx.send_viewport_cmd(ViewportCommand::Close);
        }
        match self.state {
            AppState::Idle => self.render_idle(ctx),
            AppState::Stopping => self.render_status_bar(ctx),
            AppState::Preparing(_) | AppState::Starting | AppState::Recording => {
                if !self.config.hide_window {
                    self.render_idle(ctx);
                }
            }
        }

        // A hidden viewport still has to process hotkeys and recorder events.
        ctx.request_repaint_after(Duration::from_millis(80));
    }
}

impl Drop for HiddenScreenRecorderApp {
    fn drop(&mut self) {
        self.hotkey_controller.shutdown();
        if let Some(worker) = self.hotkey_worker.take() {
            let _ = worker.join();
        }
        self.recorder.shutdown();
        self.save_config();
    }
}

fn center_window(ctx: &egui::Context, size: Vec2) {
    if let Some(monitor_size) = ctx.input(|input| input.viewport().monitor_size) {
        ctx.send_viewport_cmd(ViewportCommand::OuterPosition(Pos2::new(
            ((monitor_size.x - size.x) / 2.0).max(0.0),
            ((monitor_size.y - size.y) / 2.0).max(0.0),
        )));
    }
}

fn enumerate_monitors(language: Language) -> Vec<MonitorInfo> {
    let mut result = Monitor::enumerate()
        .unwrap_or_default()
        .into_iter()
        .enumerate()
        .map(|(position, monitor)| {
            let index = monitor.index().unwrap_or(position + 1);
            let name = monitor
                .name()
                .unwrap_or_else(|_| format!("{} {index}", language.text("Display", "Экран")));
            let dimensions = match (monitor.width(), monitor.height()) {
                (Ok(width), Ok(height)) => format!(" — {width}×{height}"),
                _ => String::new(),
            };
            MonitorInfo {
                index,
                label: format!("{name}{dimensions}"),
            }
        })
        .collect::<Vec<_>>();

    if result.is_empty() {
        result.push(MonitorInfo {
            index: 1,
            label: language.text("Primary display", "Основной экран").into(),
        });
    }
    result
}

const BACKGROUND: Color32 = Color32::from_rgb(20, 20, 22);
const SURFACE: Color32 = Color32::from_rgb(30, 30, 33);
const BORDER: Color32 = Color32::from_rgb(49, 49, 53);
const TEXT: Color32 = Color32::from_rgb(239, 239, 242);
const MUTED: Color32 = Color32::from_rgb(151, 151, 160);
const ERROR: Color32 = Color32::from_rgb(245, 85, 96);
const ACCENT: Color32 = Color32::from_rgb(10, 115, 235);

fn configure_style(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();
    style.spacing.item_spacing = Vec2::new(10.0, 5.0);
    style.spacing.button_padding = Vec2::new(12.0, 6.0);
    style.spacing.slider_width = 170.0;
    style.spacing.interact_size.y = 28.0;
    style.visuals = egui::Visuals::dark();
    style.visuals.popup_shadow = egui::epaint::Shadow::NONE;
    style.visuals.override_text_color = Some(TEXT);
    style.visuals.panel_fill = BACKGROUND;
    style.visuals.window_fill = SURFACE;
    style.visuals.extreme_bg_color = BACKGROUND;
    style.visuals.faint_bg_color = SURFACE;
    style.visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, BORDER);
    style.visuals.widgets.inactive.bg_fill = Color32::from_rgb(43, 43, 47);
    style.visuals.widgets.inactive.weak_bg_fill = Color32::from_rgb(43, 43, 47);
    style.visuals.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, BORDER);
    style.visuals.widgets.hovered.bg_fill = Color32::from_rgb(59, 59, 65);
    style.visuals.widgets.hovered.weak_bg_fill = Color32::from_rgb(59, 59, 65);
    style.visuals.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, MUTED);
    style.visuals.widgets.active.bg_fill = ACCENT;
    style.visuals.selection.bg_fill = ACCENT;
    style.visuals.selection.stroke = Stroke::new(1.0_f32, TEXT);
    style.visuals.hyperlink_color = Color32::from_rgb(93, 166, 255);
    for widget in [
        &mut style.visuals.widgets.inactive,
        &mut style.visuals.widgets.hovered,
        &mut style.visuals.widgets.active,
        &mut style.visuals.widgets.noninteractive,
    ] {
        widget.corner_radius = egui::CornerRadius::same(7);
    }
    ctx.set_style(style);
}

fn settings_card(ui: &mut egui::Ui, height: f32, content: impl FnOnce(&mut egui::Ui)) {
    settings_card_state(ui, false, height, content);
}

fn settings_card_state(
    ui: &mut egui::Ui,
    invalid: bool,
    height: f32,
    content: impl FnOnce(&mut egui::Ui),
) {
    Frame::new()
        .fill(if invalid {
            Color32::from_rgb(43, 26, 30)
        } else {
            SURFACE
        })
        .stroke(Stroke::new(1.0_f32, if invalid { ERROR } else { BORDER }))
        .corner_radius(12.0)
        .inner_margin(Margin::symmetric(18, 12))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.set_min_height(height);
            content(ui);
        });
}

fn section_title(ui: &mut egui::Ui, title: &str) {
    ui.label(RichText::new(title).size(14.0).strong().color(TEXT));
    ui.add_space(5.0);
}

fn row(ui: &mut egui::Ui, label: &str, content: impl FnOnce(&mut egui::Ui)) {
    ui.horizontal(|ui| {
        ui.set_min_height(26.0);
        ui.add_sized(
            [170.0, 24.0],
            egui::Label::new(RichText::new(label).size(13.0)).halign(Align::Min),
        );
        content(ui);
    });
}

fn toggle_row(ui: &mut egui::Ui, label: &str, value: &mut bool) -> egui::Response {
    ui.horizontal(|ui| {
        ui.set_min_height(26.0);
        ui.label(RichText::new(label).size(13.0));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            toggle(ui, value, label)
        })
        .inner
    })
    .inner
}

fn toggle(ui: &mut egui::Ui, value: &mut bool, label: &str) -> egui::Response {
    let (rect, mut response) = ui.allocate_exact_size(Vec2::new(38.0, 22.0), egui::Sense::click());
    if response.clicked() {
        *value = !*value;
        response.mark_changed();
    }
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::Checkbox, ui.is_enabled(), *value, label)
    });
    let animation = ui.ctx().animate_bool_responsive(response.id, *value);
    let track = if *value {
        ACCENT
    } else {
        Color32::from_rgb(72, 72, 79)
    };
    ui.painter().rect_filled(rect, 11.0, track);
    let x = egui::lerp((rect.left() + 11.0)..=(rect.right() - 11.0), animation);
    ui.painter()
        .circle_filled(Pos2::new(x, rect.center().y), 8.0, Color32::WHITE);
    if response.has_focus() {
        ui.painter().rect_stroke(
            rect.expand(3.0),
            13.0,
            Stroke::new(1.0_f32, TEXT),
            egui::StrokeKind::Outside,
        );
    }
    response
}

fn github_button(ui: &mut egui::Ui) {
    let id = egui::Id::new("github_texture");
    let cached = ui
        .ctx()
        .data(|data| data.get_temp::<egui::TextureHandle>(id));
    let texture = cached.unwrap_or_else(|| {
        let texture = ui.ctx().load_texture(
            "github",
            egui::ColorImage::from_rgba_unmultiplied(
                [64, 64],
                include_bytes!("assets/github.rgba"),
            ),
            egui::TextureOptions::LINEAR,
        );
        ui.ctx()
            .data_mut(|data| data.insert_temp(id, texture.clone()));
        texture
    });
    if ui
        .add(
            Button::image(egui::Image::new(&texture).fit_to_exact_size(Vec2::splat(22.0)))
                .frame(false)
                .min_size(Vec2::splat(34.0)),
        )
        .on_hover_text("GitHub · brevzor/hidden-screen-recorder")
        .clicked()
    {
        unsafe {
            windows::Win32::UI::Shell::ShellExecuteW(
                None,
                w!("open"),
                w!("https://github.com/brevzor/hidden-screen-recorder"),
                None,
                None,
                windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL,
            );
        }
    }
}

fn warning_box(ui: &mut egui::Ui, text: &str, language: Language) -> bool {
    Frame::new()
        .fill(Color32::from_rgb(33, 30, 26))
        .corner_radius(6.0)
        .inner_margin(Margin::symmetric(8, 4))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                ui.spacing_mut().button_padding = Vec2::ZERO;
                ui.spacing_mut().interact_size.y = 18.0;
                ui.label(
                    RichText::new("⚠")
                        .size(12.0)
                        .color(Color32::from_rgb(211, 166, 93)),
                );
                let width = (ui.available_width() - 24.0).max(0.0);
                ui.allocate_ui_with_layout(
                    Vec2::new(width, 18.0),
                    Layout::left_to_right(Align::Center),
                    |ui| {
                        ui.add(
                            egui::Label::new(RichText::new(text).size(11.0).color(MUTED))
                                .truncate(),
                        )
                    },
                )
                .inner
                .on_hover_text(text);
                ui.add_sized(
                    [18.0, 18.0],
                    Button::new(RichText::new("×").size(13.0).color(MUTED)).frame(false),
                )
                .on_hover_text(language.text("Dismiss", "Закрыть"))
                .clicked()
            })
            .inner
        })
        .inner
}

fn ellipsis(text: &str, maximum_chars: usize) -> String {
    let chars = text.chars().collect::<Vec<_>>();
    if chars.len() <= maximum_chars {
        return text.to_owned();
    }
    format!(
        "…{}",
        chars[chars.len() - maximum_chars..]
            .iter()
            .collect::<String>()
    )
}
