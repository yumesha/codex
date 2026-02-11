use async_trait::async_trait;
use codex_api::AuthProvider as _;
use codex_protocol::models::FunctionCallOutputBody;
use reqwest::header::AUTHORIZATION;
use reqwest::header::HeaderName;
use reqwest::header::HeaderValue;
use serde::Deserialize;
use std::path::Path;
use tokio::fs;

use crate::api_bridge::auth_provider_from_auth;
use crate::default_client::build_reqwest_client;
use crate::function_tool::FunctionCallError;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::handlers::parse_arguments;
use crate::tools::registry::ToolHandler;
use crate::tools::registry::ToolKind;

pub struct TranscribeMediaHandler;

const DEFAULT_TRANSCRIPTION_MODEL: &str = "gpt-4o-mini-transcribe";
const MAX_MEDIA_BYTES: u64 = 100 * 1024 * 1024;

#[derive(Deserialize)]
struct TranscribeMediaArgs {
    path: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    language: Option<String>,
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    temperature: Option<f32>,
}

#[derive(Deserialize)]
struct TranscriptionResponse {
    text: String,
}

#[async_trait]
impl ToolHandler for TranscribeMediaHandler {
    fn kind(&self) -> ToolKind {
        ToolKind::Function
    }

    async fn handle(&self, invocation: ToolInvocation) -> Result<ToolOutput, FunctionCallError> {
        let ToolInvocation { turn, payload, .. } = invocation;

        let arguments = match payload {
            ToolPayload::Function { arguments } => arguments,
            _ => {
                return Err(FunctionCallError::RespondToModel(
                    "transcribe_media handler received unsupported payload".to_string(),
                ));
            }
        };

        let args: TranscribeMediaArgs = parse_arguments(&arguments)?;
        if args.path.trim().is_empty() {
            return Err(FunctionCallError::RespondToModel(
                "path cannot be empty".to_string(),
            ));
        }

        let resolved_path = turn.resolve_path(Some(args.path.clone()));
        let metadata = fs::metadata(&resolved_path).await.map_err(|error| {
            FunctionCallError::RespondToModel(format!(
                "unable to locate media file at `{}`: {error}",
                resolved_path.display()
            ))
        })?;
        if !metadata.is_file() {
            return Err(FunctionCallError::RespondToModel(format!(
                "media path `{}` is not a file",
                resolved_path.display()
            )));
        }
        if metadata.len() == 0 {
            return Err(FunctionCallError::RespondToModel(format!(
                "media file `{}` is empty",
                resolved_path.display()
            )));
        }
        if metadata.len() > MAX_MEDIA_BYTES {
            return Err(FunctionCallError::RespondToModel(format!(
                "media file `{}` is too large ({} bytes > {} bytes)",
                resolved_path.display(),
                metadata.len(),
                MAX_MEDIA_BYTES
            )));
        }

        let media_bytes = fs::read(&resolved_path).await.map_err(|error| {
            FunctionCallError::RespondToModel(format!(
                "failed to read media file `{}`: {error}",
                resolved_path.display()
            ))
        })?;

        let auth = match &turn.auth_manager {
            Some(manager) => manager.auth().await,
            None => None,
        };
        let auth_mode = auth.as_ref().map(crate::auth::CodexAuth::auth_mode);
        let api_provider = turn
            .provider
            .to_api_provider(auth_mode)
            .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?;
        let auth_provider = auth_provider_from_auth(auth, &turn.provider)
            .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?;
        let token = auth_provider.bearer_token().ok_or_else(|| {
            FunctionCallError::RespondToModel(
                "transcribe_media requires an authenticated provider token".to_string(),
            )
        })?;

        let file_name = file_name_for_upload(&resolved_path);
        let mut form = reqwest::multipart::Form::new()
            .part(
                "file",
                reqwest::multipart::Part::bytes(media_bytes).file_name(file_name),
            )
            .text(
                "model",
                args.model
                    .unwrap_or_else(|| DEFAULT_TRANSCRIPTION_MODEL.to_owned()),
            )
            .text("response_format", "json");
        if let Some(language) = args.language {
            form = form.text("language", language);
        }
        if let Some(prompt) = args.prompt {
            form = form.text("prompt", prompt);
        }
        if let Some(temperature) = args.temperature {
            form = form.text("temperature", temperature.to_string());
        }

        let client = build_reqwest_client();
        let mut request = client
            .post(api_provider.url_for_path("audio/transcriptions"))
            .header(AUTHORIZATION, format!("Bearer {token}"))
            .multipart(form);
        if let Some(account_id) = auth_provider.account_id()
            && let Ok(value) = HeaderValue::from_str(&account_id)
        {
            request = request.header(HeaderName::from_static("chatgpt-account-id"), value);
        }
        for (name, value) in &api_provider.headers {
            request = request.header(name, value);
        }

        let response = request.send().await.map_err(|error| {
            FunctionCallError::RespondToModel(format!("failed to request transcription: {error}"))
        })?;
        let status = response.status();
        let body = response.text().await.map_err(|error| {
            FunctionCallError::RespondToModel(format!(
                "failed to read transcription response: {error}"
            ))
        })?;
        if !status.is_success() {
            return Err(FunctionCallError::RespondToModel(format!(
                "transcription request failed ({status}): {}",
                summarize_error_body(&body)
            )));
        }

        let transcript = match serde_json::from_str::<TranscriptionResponse>(&body) {
            Ok(parsed) => parsed.text,
            Err(_) => body.trim().to_string(),
        };
        if transcript.is_empty() {
            return Err(FunctionCallError::RespondToModel(
                "transcription response did not contain text".to_string(),
            ));
        }

        Ok(ToolOutput::Function {
            body: FunctionCallOutputBody::Text(transcript),
            success: Some(true),
        })
    }
}

fn file_name_for_upload(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| "media".to_string())
}

fn summarize_error_body(body: &str) -> String {
    const MAX_ERROR_CHARS: usize = 400;
    let trimmed = body.trim();
    if trimmed.len() <= MAX_ERROR_CHARS {
        return trimmed.to_string();
    }
    let mut summary = trimmed
        .chars()
        .take(MAX_ERROR_CHARS.saturating_sub("...".len()))
        .collect::<String>();
    summary.push_str("...");
    summary
}
