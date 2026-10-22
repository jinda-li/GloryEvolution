mod api;
mod audio;
mod history;
mod optimizer;
mod paste;
mod settings;
mod stt;
mod text_filter;

use std::{
    str::FromStr,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Mutex,
    },
    time::{Duration, Instant},
};

use serde::Serialize;
use tauri::{
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Emitter, Manager, PhysicalPosition, State, WindowEvent,
};
use tauri_plugin_autostart::{MacosLauncher, ManagerExt};
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Shortcut, ShortcutState};
use tauri_plugin_opener::OpenerExt;
use thiserror::Error;

use settings::RecordMode;

type AppResult<T> = Result<T, AppError>;

/// Recordings are capped so a forgotten session cannot run up an API bill.
const MAX_RECORDING_SECONDS: u64 = 20 * 60;
/// The pill warns this long before the cap stops the take.
const LIMIT_WARNING_SECONDS: u64 = 60;
/// Past this length, Esc must be pressed twice so a stray press elsewhere
/// cannot throw away a long dictation.
const CANCEL_CONFIRM_AFTER_SECONDS: f64 = 15.0;
const CANCEL_CONFIRM_WINDOW: Duration = Duration::from_secs(2);
const MIN_RECORDING_SECONDS: f64 = 0.4;
/// RMS below this across the whole take means the mic heard nothing.
const SILENCE_PEAK: f32 = 0.004;
const AUTOSTART_FLAG: &str = "--minimized";

#[derive(Debug, Error)]
pub enum AppError {
    #[error("{0}")]
    Config(String),
    #[error("{0}")]
    Audio(String),
    #[error("{0}")]
    Api(String),
    /// Processing failed but the audio was kept; the user can retry it.
    #[error("{0}")]
    Recoverable(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Tauri(#[from] tauri::Error),
    #[error(transparent)]
    GlobalShortcut(#[from] tauri_plugin_global_shortcut::Error),
    #[error(transparent)]
    Clipboard(#[from] arboard::Error),
    #[error(transparent)]
    Hound(#[from] hound::Error),
    #[error("麦克风不可用：{0}")]
    CpalDefaultConfig(#[from] cpal::DefaultStreamConfigError),
    #[error("麦克风不可用：{0}")]
    CpalBuildStream(#[from] cpal::BuildStreamError),
    #[error("麦克风不可用：{0}")]
    CpalPlayStream(#[from] cpal::PlayStreamError),
}

impl serde::Serialize for AppError {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum DictationMode {
    #[default]
    Dictate,
    Translate,
}

/// Audio of the last take whose processing failed, held in memory (never on
/// disk) so a network hiccup does not cost the user a long dictation.
struct FailedTake {
    recording: audio::StoppedRecording,
    mode: DictationMode,
}

struct AppState {
    recording: Mutex<Option<audio::RecordingSession>>,
    session_mode: Mutex<DictationMode>,
    processing: AtomicBool,
    session_id: AtomicU64,
    registered_shortcuts: Mutex<Vec<Shortcut>>,
    cancel_shortcut_active: AtomicBool,
    cancel_armed_at: Mutex<Option<Instant>>,
    last_shortcut_toggle: Mutex<Instant>,
    shortcut_capture_mode: AtomicBool,
    failed_take: Mutex<Option<FailedTake>>,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            recording: Mutex::new(None),
            session_mode: Mutex::new(DictationMode::Dictate),
            processing: AtomicBool::new(false),
            session_id: AtomicU64::new(0),
            registered_shortcuts: Mutex::new(Vec::new()),
            cancel_shortcut_active: AtomicBool::new(false),
            cancel_armed_at: Mutex::new(None),
            last_shortcut_toggle: Mutex::new(Instant::now() - Duration::from_secs(1)),
            shortcut_capture_mode: AtomicBool::new(false),
            failed_take: Mutex::new(None),
        }
    }
}

fn cancel_shortcut() -> Shortcut {
    Shortcut::new(None, Code::Escape)
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProcessResult {
    raw_text: String,
    optimized_text: String,
    duration_seconds: f64,
    delivery: &'static str,
    warning: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct RecordingEvent {
    status: &'static str,
    message: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct AudioLevelEvent {
    level: f32,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProcessingProgressEvent {
    phase: &'static str,
    message: String,
    progress: f32,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct LatencyMetricsEvent {
    total_ms: u128,
    stt_ms: u128,
    optimize_ms: u128,
    paste_ms: u128,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct RecorderHintEvent {
    message: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct TextPreviewEvent {
    text: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct AppInfo {
    version: &'static str,
    default_system_prompt: String,
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

#[tauri::command]
fn load_settings(app: AppHandle) -> AppResult<settings::AppSettings> {
    settings::load(&app)
}

#[tauri::command]
fn save_settings(
    app: AppHandle,
    state: State<'_, AppState>,
    new_settings: settings::AppSettings,
) -> AppResult<()> {
    // Validate shortcuts before persisting so a typo cannot brick the hotkey.
    let shortcuts = parse_shortcuts(&new_settings)?;
    if shortcuts.translate.is_some() && shortcuts.translate == Some(shortcuts.dictate) {
        return Err(AppError::Config("翻译快捷键不能和听写快捷键相同。".to_string()));
    }
    settings::save(&app, &new_settings)?;
    register_shortcut_from_settings(&app, state.inner())?;
    apply_autostart(&app, new_settings.launch_at_login);
    Ok(())
}

#[tauri::command]
fn app_info() -> AppInfo {
    AppInfo {
        version: env!("CARGO_PKG_VERSION"),
        default_system_prompt: settings::default_system_prompt(),
    }
}

#[tauri::command]
async fn test_connection(new_settings: settings::AppSettings) -> AppResult<api::KeyStatus> {
    api::check_key(&new_settings).await
}

#[tauri::command]
fn list_input_devices() -> Vec<String> {
    audio::list_input_devices()
}

#[tauri::command]
fn get_history(app: AppHandle) -> AppResult<history::HistoryStore> {
    history::load(&app)
}

#[tauri::command]
fn delete_history_entry(app: AppHandle, id: String) -> AppResult<history::HistoryStore> {
    history::delete(&app, &id)
}

#[tauri::command]
fn clear_history(app: AppHandle) -> AppResult<history::HistoryStore> {
    history::clear(&app)
}

#[tauri::command]
fn open_external(app: AppHandle, url: String) -> AppResult<()> {
    if !url.starts_with("https://") {
        return Err(AppError::Config("只允许打开 https 链接。".to_string()));
    }
    app.opener()
        .open_url(url, None::<&str>)
        .map_err(|error| AppError::Config(error.to_string()))
}

#[tauri::command]
fn copy_text(text: String) -> AppResult<()> {
    paste::write_clipboard(&text)
}

#[tauri::command]
fn set_shortcut_capture_mode(state: State<'_, AppState>, capturing: bool) {
    state.shortcut_capture_mode.store(capturing, Ordering::SeqCst);
}

#[tauri::command]
fn start_recording(app: AppHandle, state: State<'_, AppState>) -> AppResult<()> {
    start_recording_inner(&app, state.inner(), DictationMode::Dictate)
}

#[tauri::command]
async fn stop_and_process(app: AppHandle, state: State<'_, AppState>) -> AppResult<()> {
    stop_and_process_inner(&app, state.inner()).await
}

#[tauri::command]
fn cancel_recording(app: AppHandle, state: State<'_, AppState>) -> AppResult<()> {
    cancel_recording_inner(&app, state.inner())
}

#[tauri::command]
async fn toggle_recording(app: AppHandle, state: State<'_, AppState>) -> AppResult<()> {
    toggle_recording_inner(&app, state.inner(), DictationMode::Dictate).await
}

#[tauri::command]
async fn retry_failed(app: AppHandle, state: State<'_, AppState>) -> AppResult<()> {
    retry_failed_inner(&app, state.inner()).await
}

#[tauri::command]
fn discard_failed(app: AppHandle, state: State<'_, AppState>) -> AppResult<()> {
    if let Ok(mut failed) = state.failed_take.lock() {
        *failed = None;
    }
    hide_recorder_window(&app)
}

#[tauri::command]
fn hide_recorder(app: AppHandle) -> AppResult<()> {
    hide_recorder_window(&app)
}

// ---------------------------------------------------------------------------
// Recording pipeline
// ---------------------------------------------------------------------------

fn is_recording(state: &AppState) -> bool {
    state
        .recording
        .lock()
        .map(|recording| recording.is_some())
        .unwrap_or(false)
}

fn start_recording_inner(app: &AppHandle, state: &AppState, mode: DictationMode) -> AppResult<()> {
    if state.processing.load(Ordering::SeqCst) {
        return Ok(());
    }

    let settings = settings::load(app)?;
    if settings.api_key.trim().is_empty() {
        show_main_window(app);
        let message = "请先在设置中填写 OpenRouter API Key。".to_string();
        emit_recording(app, "error", &message);
        return Err(AppError::Config(message));
    }

    let mut recording = state
        .recording
        .lock()
        .map_err(|_| AppError::Audio("录音状态锁定失败。".to_string()))?;
    if recording.is_some() {
        return Ok(());
    }

    let app_for_level = app.clone();
    let app_for_notice = app.clone();
    let last_emit = Mutex::new(Instant::now() - Duration::from_millis(100));
    let session = audio::start(
        &settings.input_device,
        std::sync::Arc::new(move |level| {
            if let Ok(mut last_emit) = last_emit.lock() {
                if last_emit.elapsed() >= Duration::from_millis(50) {
                    *last_emit = Instant::now();
                    let _ = app_for_level.emit("audio-level", AudioLevelEvent { level });
                }
            }
        }),
        std::sync::Arc::new(move |notice| {
            let message = match notice {
                audio::CaptureNotice::Interrupted => "麦克风中断，正在重新连接…",
                audio::CaptureNotice::Recovered => "麦克风已恢复，继续录音。",
            };
            emit_hint(&app_for_notice, message);
        }),
    )?;
    *recording = Some(session);
    drop(recording);
    if let Ok(mut session_mode) = state.session_mode.lock() {
        *session_mode = mode;
    }
    if let Ok(mut armed) = state.cancel_armed_at.lock() {
        *armed = None;
    }

    let session_id = state.session_id.fetch_add(1, Ordering::SeqCst) + 1;
    set_cancel_shortcut(app, state, true);
    show_recorder_window(app)?;
    let hint = match settings.record_mode {
        RecordMode::Toggle => "再按一次快捷键结束，Esc 取消。",
        RecordMode::Hold => "松开快捷键结束，Esc 取消。",
    };
    let hint = match mode {
        DictationMode::Dictate => format!("正在聆听，{hint}"),
        DictationMode::Translate => format!(
            "正在聆听（翻译为{}），{hint}",
            optimizer::language_name(&settings.translate_language)
        ),
    };
    emit_recording(app, "recording", &hint);
    spawn_recording_watchdog(app.clone(), session_id);
    Ok(())
}

fn spawn_recording_watchdog(app: AppHandle, session_id: u64) {
    tauri::async_runtime::spawn(async move {
        let still_running = |app: &AppHandle| {
            let state = app.state::<AppState>();
            state.session_id.load(Ordering::SeqCst) == session_id && is_recording(state.inner())
        };
        tokio::time::sleep(Duration::from_secs(MAX_RECORDING_SECONDS - LIMIT_WARNING_SECONDS)).await;
        if !still_running(&app) {
            return;
        }
        emit_hint(&app, "还剩 1 分钟，到时会自动结束并识别。");
        tokio::time::sleep(Duration::from_secs(LIMIT_WARNING_SECONDS)).await;
        if still_running(&app) {
            let state = app.state::<AppState>();
            let _ = stop_and_process_inner(&app, state.inner()).await;
        }
    });
}

async fn stop_and_process_inner(app: &AppHandle, state: &AppState) -> AppResult<()> {
    if state.processing.swap(true, Ordering::SeqCst) {
        return Ok(());
    }
    let session = match state.recording.lock() {
        Ok(mut recording) => recording.take(),
        Err(_) => None,
    };
    let Some(session) = session else {
        state.processing.store(false, Ordering::SeqCst);
        return Ok(());
    };
    set_cancel_shortcut(app, state, false);
    let mode = state.session_mode.lock().map(|mode| *mode).unwrap_or_default();

    let result = process_session(app, state, session, mode).await;
    state.processing.store(false, Ordering::SeqCst);
    report_failure(app, &result);
    result.map(|_| ())
}

fn report_failure<T>(app: &AppHandle, result: &AppResult<T>) {
    match result {
        // "failed" keeps the pill open with a retry button instead of fading out.
        Err(error @ AppError::Recoverable(_)) => emit_recording(app, "failed", &error.to_string()),
        Err(error) => emit_recording(app, "error", &error.to_string()),
        Ok(_) => {}
    }
}

/// Returns `None` when the take was too short to be worth transcribing.
async fn process_session(
    app: &AppHandle,
    state: &AppState,
    session: audio::RecordingSession,
    mode: DictationMode,
) -> AppResult<Option<ProcessResult>> {
    emit_recording(app, "processing", "正在识别…");
    emit_progress(app, "stopping", "准备音频", 0.1);
    let too_short = session.elapsed_seconds() < MIN_RECORDING_SECONDS;
    let stopped = tauri::async_runtime::spawn_blocking(move || session.stop())
        .await
        .map_err(|error| AppError::Audio(error.to_string()))??;

    if too_short || stopped.duration_seconds < MIN_RECORDING_SECONDS {
        hide_recorder_window(app)?;
        emit_recording(app, "idle", "录音太短，已忽略。");
        return Ok(None);
    }
    if stopped.peak < SILENCE_PEAK {
        return Err(AppError::Audio(
            "没有检测到声音，请检查麦克风是否静音或选错设备。".to_string(),
        ));
    }

    transcribe_and_deliver(app, state, stopped, mode).await.map(Some)
}

async fn retry_failed_inner(app: &AppHandle, state: &AppState) -> AppResult<()> {
    if is_recording(state) || state.processing.swap(true, Ordering::SeqCst) {
        return Ok(());
    }
    let failed = state.failed_take.lock().ok().and_then(|mut failed| failed.take());
    let Some(failed) = failed else {
        state.processing.store(false, Ordering::SeqCst);
        return Err(AppError::Config("没有需要重试的录音。".to_string()));
    };

    let result = async {
        show_recorder_window(app)?;
        emit_recording(app, "processing", "正在重新识别…");
        transcribe_and_deliver(app, state, failed.recording, failed.mode).await
    }
    .await;
    state.processing.store(false, Ordering::SeqCst);
    report_failure(app, &result);
    result.map(|_| ())
}

async fn transcribe_and_deliver(
    app: &AppHandle,
    state: &AppState,
    recording: audio::StoppedRecording,
    mode: DictationMode,
) -> AppResult<ProcessResult> {
    let total_start = Instant::now();
    let settings = settings::load(app)?;

    emit_progress(app, "stt", "语音识别", 0.3);
    let stt_start = Instant::now();
    let transcribed = stt::transcribe(&recording.samples, &settings, |done, total| {
        if total > 1 {
            let progress = 0.3 + 0.3 * done as f32 / total as f32;
            emit_progress(app, "stt", &format!("语音识别 {done}/{total}"), progress);
        }
    })
    .await;
    let raw_text = match transcribed {
        Ok(text) => text,
        Err(error) => {
            let message = error.to_string();
            if let Ok(mut failed) = state.failed_take.lock() {
                *failed = Some(FailedTake { recording, mode });
            }
            return Err(AppError::Recoverable(message));
        }
    };
    let stt_ms = stt_start.elapsed().as_millis();
    let duration_seconds = recording.duration_seconds;
    drop(recording);

    let human_text = text_filter::clean_human_speech_text(&raw_text, settings.language.trim())?;

    let optimize_start = Instant::now();
    let mut warning = None;
    let mut polished = false;
    let translated_to = (mode == DictationMode::Translate).then(|| settings.translate_language.trim().to_string());
    let final_text = if let Some(target) = &translated_to {
        emit_progress(app, "optimize", "翻译", 0.65);
        match optimizer::translate(&human_text, target, &settings).await {
            Ok(text) if !text.trim().is_empty() => {
                polished = true;
                text
            }
            Ok(_) => human_text.clone(),
            Err(error) => {
                warning = Some(format!("翻译失败，已输出原始识别：{error}"));
                human_text.clone()
            }
        }
    } else if settings.polish_enabled && !settings.llm_model.trim().is_empty() {
        emit_progress(app, "optimize", "智能润色", 0.65);
        match optimizer::optimize(&human_text, &settings).await {
            Ok(text) if !text.trim().is_empty() => {
                polished = true;
                text
            }
            Ok(_) => human_text.clone(),
            // Never lose a dictation because the polish step failed.
            Err(error) => {
                warning = Some(format!("润色失败，已输出原始识别：{error}"));
                human_text.clone()
            }
        }
    } else {
        human_text.clone()
    };
    let optimize_ms = optimize_start.elapsed().as_millis();

    // Saved before typing so the text survives even if delivery goes wrong.
    if let Ok(store) = history::record(
        app,
        settings.history_enabled,
        &human_text,
        &final_text,
        duration_seconds,
        polished,
        translated_to.as_deref(),
    ) {
        let _ = app.emit("history-updated", &store);
    }

    emit_progress(app, "paste", "输出文本", 0.92);
    let paste_start = Instant::now();
    let delivery = {
        let settings = settings.clone();
        let text = final_text.clone();
        tauri::async_runtime::spawn_blocking(move || paste::deliver_text(&settings, &text))
            .await
            .map_err(|error| AppError::Audio(error.to_string()))?
            // Typing failed outright: fall back to the copy preview.
            .unwrap_or(paste::TextDelivery::NeedsCopy)
    };
    let paste_ms = paste_start.elapsed().as_millis();

    let delivery_label = match delivery {
        paste::TextDelivery::Inserted => {
            hide_recorder_window(app)?;
            emit_recording(app, "done", "已输入。");
            "inserted"
        }
        paste::TextDelivery::NeedsCopy => {
            emit_text_preview(app, &final_text);
            emit_recording(app, "preview", "没有找到输入框，点击复制。");
            "needsCopy"
        }
    };
    emit_progress(app, "done", "完成", 1.0);

    let result = ProcessResult {
        raw_text: human_text,
        optimized_text: final_text,
        duration_seconds,
        delivery: delivery_label,
        warning,
    };
    let _ = app.emit("process-result", &result);
    let _ = app.emit(
        "latency-metrics",
        LatencyMetricsEvent {
            total_ms: total_start.elapsed().as_millis(),
            stt_ms,
            optimize_ms,
            paste_ms,
        },
    );
    Ok(result)
}

fn cancel_recording_inner(app: &AppHandle, state: &AppState) -> AppResult<()> {
    let session = state
        .recording
        .lock()
        .map_err(|_| AppError::Audio("录音状态锁定失败。".to_string()))?
        .take();
    set_cancel_shortcut(app, state, false);

    if let Some(session) = session {
        std::thread::spawn(move || {
            let _ = session.stop();
        });
    }

    hide_recorder_window(app)?;
    emit_recording(app, "idle", "录音已取消。");
    Ok(())
}

async fn toggle_recording_inner(app: &AppHandle, state: &AppState, mode: DictationMode) -> AppResult<()> {
    if is_recording(state) {
        stop_and_process_inner(app, state).await
    } else {
        start_recording_inner(app, state, mode)
    }
}

/// Esc is global while recording, so a press meant for another app could
/// silently discard minutes of speech. Long takes need a second press.
fn confirm_cancel(app: &AppHandle, state: &AppState) -> bool {
    let elapsed = state
        .recording
        .lock()
        .ok()
        .and_then(|recording| recording.as_ref().map(|session| session.elapsed_seconds()))
        .unwrap_or(0.0);
    if elapsed < CANCEL_CONFIRM_AFTER_SECONDS {
        return true;
    }
    let Ok(mut armed) = state.cancel_armed_at.lock() else {
        return true;
    };
    if armed.is_some_and(|at| at.elapsed() < CANCEL_CONFIRM_WINDOW) {
        *armed = None;
        return true;
    }
    *armed = Some(Instant::now());
    emit_hint(app, "再按一次 Esc 放弃这段录音");
    false
}

// ---------------------------------------------------------------------------
// Shortcuts
// ---------------------------------------------------------------------------

fn handle_global_shortcut(app: &AppHandle, shortcut: &Shortcut, pressed: bool) {
    let state = app.state::<AppState>();
    if state.shortcut_capture_mode.load(Ordering::SeqCst) {
        return;
    }

    if *shortcut == cancel_shortcut() {
        if pressed
            && state.cancel_shortcut_active.load(Ordering::SeqCst)
            && confirm_cancel(app, state.inner())
        {
            let _ = cancel_recording_inner(app, state.inner());
        }
        return;
    }

    let settings = settings::load(app).unwrap_or_default();
    let dictation_mode = match parse_shortcuts(&settings) {
        Ok(ParsedShortcuts {
            translate: Some(translate),
            ..
        }) if translate == *shortcut => DictationMode::Translate,
        _ => DictationMode::Dictate,
    };

    let action = match (settings.record_mode, pressed) {
        (RecordMode::Toggle, true) => {
            if let Ok(mut last_toggle) = state.last_shortcut_toggle.lock() {
                if last_toggle.elapsed() < Duration::from_millis(300) {
                    return;
                }
                *last_toggle = Instant::now();
            }
            ShortcutAction::Toggle
        }
        (RecordMode::Toggle, false) => return,
        (RecordMode::Hold, true) => ShortcutAction::Start,
        (RecordMode::Hold, false) => ShortcutAction::Stop,
    };

    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let state = app.state::<AppState>();
        let result = match action {
            ShortcutAction::Toggle => {
                toggle_recording_inner(&app, state.inner(), dictation_mode).await
            }
            ShortcutAction::Start => {
                if is_recording(state.inner()) {
                    Ok(())
                } else {
                    start_recording_inner(&app, state.inner(), dictation_mode)
                }
            }
            ShortcutAction::Stop => stop_and_process_inner(&app, state.inner()).await,
        };
        report_failure(&app, &result);
    });
}

#[derive(Clone, Copy)]
enum ShortcutAction {
    Toggle,
    Start,
    Stop,
}

fn set_cancel_shortcut(app: &AppHandle, state: &AppState, active: bool) {
    let was_active = state.cancel_shortcut_active.swap(active, Ordering::SeqCst);
    if was_active == active {
        return;
    }
    let shortcuts = app.global_shortcut();
    if active {
        // Another app may own Esc globally; recording still works without it.
        let _ = shortcuts.register(cancel_shortcut());
    } else {
        let _ = shortcuts.unregister(cancel_shortcut());
    }
}

struct ParsedShortcuts {
    dictate: Shortcut,
    translate: Option<Shortcut>,
}

fn parse_shortcuts(settings: &settings::AppSettings) -> AppResult<ParsedShortcuts> {
    let parse = |text: &str| {
        Shortcut::from_str(text.trim())
            .map_err(|error| AppError::Config(format!("无效的快捷键 \"{text}\"：{error}")))
    };
    let translate = settings.translate_shortcut.trim();
    Ok(ParsedShortcuts {
        dictate: parse(&settings.shortcut)?,
        translate: if translate.is_empty() {
            None
        } else {
            Some(parse(translate)?)
        },
    })
}

fn register_shortcut_from_settings(app: &AppHandle, state: &AppState) -> AppResult<()> {
    let settings = settings::load(app)?;
    let parsed = parse_shortcuts(&settings)?;
    let wanted: Vec<Shortcut> = std::iter::once(parsed.dictate).chain(parsed.translate).collect();

    let mut registered = state
        .registered_shortcuts
        .lock()
        .map_err(|_| AppError::Config("快捷键状态锁定失败。".to_string()))?;

    if *registered == wanted {
        return Ok(());
    }

    for previous in registered.drain(..) {
        let _ = app.global_shortcut().unregister(previous);
    }

    app.global_shortcut().register(parsed.dictate).map_err(|error| {
        AppError::Config(format!(
            "快捷键 {} 注册失败，可能已被其他程序占用：{error}",
            settings.shortcut
        ))
    })?;
    registered.push(parsed.dictate);

    if let Some(translate) = parsed.translate {
        app.global_shortcut().register(translate).map_err(|error| {
            AppError::Config(format!(
                "翻译快捷键 {} 注册失败，可能已被其他程序占用：{error}",
                settings.translate_shortcut
            ))
        })?;
        registered.push(translate);
    }
    Ok(())
}

fn apply_autostart(app: &AppHandle, enabled: bool) {
    let autolaunch = app.autolaunch();
    let current = autolaunch.is_enabled().unwrap_or(false);
    if enabled && !current {
        let _ = autolaunch.enable();
    } else if !enabled && current {
        let _ = autolaunch.disable();
    }
}

// ---------------------------------------------------------------------------
// App setup
// ---------------------------------------------------------------------------

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            show_main_window(app);
        }))
        .manage(AppState::default())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            Some(vec![AUTOSTART_FLAG]),
        ))
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, shortcut, event| {
                    handle_global_shortcut(app, shortcut, event.state == ShortcutState::Pressed);
                })
                .build(),
        )
        .setup(|app| {
            setup_tray(app.handle())?;
            let state = app.state::<AppState>();
            let settings = settings::load(app.handle()).unwrap_or_default();
            if let Err(error) = register_shortcut_from_settings(app.handle(), state.inner()) {
                eprintln!("{error}");
            }

            let started_hidden = std::env::args().any(|arg| arg == AUTOSTART_FLAG);
            if !started_hidden || settings.api_key.trim().is_empty() {
                show_main_window(app.handle());
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            // Closing the main window keeps the app alive in the tray.
            if let WindowEvent::CloseRequested { api, .. } = event {
                if window.label() == "main" {
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            load_settings,
            save_settings,
            app_info,
            test_connection,
            list_input_devices,
            get_history,
            delete_history_entry,
            clear_history,
            open_external,
            copy_text,
            set_shortcut_capture_mode,
            start_recording,
            stop_and_process,
            cancel_recording,
            toggle_recording,
            retry_failed,
            discard_failed,
            hide_recorder
        ])
        .run(tauri::generate_context!())
        .expect("error while running GloriousEvolution");
}

fn setup_tray(app: &AppHandle) -> AppResult<()> {
    let show = MenuItem::with_id(app, "show", "打开主界面", true, None::<&str>)?;
    let toggle = MenuItem::with_id(app, "toggle", "开始 / 结束录音", true, None::<&str>)?;
    let retry = MenuItem::with_id(app, "retry", "重试上次失败的录音", true, None::<&str>)?;
    let separator = PredefinedMenuItem::separator(app)?;
    let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &toggle, &retry, &separator, &quit])?;
    let icon = tauri::image::Image::from_bytes(include_bytes!("../icons/icon.png"))?;

    TrayIconBuilder::with_id("glorious-evolution")
        .tooltip("GloriousEvolution 语音输入")
        .icon(icon)
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "show" => show_main_window(app),
            "toggle" => {
                let app = app.clone();
                tauri::async_runtime::spawn(async move {
                    let state = app.state::<AppState>();
                    let result =
                        toggle_recording_inner(&app, state.inner(), DictationMode::Dictate).await;
                    report_failure(&app, &result);
                });
            }
            "retry" => {
                let app = app.clone();
                tauri::async_runtime::spawn(async move {
                    let state = app.state::<AppState>();
                    if let Err(AppError::Config(message)) = retry_failed_inner(&app, state.inner()).await {
                        let _ = show_recorder_window(&app);
                        emit_recording(&app, "error", &message);
                    }
                });
            }
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_main_window(tray.app_handle());
            }
        })
        .build(app)?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Windows & events
// ---------------------------------------------------------------------------

fn show_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}

fn show_recorder_window(app: &AppHandle) -> AppResult<()> {
    let Some(window) = app.get_webview_window("recorder") else {
        return Ok(());
    };

    // Follow the cursor so the pill appears on the monitor the user is working on.
    let monitor = app
        .cursor_position()
        .ok()
        .and_then(|point| app.monitor_from_point(point.x, point.y).ok().flatten())
        .or_else(|| window.current_monitor().ok().flatten())
        .or_else(|| app.primary_monitor().ok().flatten());

    if let Some(monitor) = monitor {
        let area = monitor.work_area();
        let win_size = window.outer_size()?;
        let margin = (24.0 * monitor.scale_factor()) as i32;
        let x = area.position.x + (area.size.width.saturating_sub(win_size.width) / 2) as i32;
        let y = area.position.y + area.size.height.saturating_sub(win_size.height) as i32 - margin;
        window.set_position(PhysicalPosition { x, y })?;
    }
    window.show()?;
    Ok(())
}

fn hide_recorder_window(app: &AppHandle) -> AppResult<()> {
    if let Some(window) = app.get_webview_window("recorder") {
        window.hide()?;
    }
    Ok(())
}

fn emit_recording(app: &AppHandle, status: &'static str, message: &str) {
    let _ = app.emit(
        "recording-status",
        RecordingEvent {
            status,
            message: message.to_string(),
        },
    );
}

fn emit_progress(app: &AppHandle, phase: &'static str, message: &str, progress: f32) {
    let _ = app.emit(
        "processing-progress",
        ProcessingProgressEvent {
            phase,
            message: message.to_string(),
            progress,
        },
    );
}

fn emit_hint(app: &AppHandle, message: &str) {
    let _ = app.emit(
        "recorder-hint",
        RecorderHintEvent {
            message: message.to_string(),
        },
    );
}

fn emit_text_preview(app: &AppHandle, text: &str) {
    let _ = app.emit(
        "text-preview",
        TextPreviewEvent {
            text: text.to_string(),
        },
    );
}
