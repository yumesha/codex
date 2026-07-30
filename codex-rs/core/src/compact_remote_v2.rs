use std::sync::Arc;

use crate::Prompt;
use crate::ResponseStream;
use crate::client::ModelClientSession;
use crate::client_common::ResponseEvent;
use crate::compact::CompactedHistoryMetadata;
use crate::compact::CompactionAnalyticsAttempt;
use crate::compact::CompactionAnalyticsDetails;
use crate::compact::InitialContextInjection;
use crate::compact::compaction_status_from_result;
use crate::compact_model_fallback::record_model_fallback;
use crate::compact_model_fallback::should_retry_with_current_model;
use crate::compact_remote::process_compacted_history;
use crate::compact_remote::should_keep_compacted_history_item;
use crate::hook_runtime::PostCompactHookOutcome;
use crate::hook_runtime::PreCompactHookOutcome;
use crate::hook_runtime::run_post_compact_hooks;
use crate::hook_runtime::run_pre_compact_hooks;
use crate::responses_metadata::CodexResponsesMetadata;
use crate::responses_metadata::CompactionTurnMetadata;
use crate::responses_retry::ResponsesStreamRequest;
use crate::responses_retry::handle_retryable_response_stream_error;
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use crate::session::turn_context::TurnContext;
use codex_analytics::CompactionImplementation;
use codex_analytics::CompactionPhase;
use codex_analytics::CompactionReason;
use codex_analytics::CompactionTrigger;
use codex_protocol::error::CodexErr;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::items::ContextCompactionItem;
use codex_protocol::items::TurnItem;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::TurnStartedEvent;
use codex_rollout_trace::CompactionCheckpointTracePayload;
use codex_rollout_trace::InferenceTraceContext;
use futures::StreamExt;
use tokio_util::sync::CancellationToken;

#[path = "compact_remote_v2_attempt.rs"]
mod attempt;
use attempt::RemoteCompactV2Attempt;
use attempt::run_remote_compact_v2_attempt;

#[path = "compact_remote_v2_retention.rs"]
mod retention;
use retention::RETAINED_MESSAGE_ITEM_TOKEN_BUDGET;
use retention::RETAINED_MESSAGE_TOKEN_BUDGET;
use retention::retained_message_token_upper_bound;
use retention::truncate_message_to_retention_budget;

// Compact attempts can run much longer than normal turns, so keep the per-transport
// retry budget smaller than the general Responses stream retry budget.
const MAX_REMOTE_COMPACTION_V2_STREAM_RETRIES: u64 = 2;
const MAX_REMOTE_COMPACTION_V2_AUXILIARY_ITEMS: usize = 128;
const MAX_REMOTE_COMPACTION_V2_AUXILIARY_BYTES: usize = 8 * 1024 * 1024;

pub(crate) async fn run_inline_remote_auto_compact_task(
    sess: Arc<Session>,
    step_context: Arc<StepContext>,
    fallback_step_context: Option<Arc<StepContext>>,
    client_session: &mut ModelClientSession,
    initial_context_injection: InitialContextInjection,
    reason: CompactionReason,
    phase: CompactionPhase,
) -> CodexResult<()> {
    let compaction_metadata = CompactionTurnMetadata::new(
        CompactionTrigger::Auto,
        reason,
        CompactionImplementation::ResponsesCompactionV2,
        phase,
    );
    run_remote_compact_task_inner(
        &sess,
        &step_context,
        fallback_step_context.as_ref(),
        Some(client_session),
        initial_context_injection,
        compaction_metadata,
    )
    .await
}

pub(crate) async fn run_remote_compact_task(
    sess: Arc<Session>,
    turn_context: Arc<TurnContext>,
) -> CodexResult<()> {
    // Standalone compaction is its own request boundary, so it captures a fresh step.
    let step_context = sess
        .capture_step_context(Arc::clone(&turn_context), &CancellationToken::new())
        .await?;
    let start_event = EventMsg::TurnStarted(TurnStartedEvent {
        turn_id: turn_context.sub_id.clone(),
        trace_id: turn_context.trace_id.clone(),
        started_at: turn_context.turn_timing_state.started_at_unix_secs().await,
        model_context_window: turn_context.model_context_window(),
        collaboration_mode_kind: turn_context.mode,
    });
    sess.send_event(&turn_context, start_event).await;

    let compaction_metadata = CompactionTurnMetadata::new(
        CompactionTrigger::Manual,
        CompactionReason::UserRequested,
        CompactionImplementation::ResponsesCompactionV2,
        CompactionPhase::StandaloneTurn,
    );
    run_remote_compact_task_inner(
        &sess,
        &step_context,
        /*fallback_step_context*/ None,
        /*client_session*/ None,
        InitialContextInjection::DoNotInject,
        compaction_metadata,
    )
    .await
}

async fn run_remote_compact_task_inner(
    sess: &Arc<Session>,
    step_context: &Arc<StepContext>,
    fallback_step_context: Option<&Arc<StepContext>>,
    client_session: Option<&mut ModelClientSession>,
    initial_context_injection: InitialContextInjection,
    compaction_metadata: CompactionTurnMetadata,
) -> CodexResult<()> {
    let turn_context = &step_context.turn;
    let trigger = compaction_metadata.trigger();
    let reason = compaction_metadata.reason();
    let implementation = compaction_metadata.implementation();
    let phase = compaction_metadata.phase();
    let mut analytics_details = CompactionAnalyticsDetails {
        active_context_tokens_before: Some(sess.get_total_token_usage().await),
        ..Default::default()
    };
    let attempt = CompactionAnalyticsAttempt::begin(
        sess.as_ref(),
        turn_context.as_ref(),
        trigger,
        reason,
        implementation,
        phase,
    )
    .await;
    let pre_compact_outcome = run_pre_compact_hooks(sess, turn_context, trigger).await;
    match pre_compact_outcome {
        PreCompactHookOutcome::Continue => {}
        PreCompactHookOutcome::Stopped => {
            let error = CodexErr::TurnAborted;
            attempt
                .track(
                    sess.as_ref(),
                    codex_analytics::CompactionStatus::Interrupted,
                    Some(&error),
                    analytics_details,
                )
                .await;
            return Err(error);
        }
    }
    let result = run_remote_compact_task_inner_impl(
        sess,
        step_context,
        fallback_step_context,
        client_session,
        initial_context_injection,
        compaction_metadata,
        &mut analytics_details,
    )
    .await;
    let status = compaction_status_from_result(&result);
    let codex_error = result.as_ref().err();
    if result.is_ok() {
        let post_compact_outcome = run_post_compact_hooks(sess, turn_context, trigger).await;
        if let PostCompactHookOutcome::Stopped = post_compact_outcome {
            attempt
                .track(sess.as_ref(), status, codex_error, analytics_details)
                .await;
            return Err(CodexErr::TurnAborted);
        }
    }
    attempt
        .track(sess.as_ref(), status, codex_error, analytics_details)
        .await;
    match result {
        Ok(()) => Ok(()),
        Err(err) if matches!(err.details(), CodexErrorDetails::TurnAborted) => Err(err),
        Err(err) => {
            sess.track_turn_codex_error(turn_context, &err);
            let event = EventMsg::Error(
                err.to_error_event(Some("Error running remote compact task".to_string())),
            );
            sess.send_event(turn_context, event).await;
            Err(err)
        }
    }
}

async fn run_remote_compact_task_inner_impl(
    sess: &Arc<Session>,
    step_context: &Arc<StepContext>,
    fallback_step_context: Option<&Arc<StepContext>>,
    mut client_session: Option<&mut ModelClientSession>,
    initial_context_injection: InitialContextInjection,
    compaction_metadata: CompactionTurnMetadata,
    analytics_details: &mut CompactionAnalyticsDetails,
) -> CodexResult<()> {
    let turn_context = &step_context.turn;
    let context_compaction_item = ContextCompactionItem::new();
    let compaction_id = context_compaction_item.id.clone();
    let compaction_trace = sess.services.rollout_thread_trace.compaction_trace_context(
        turn_context.sub_id.as_str(),
        compaction_id.as_str(),
        turn_context.model_info.slug.as_str(),
        turn_context.provider.info().name.as_str(),
    );
    let compaction_item = TurnItem::ContextCompaction(context_compaction_item);
    sess.emit_turn_item_started(turn_context, &compaction_item)
        .await;

    let attempt = run_remote_compact_v2_attempt(
        sess,
        step_context,
        client_session.as_deref_mut(),
        &compaction_trace,
        compaction_metadata,
        analytics_details,
    )
    .await;
    let (attempt, compaction_turn_context) = match attempt {
        Ok(attempt) => (attempt, turn_context),
        Err(error) => {
            let Some(fallback_step_context) = fallback_step_context else {
                return Err(error);
            };
            if !should_retry_with_current_model(&error) {
                return Err(error);
            }
            let fallback_turn_context = &fallback_step_context.turn;
            let fallback_compaction_trace =
                sess.services.rollout_thread_trace.compaction_trace_context(
                    fallback_turn_context.sub_id.as_str(),
                    compaction_id.as_str(),
                    fallback_turn_context.model_info.slug.as_str(),
                    fallback_turn_context.provider.info().name.as_str(),
                );
            let fallback_result = run_remote_compact_v2_attempt(
                sess,
                fallback_step_context,
                client_session,
                &fallback_compaction_trace,
                compaction_metadata,
                analytics_details,
            )
            .await;
            record_model_fallback(
                &sess.services.session_telemetry,
                turn_context.model_info.slug.as_str(),
                fallback_turn_context.model_info.slug.as_str(),
                compaction_metadata.reason(),
                compaction_metadata.implementation(),
                fallback_result.as_ref().err(),
            );
            match fallback_result {
                Ok(attempt) => (attempt, fallback_turn_context),
                Err(_) => return Err(error),
            }
        }
    };
    let RemoteCompactV2Attempt {
        trace_input_history,
        prompt_input,
        compaction_output,
        token_usage,
        owned_client_session: _owned_client_session,
    } = attempt;
    if let Some(token_usage) = token_usage {
        sess.record_rollout_budget_usage(&token_usage)?;
        analytics_details.active_context_tokens_before = Some(token_usage.input_tokens);
        analytics_details.compaction_summary_tokens = Some(token_usage.output_tokens);
        analytics_details.cached_input_tokens = Some(token_usage.cached_input_tokens);
        analytics_details.cache_write_input_tokens = Some(token_usage.cache_write_input_tokens);
    }
    let (compacted_history, retained_images) =
        build_v2_compacted_history(&prompt_input, compaction_output);
    analytics_details.retained_image_count = Some(retained_images);
    let (new_window_number, new_window_ids) = sess.advance_auto_compact_window().await;
    let (new_history, world_state_baseline) =
        process_compacted_history(sess.as_ref(), compacted_history, &initial_context_injection)
            .await;

    let reference_context_item = match initial_context_injection {
        InitialContextInjection::DoNotInject => None,
        InitialContextInjection::BeforeLastUserMessage { .. } => {
            Some(compaction_turn_context.to_turn_context_item())
        }
    };
    if let Some(trace_input_history) = trace_input_history.as_deref() {
        compaction_trace.record_installed(&CompactionCheckpointTracePayload {
            input_history: trace_input_history,
            replacement_history: &new_history,
        });
    }
    sess.replace_compacted_history(
        new_history,
        reference_context_item,
        world_state_baseline,
        CompactedHistoryMetadata {
            message: String::new(),
            window_number: new_window_number,
            window_ids: new_window_ids,
        },
    )
    .await;
    sess.recompute_token_usage(compaction_turn_context).await;

    sess.emit_turn_item_completed(compaction_turn_context, compaction_item)
        .await;
    Ok(())
}

struct RemoteCompactionV2Output {
    output_items: Vec<ResponseItem>,
    compaction_index: usize,
    response_id: String,
    token_usage: Option<TokenUsage>,
}

async fn run_remote_compaction_request_v2(
    sess: &Session,
    turn_context: &TurnContext,
    client_session: &mut ModelClientSession,
    prompt: &Prompt,
    responses_metadata: &CodexResponsesMetadata,
) -> CodexResult<RemoteCompactionV2Output> {
    let max_retries = turn_context
        .provider
        .info()
        .stream_max_retries()
        .min(MAX_REMOTE_COMPACTION_V2_STREAM_RETRIES);
    let mut retries = 0;
    loop {
        let result = match client_session
            .stream(
                prompt,
                &turn_context.model_info,
                &turn_context.session_telemetry,
                turn_context.reasoning_effort.clone(),
                turn_context.reasoning_summary,
                turn_context.config.service_tier.clone(),
                responses_metadata,
                &InferenceTraceContext::disabled(),
            )
            .await
        {
            Ok(stream) => collect_compaction_output(stream).await,
            Err(err) => Err(err),
        };

        match result {
            Ok(compaction_output) => return Ok(compaction_output),
            Err(err) if !err.is_retryable() => return Err(err),
            Err(err) => {
                handle_retryable_response_stream_error(
                    &mut retries,
                    max_retries,
                    err,
                    client_session,
                    sess,
                    turn_context,
                    ResponsesStreamRequest::RemoteCompactionV2,
                )
                .await?;
            }
        }
    }
}

async fn collect_compaction_output(
    mut stream: ResponseStream,
) -> CodexResult<RemoteCompactionV2Output> {
    let mut output_item_count = 0usize;
    let mut compaction_count = 0usize;
    let mut auxiliary_item_count = 0usize;
    let mut auxiliary_bytes = 0usize;
    let mut output_items = Vec::new();
    let mut compaction_index = None;
    let mut saw_completed = false;
    let mut completed_response_id = None;
    let mut completed_token_usage = None;
    while let Some(event) = stream.next().await {
        match event? {
            ResponseEvent::OutputItemDone(item) => {
                output_item_count += 1;
                if matches!(&item, ResponseItem::Compaction { .. }) {
                    compaction_count += 1;
                    if compaction_count > 1 {
                        return Err(CodexErr::Fatal(format!(
                            "remote compaction v2 expected exactly one compaction output item, got at least {compaction_count} from {output_item_count} output items"
                        )));
                    }
                    if compaction_index.is_none() {
                        compaction_index = Some(output_items.len());
                    }
                } else {
                    auxiliary_item_count += 1;
                    if auxiliary_item_count > MAX_REMOTE_COMPACTION_V2_AUXILIARY_ITEMS {
                        return Err(CodexErr::Fatal(format!(
                            "remote compaction v2 auxiliary output exceeded the {MAX_REMOTE_COMPACTION_V2_AUXILIARY_ITEMS}-item limit"
                        )));
                    }
                    let serialized_bytes = serde_json::to_vec(&item)
                        .map_err(|err| {
                            CodexErr::Fatal(format!(
                                "failed to serialize remote compaction v2 auxiliary output: {err}"
                            ))
                        })?
                        .len();
                    auxiliary_bytes = auxiliary_bytes.saturating_add(serialized_bytes);
                    if auxiliary_bytes > MAX_REMOTE_COMPACTION_V2_AUXILIARY_BYTES {
                        return Err(CodexErr::Fatal(format!(
                            "remote compaction v2 auxiliary output exceeded the {MAX_REMOTE_COMPACTION_V2_AUXILIARY_BYTES}-byte limit"
                        )));
                    }
                }
                output_items.push(item);
            }
            ResponseEvent::Completed {
                response_id,
                token_usage,
                ..
            } => {
                saw_completed = true;
                completed_response_id = Some(response_id);
                completed_token_usage = token_usage;
                break;
            }
            _ => {}
        }
    }

    if !saw_completed {
        return Err(CodexErr::Stream(
            "remote compaction v2 stream closed before response.completed".to_string(),
        ));
    }

    if compaction_count != 1 {
        return Err(CodexErr::Fatal(format!(
            "remote compaction v2 expected exactly one compaction output item, got {compaction_count} from {output_item_count} output items"
        )));
    }

    let Some(compaction_index) = compaction_index else {
        unreachable!("compaction output must exist when count is exactly one");
    };
    let Some(response_id) = completed_response_id else {
        unreachable!("response id must exist after response.completed");
    };
    Ok(RemoteCompactionV2Output {
        output_items,
        compaction_index,
        response_id,
        token_usage: completed_token_usage,
    })
}

fn build_v2_compacted_history(
    prompt_input: &[ResponseItem],
    compaction_output: ResponseItem,
) -> (Vec<ResponseItem>, usize) {
    let retained = prompt_input
        .iter()
        .filter(|item| is_retained_for_remote_compaction_v2(item))
        .filter(|item| should_keep_compacted_history_item(item))
        .cloned()
        .collect::<Vec<_>>();
    let mut retained =
        truncate_retained_messages_for_remote_compaction(retained, RETAINED_MESSAGE_TOKEN_BUDGET);
    let retained_image_count = retained
        .iter()
        .map(retained_input_image_count)
        .sum::<usize>();
    retained.push(compaction_output);
    (retained, retained_image_count)
}

fn is_retained_for_remote_compaction_v2(item: &ResponseItem) -> bool {
    let ResponseItem::Message { role, .. } = item else {
        return false;
    };

    matches!(role.as_str(), "user" | "developer" | "system")
}

fn retained_input_image_count(item: &ResponseItem) -> usize {
    let ResponseItem::Message { content, .. } = item else {
        return 0;
    };

    content
        .iter()
        .filter(|item| matches!(item, ContentItem::InputImage { .. }))
        .count()
}

fn truncate_retained_messages_for_remote_compaction(
    items: Vec<ResponseItem>,
    max_tokens: usize,
) -> Vec<ResponseItem> {
    let mut remaining = max_tokens;
    let mut truncated_reversed = Vec::with_capacity(items.len());
    for item in items.into_iter().rev() {
        if remaining == 0 {
            continue;
        }

        let item_budget = remaining.min(RETAINED_MESSAGE_ITEM_TOKEN_BUDGET);
        let token_count = retained_message_token_upper_bound(&item).max(1);
        if token_count <= item_budget {
            truncated_reversed.push(item);
            remaining = remaining.saturating_sub(token_count);
        } else if let Some(truncated_item) =
            truncate_message_to_retention_budget(item, /*max_tokens*/ item_budget)
        {
            let truncated_token_count = retained_message_token_upper_bound(&truncated_item).max(1);
            truncated_reversed.push(truncated_item);
            remaining = remaining.saturating_sub(truncated_token_count);
        }
    }
    truncated_reversed.reverse();
    truncated_reversed
}

#[cfg(test)]
mod tests {
    use super::retention::retained_message_non_text_token_upper_bound;
    use super::*;
    use crate::context_manager::estimate_item_token_count;
    use codex_protocol::models::ContentItem;
    use codex_protocol::models::MessagePhase;
    use codex_protocol::models::ReasoningItemReasoningSummary;
    use pretty_assertions::assert_eq;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    fn message(role: &str, text: &str, phase: Option<MessagePhase>) -> ResponseItem {
        ResponseItem::Message {
            id: None,
            role: role.to_string(),
            content: vec![ContentItem::InputText {
                text: text.to_string(),
            }],
            phase,
            internal_chat_message_metadata_passthrough: None,
        }
    }

    fn response_stream(events: Vec<CodexResult<ResponseEvent>>) -> ResponseStream {
        let (tx_event, rx_event) = mpsc::channel(events.len().max(1));
        for event in events {
            tx_event
                .try_send(event)
                .expect("response stream test channel should have capacity");
        }
        drop(tx_event);
        ResponseStream {
            rx_event,
            consumer_dropped: CancellationToken::new(),
        }
    }

    #[test]
    fn build_v2_compacted_history_filters_to_installed_retention_shape() {
        let input = vec![
            message("developer", "dev", /*phase*/ None),
            message("system", "sys", /*phase*/ None),
            message("user", "user", /*phase*/ None),
            message("assistant", "commentary", Some(MessagePhase::Commentary)),
            message("assistant", "final", Some(MessagePhase::FinalAnswer)),
            ResponseItem::FunctionCall {
                id: None,
                name: "shell_command".to_string(),
                namespace: None,
                arguments: "{}".to_string(),
                call_id: "call_1".to_string(),
                encrypted_function_args: None,
                internal_chat_message_metadata_passthrough: None,
            },
            ResponseItem::Compaction {
                id: None,
                encrypted_content: "old".to_string(),
                internal_chat_message_metadata_passthrough: None,
            },
        ];
        let output = ResponseItem::Compaction {
            id: None,
            encrypted_content: "new".to_string(),
            internal_chat_message_metadata_passthrough: None,
        };

        let (history, _) = build_v2_compacted_history(&input, output.clone());

        assert_eq!(
            history,
            vec![message("user", "user", /*phase*/ None), output]
        );
    }

    #[test]
    fn build_v2_compacted_history_discards_messages_before_truncating() {
        let old = message("user", "old", /*phase*/ None);
        let new = message("user", "new", /*phase*/ None);
        let huge_developer_message = "d".repeat((RETAINED_MESSAGE_TOKEN_BUDGET + 1) * 4);
        let huge_contextual_message = format!(
            "<environment_context>\n{}\n</environment_context>",
            "c".repeat((RETAINED_MESSAGE_TOKEN_BUDGET + 1) * 4)
        );
        let input = vec![
            old.clone(),
            message("developer", &huge_developer_message, /*phase*/ None),
            message("user", &huge_contextual_message, /*phase*/ None),
            new.clone(),
        ];
        let output = ResponseItem::Compaction {
            id: None,
            encrypted_content: "new".to_string(),
            internal_chat_message_metadata_passthrough: None,
        };

        let (history, _) = build_v2_compacted_history(&input, output.clone());

        assert_eq!(history, vec![old, new, output]);
    }

    #[test]
    fn build_v2_compacted_history_counts_retained_input_images() {
        let input = ["abc", "def"]
            .into_iter()
            .map(|payload| ResponseItem::Message {
                id: None,
                role: "user".to_string(),
                content: vec![
                    ContentItem::InputText {
                        text: "user".to_string(),
                    },
                    ContentItem::InputImage {
                        image_url: format!("data:image/png;base64,{payload}"),
                        detail: None,
                    },
                ],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            })
            .collect::<Vec<_>>();
        let output = ResponseItem::Compaction {
            id: None,
            encrypted_content: "new".to_string(),
            internal_chat_message_metadata_passthrough: None,
        };

        let (_, retained_image_count) = build_v2_compacted_history(&input, output);

        assert_eq!(retained_image_count, 2);
    }

    #[test]
    fn retained_history_truncation_keeps_newest_messages_first() {
        let middle = message(
            "user",
            &format!("middle{}1234", "x".repeat(1_000)),
            /*phase*/ None,
        );
        let new = message("user", "new", /*phase*/ None);
        let retained = vec![
            message("user", &"old".repeat(1_000), /*phase*/ None),
            middle,
            new.clone(),
        ];
        let max_tokens = retained_message_token_upper_bound(&new).saturating_add(256);

        let truncated = truncate_retained_messages_for_remote_compaction(retained, max_tokens);

        assert_eq!(truncated.len(), 2);
        assert_eq!(truncated.last(), Some(&new));
        let ResponseItem::Message { content, .. } = &truncated[0] else {
            panic!("retained history should contain a message");
        };
        let [ContentItem::InputText { text }] = content.as_slice() else {
            panic!("truncated message should contain one text part");
        };
        assert!(text.starts_with("middle"));
        assert!(text.ends_with("1234"));
        assert!(
            truncated
                .iter()
                .map(retained_message_token_upper_bound)
                .sum::<usize>()
                <= max_tokens
        );
    }

    #[test]
    fn retained_history_truncation_preserves_images_and_bounds_text_parts() {
        let item = ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![
                ContentItem::InputText {
                    text: "abcdef".repeat(100),
                },
                ContentItem::InputImage {
                    image_url: "data:image/png;base64,abc".to_string(),
                    detail: None,
                },
                ContentItem::OutputText {
                    text: "uvwxyz".repeat(100),
                },
            ],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        };
        let max_tokens = retained_message_non_text_token_upper_bound(&item).saturating_add(256);

        let truncated = truncate_retained_messages_for_remote_compaction(vec![item], max_tokens);

        assert_eq!(truncated.len(), 1);
        assert_eq!(retained_input_image_count(&truncated[0]), 1);
        assert!(retained_message_token_upper_bound(&truncated[0]) <= max_tokens);
        let ResponseItem::Message { content, .. } = &truncated[0] else {
            panic!("retained history should contain a message");
        };
        assert!(content.iter().any(|item| matches!(
            item,
            ContentItem::InputText { text }
                if text.starts_with("abcdef") && text.ends_with("abcdef")
        )));
        assert!(
            content
                .iter()
                .all(|item| !matches!(item, ContentItem::OutputText { .. }))
        );
    }

    #[test]
    fn retained_history_truncation_charges_image_only_messages() {
        let image_only_message = ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputImage {
                image_url: "data:image/png;base64,abc".to_string(),
                detail: None,
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        };
        let newest = message("user", "new", /*phase*/ None);
        let retained = vec![
            message("user", "old", /*phase*/ None),
            image_only_message.clone(),
            newest.clone(),
        ];
        let max_tokens = retained_message_token_upper_bound(&image_only_message)
            .saturating_add(retained_message_token_upper_bound(&newest));

        assert_eq!(
            retained_message_token_upper_bound(&image_only_message),
            usize::try_from(estimate_item_token_count(&image_only_message).max(0))
                .unwrap_or(usize::MAX)
                .saturating_mul(4),
            "all modeled non-text bytes must receive the conservative retained-item multiplier",
        );

        let truncated = truncate_retained_messages_for_remote_compaction(retained, max_tokens);

        assert_eq!(truncated, vec![image_only_message, newest]);
    }

    #[test]
    fn retained_history_truncation_drops_image_only_messages_after_budget_is_spent() {
        let image_only_message = ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputImage {
                image_url: "data:image/png;base64,abc".to_string(),
                detail: None,
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        };
        let newest = message("user", "new", /*phase*/ None);
        let retained = vec![image_only_message, newest.clone()];
        let max_tokens = retained_message_token_upper_bound(&newest);

        let truncated = truncate_retained_messages_for_remote_compaction(retained, max_tokens);

        assert_eq!(truncated, vec![newest]);
    }

    #[test]
    fn retained_history_truncation_drops_remote_media_with_unknown_cost() {
        let remote_image = ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputImage {
                image_url: "https://example.com/large-image.png".to_string(),
                detail: None,
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        };
        let remote_audio = ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputAudio {
                audio_url: "https://example.com/long-audio.wav".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        };
        let newest = message("user", "new", /*phase*/ None);

        let truncated = truncate_retained_messages_for_remote_compaction(
            vec![remote_image, remote_audio, newest.clone()],
            RETAINED_MESSAGE_TOKEN_BUDGET,
        );

        assert_eq!(truncated, vec![newest]);
    }

    #[test]
    fn retained_history_truncation_handles_text_part_boundaries() {
        let item = ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![
                ContentItem::InputText {
                    text: "123456789012345".to_string(),
                },
                ContentItem::OutputText {
                    text: "x".repeat(1_000),
                },
            ],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        };
        let max_tokens = retained_message_non_text_token_upper_bound(&item).saturating_add(20);

        let truncated = truncate_retained_messages_for_remote_compaction(vec![item], max_tokens);

        assert_eq!(
            truncated,
            vec![message("user", "123456789012345", /*phase*/ None)]
        );
    }

    #[test]
    fn retained_history_truncation_caps_each_message_and_keeps_escaped_newest_text() {
        let escaped_newest = message(
            "user",
            &format!("ESCAPED_START{}ESCAPED_END", "\0\\\"\n".repeat(10_000)),
            /*phase*/ None,
        );
        let retained = vec![
            message(
                "user",
                &"ordinary text ".repeat(10_000),
                /*phase*/ None,
            ),
            escaped_newest,
        ];

        let truncated = truncate_retained_messages_for_remote_compaction(
            retained,
            RETAINED_MESSAGE_TOKEN_BUDGET,
        );

        assert_eq!(truncated.len(), 2);
        assert!(truncated.iter().all(|item| {
            retained_message_token_upper_bound(item) <= RETAINED_MESSAGE_ITEM_TOKEN_BUDGET
        }));
        assert!(
            truncated
                .iter()
                .map(retained_message_token_upper_bound)
                .sum::<usize>()
                <= RETAINED_MESSAGE_TOKEN_BUDGET
        );
        let ResponseItem::Message { content, .. } = &truncated[1] else {
            panic!("newest retained item should remain a message");
        };
        let [ContentItem::InputText { text }] = content.as_slice() else {
            panic!("newest retained message should contain text");
        };
        assert!(text.starts_with("ESCAPED_START"));
        assert!(text.ends_with("ESCAPED_END"));
    }

    #[tokio::test]
    async fn collect_compaction_output_accepts_additional_output_items() {
        let reasoning = ResponseItem::Reasoning {
            id: None,
            summary: vec![ReasoningItemReasoningSummary::SummaryText {
                text: "LOCAL_ONLY_COMPACTION_REASONING".to_string(),
            }],
            content: None,
            encrypted_content: Some("encrypted-reasoning".to_string()),
            internal_chat_message_metadata_passthrough: None,
        };
        let ignored_message = message(
            "assistant",
            "IGNORED_COMPACT_REPLY",
            Some(MessagePhase::FinalAnswer),
        );
        let compaction = ResponseItem::Compaction {
            id: None,
            encrypted_content: "encrypted".to_string(),
            internal_chat_message_metadata_passthrough: None,
        };
        let stream = response_stream(vec![
            Ok(ResponseEvent::OutputItemDone(reasoning.clone())),
            Ok(ResponseEvent::OutputItemDone(ignored_message.clone())),
            Ok(ResponseEvent::OutputItemDone(compaction.clone())),
            Ok(ResponseEvent::Completed {
                response_id: "resp-compact".to_string(),
                token_usage: Some(TokenUsage {
                    input_tokens: 123_456,
                    cached_input_tokens: 7_890,
                    cache_write_input_tokens: 0,
                    output_tokens: 42,
                    reasoning_output_tokens: 5,
                    total_tokens: 123_498,
                }),
                end_turn: Some(true),
            }),
        ]);

        let output = collect_compaction_output(stream)
            .await
            .expect("compaction should be collected");

        assert_eq!(
            output.output_items,
            vec![reasoning, ignored_message, compaction]
        );
        assert_eq!(output.compaction_index, 2);
        assert_eq!(output.response_id, "resp-compact");
        assert_eq!(
            output.token_usage,
            Some(TokenUsage {
                input_tokens: 123_456,
                cached_input_tokens: 7_890,
                cache_write_input_tokens: 0,
                output_tokens: 42,
                reasoning_output_tokens: 5,
                total_tokens: 123_498,
            })
        );
    }

    #[tokio::test]
    async fn collect_compaction_output_rejects_too_many_auxiliary_items() {
        let mut events = (0..=MAX_REMOTE_COMPACTION_V2_AUXILIARY_ITEMS)
            .map(|index| {
                Ok(ResponseEvent::OutputItemDone(message(
                    "assistant",
                    &format!("auxiliary-{index}"),
                    Some(MessagePhase::FinalAnswer),
                )))
            })
            .collect::<Vec<_>>();
        events.push(Ok(ResponseEvent::OutputItemDone(
            ResponseItem::Compaction {
                id: None,
                encrypted_content: "encrypted".to_string(),
                internal_chat_message_metadata_passthrough: None,
            },
        )));
        events.push(Ok(ResponseEvent::Completed {
            response_id: "resp-compact".to_string(),
            token_usage: None,
            end_turn: Some(true),
        }));

        let error = collect_compaction_output(response_stream(events))
            .await
            .err()
            .expect("excess auxiliary items should fail compaction collection");

        assert!(matches!(
            error.details(),
            CodexErrorDetails::Fatal(message)
                if message.contains(&format!(
                    "{MAX_REMOTE_COMPACTION_V2_AUXILIARY_ITEMS}-item limit"
                ))
        ));
    }

    #[tokio::test]
    async fn collect_compaction_output_rejects_oversized_auxiliary_output() {
        let oversized_message = ResponseItem::Message {
            id: None,
            role: "assistant".to_string(),
            content: vec![ContentItem::OutputText {
                text: "x".repeat(MAX_REMOTE_COMPACTION_V2_AUXILIARY_BYTES + 1),
            }],
            phase: Some(MessagePhase::FinalAnswer),
            internal_chat_message_metadata_passthrough: None,
        };
        let stream = response_stream(vec![Ok(ResponseEvent::OutputItemDone(oversized_message))]);

        let error = collect_compaction_output(stream)
            .await
            .err()
            .expect("oversized auxiliary output should fail compaction collection");

        assert!(matches!(
            error.details(),
            CodexErrorDetails::Fatal(message)
                if message.contains(&format!(
                    "{MAX_REMOTE_COMPACTION_V2_AUXILIARY_BYTES}-byte limit"
                ))
        ));
    }

    #[tokio::test]
    async fn collect_compaction_output_exempts_large_compaction_item_from_auxiliary_byte_limit() {
        let oversized_compaction_bytes = MAX_REMOTE_COMPACTION_V2_AUXILIARY_BYTES + 1;
        let stream = response_stream(vec![
            Ok(ResponseEvent::OutputItemDone(ResponseItem::Compaction {
                id: None,
                encrypted_content: "x".repeat(oversized_compaction_bytes),
                internal_chat_message_metadata_passthrough: None,
            })),
            Ok(ResponseEvent::Completed {
                response_id: "resp-large-compaction".to_string(),
                token_usage: None,
                end_turn: Some(true),
            }),
        ]);

        let output = collect_compaction_output(stream)
            .await
            .expect("large compaction item should not count against the auxiliary byte limit");

        let [
            ResponseItem::Compaction {
                encrypted_content, ..
            },
        ] = output.output_items.as_slice()
        else {
            panic!("expected the large compaction item to be retained");
        };
        assert_eq!(encrypted_content.len(), oversized_compaction_bytes);
        assert_eq!(output.compaction_index, 0);
        assert_eq!(output.response_id, "resp-large-compaction");
        assert_eq!(output.token_usage, None);
    }
}
