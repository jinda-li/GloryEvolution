use std::sync::Arc;

use reqwest::multipart;
use serde::Deserialize;
use tokio::sync::Semaphore;

use crate::{
    api, audio,
    i18n::{self, tr, trf},
    settings::AppSettings,
    AppError, AppResult,
};

/// Segments of one long take are transcribed side by side, but not so many
/// at once that a slow uplink times every upload out.
const PARALLEL_SEGMENTS: usize = 4;

#[derive(Debug, Deserialize)]
struct TranscriptionResponse {
    #[serde(default)]
    text: String,
}

/// Transcribes a whole take. Long takes are split at pauses and each piece is
/// sent separately: one long upload used to hit the request timeout, and
/// transcription models cap their output (~2k tokens for gpt-4o-transcribe),
/// which silently cut long dictations off halfway.
pub async fn transcribe(
    samples: &[i16],
    settings: &AppSettings,
    on_segment_done: impl Fn(usize, usize),
) -> AppResult<String> {
    let segments = audio::split_segments(samples);
    let total = segments.len();
    if total == 1 {
        let text = transcribe_wav(audio::encode_wav(segments[0])?, settings).await?;
        on_segment_done(1, 1);
        return Ok(text);
    }

    let settings = Arc::new(settings.clone());
    let permits = Arc::new(Semaphore::new(PARALLEL_SEGMENTS));
    let mut handles = Vec::with_capacity(total);
    for segment in segments {
        let wav = audio::encode_wav(segment)?;
        let settings = settings.clone();
        let permits = permits.clone();
        handles.push(tauri::async_runtime::spawn(async move {
            let _permit = permits.acquire_owned().await;
            transcribe_wav(wav, &settings).await
        }));
    }

    let mut parts = Vec::with_capacity(total);
    for (index, handle) in handles.into_iter().enumerate() {
        let text = handle
            .await
            .map_err(|error| AppError::Api(trf(&i18n::STT_TASK_FAILED, &[&error])))??;
        on_segment_done(index + 1, total);
        parts.push(text);
    }
    Ok(join_segments(&parts))
}

async fn transcribe_wav(wav: Vec<u8>, settings: &AppSettings) -> AppResult<String> {
    let base_url = settings.base_url();
    let model = settings.stt_model.trim().to_string();
    let language = settings.language.trim().to_string();

    let response = api::send_with_retry(|| {
        let mut form = multipart::Form::new()
            .part(
                "file",
                multipart::Part::bytes(wav.clone())
                    .file_name("recording.wav")
                    .mime_str("audio/wav")
                    .expect("static mime type is valid"),
            )
            .text("model", model.clone())
            .text("temperature", "0");
        if !language.is_empty() && language != "auto" {
            form = form.text("language", language.clone());
        }

        api::authorized(
            api::client().post(format!("{base_url}/audio/transcriptions")),
            settings,
        )
        .multipart(form)
    })
    .await?;

    let payload: TranscriptionResponse = api::parse_json(response, tr(&i18n::STAGE_STT)).await?;
    Ok(payload.text.trim().to_string())
}

/// Chinese and Japanese run on without spaces; everything else (Korean
/// included) gets one.
fn join_segments(parts: &[String]) -> String {
    let mut joined = String::new();
    for part in parts.iter().map(|part| part.trim()).filter(|part| !part.is_empty()) {
        let needs_space = match (joined.chars().last(), part.chars().next()) {
            (Some(last), Some(first)) => !(is_cjk_like(last) || is_cjk_like(first)),
            _ => false,
        };
        if needs_space {
            joined.push(' ');
        }
        joined.push_str(part);
    }
    joined
}

fn is_cjk_like(ch: char) -> bool {
    matches!(ch as u32,
        0x3000..=0x30FF | 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF | 0xFF00..=0xFFEF)
}

#[cfg(test)]
mod tests {
    use super::join_segments;

    #[test]
    fn joins_segments_by_script() {
        let parts = |items: &[&str]| items.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(join_segments(&parts(&["今天开会，", "讨论预算。"])), "今天开会，讨论预算。");
        assert_eq!(join_segments(&parts(&["First part.", "Second part."])), "First part. Second part.");
        assert_eq!(join_segments(&parts(&["你好", "", " world "])), "你好world");
    }
}
