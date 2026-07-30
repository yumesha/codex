use crate::context_manager::estimate_item_token_count;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::TruncationPolicy;
use codex_utils_output_truncation::truncate_text;

// Retained messages are budgeted with a one-token-per-model-visible-byte upper bound so
// tokenizer-dense observations cannot refill the context immediately after compaction.
pub(super) const RETAINED_MESSAGE_TOKEN_BUDGET: usize = 64_000;
// Keep each retained message independently bounded. Besides limiting the largest context item,
// this lets the aggregate budget preserve several recent turns instead of one oversized turn.
pub(super) const RETAINED_MESSAGE_ITEM_TOKEN_BUDGET: usize = 10_000;

pub(super) fn retained_message_token_upper_bound(item: &ResponseItem) -> usize {
    retained_message_non_text_token_upper_bound(item)
        .saturating_add(retained_message_text_byte_count(item))
}

pub(super) fn truncate_message_to_retention_budget(
    item: ResponseItem,
    max_tokens: usize,
) -> Option<ResponseItem> {
    let non_text_tokens = retained_message_non_text_token_upper_bound(&item);
    if non_text_tokens > max_tokens {
        return None;
    }
    let text_byte_budget = max_tokens.saturating_sub(non_text_tokens);
    let candidate = truncate_message_text_to_byte_budget(item, text_byte_budget)?;
    (retained_message_token_upper_bound(&candidate) <= max_tokens).then_some(candidate)
}

pub(super) fn retained_message_non_text_token_upper_bound(item: &ResponseItem) -> usize {
    if contains_remote_media(item) {
        // The token cost of remote media cannot be known without fetching it. Do not retain the
        // containing message after compaction because it cannot satisfy a hard context bound.
        return usize::MAX;
    }
    let mut textless_item = item.clone();
    if let ResponseItem::Message { content, .. } = &mut textless_item {
        for content_item in content {
            if let ContentItem::InputText { text } | ContentItem::OutputText { text } = content_item
            {
                text.clear();
            }
        }
    }
    // The shared estimator prices modeled bytes at four bytes per token. Multiplying its rounded
    // result back by four and treating each modeled byte as one token keeps wrappers and inline
    // media conservative too, instead of applying the hard bound only to message text.
    usize::try_from(estimate_item_token_count(&textless_item).max(0))
        .unwrap_or(usize::MAX)
        .saturating_mul(4)
}

fn contains_remote_media(item: &ResponseItem) -> bool {
    let ResponseItem::Message { content, .. } = item else {
        return false;
    };
    content.iter().any(|content_item| {
        let media_url = match content_item {
            ContentItem::InputImage { image_url, .. } => Some(image_url),
            ContentItem::InputAudio { audio_url } => Some(audio_url),
            ContentItem::InputText { .. } | ContentItem::OutputText { .. } => None,
        };
        media_url.is_some_and(|url| {
            !url.get(.."data:".len())
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case("data:"))
        })
    })
}

fn retained_message_text_byte_count(item: &ResponseItem) -> usize {
    let ResponseItem::Message { content, .. } = item else {
        return 0;
    };
    content
        .iter()
        .filter_map(|content_item| match content_item {
            ContentItem::InputText { text } | ContentItem::OutputText { text } => Some(text.len()),
            ContentItem::InputImage { .. } | ContentItem::InputAudio { .. } => None,
        })
        .fold(0, usize::saturating_add)
}

fn truncate_message_text_to_byte_budget(
    item: ResponseItem,
    max_bytes: usize,
) -> Option<ResponseItem> {
    let ResponseItem::Message {
        id,
        role,
        content,
        phase,
        internal_chat_message_metadata_passthrough: metadata,
    } = item
    else {
        return Some(item);
    };

    let mut remaining = max_bytes;
    let mut truncated_content = Vec::with_capacity(content.len());
    for mut content_item in content {
        match &mut content_item {
            ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                if remaining == 0 {
                    continue;
                }

                let byte_count = text.len();
                if byte_count <= remaining {
                    remaining = remaining.saturating_sub(byte_count);
                } else {
                    let mut source_byte_budget = remaining;
                    loop {
                        let candidate =
                            truncate_text(text, TruncationPolicy::Bytes(source_byte_budget));
                        if candidate.len() <= remaining {
                            *text = candidate;
                            remaining = remaining.saturating_sub(text.len());
                            break;
                        }
                        let overflow = candidate.len().saturating_sub(remaining).max(1);
                        let next_source_byte_budget = source_byte_budget.saturating_sub(overflow);
                        if next_source_byte_budget == source_byte_budget {
                            text.clear();
                            break;
                        }
                        source_byte_budget = next_source_byte_budget;
                    }
                }
                if !text.is_empty() {
                    truncated_content.push(content_item);
                }
            }
            ContentItem::InputImage { .. } | ContentItem::InputAudio { .. } => {
                truncated_content.push(content_item);
            }
        }
    }

    if truncated_content.is_empty() {
        return None;
    }

    Some(ResponseItem::Message {
        id,
        role,
        content: truncated_content,
        phase,
        internal_chat_message_metadata_passthrough: metadata,
    })
}
