//! In-session `/model <provider>/<model>` command parsing.
//!
//! Claude Code sends the user's turn in the JSON request body under `messages`.
//! If a user message contains a `/model provider/model` command, we reroute the
//! request to that provider+model and strip the command text so the upstream LLM
//! never sees it -- mirroring the original Python behavior, but additionally
//! handling the array-of-content-blocks message shape that Claude Code actually
//! emits (the original only matched plain string content).

use serde_json::Value;

/// A parsed routing command: `(provider, model)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelCommand {
    pub provider: String,
    pub model: String,
}

/// Scan `body["messages"]` for the first user message carrying a
/// `/model <provider>/<model>` command.
///
/// On a match, mutates `body` in place: strips the command token from the
/// message text, removing the whole message if nothing else remains. Returns the
/// parsed command, or `None` if no valid command is present.
///
/// A command is only recognized when the identifier contains a `/`
/// (i.e. `provider/model`); a bare `/model foo` is ignored, matching the
/// original.
pub fn parse_and_strip(body: &mut Value) -> Option<ModelCommand> {
    let messages = body.get_mut("messages")?.as_array_mut()?;

    let mut matched: Option<(usize, ModelCommand, Option<StripTarget>)> = None;

    for (i, msg) in messages.iter().enumerate() {
        if msg.get("role").and_then(Value::as_str) != Some("user") {
            continue;
        }
        let Some(content) = msg.get("content") else {
            continue;
        };

        // Case 1: content is a plain string.
        if let Some(text) = content.as_str() {
            if let Some((cmd, remainder)) = extract_command(text) {
                let target = match remainder {
                    Some(rem) => StripTarget::String(rem),
                    None => StripTarget::RemoveMessage,
                };
                matched = Some((i, cmd, Some(target)));
                break;
            }
        }
        // Case 2: content is an array of blocks; check text blocks.
        else if let Some(blocks) = content.as_array() {
            for (bi, block) in blocks.iter().enumerate() {
                let is_text = block.get("type").and_then(Value::as_str) == Some("text");
                let Some(text) = block.get("text").and_then(Value::as_str) else {
                    continue;
                };
                if !is_text {
                    continue;
                }
                if let Some((cmd, remainder)) = extract_command(text) {
                    let target = StripTarget::Block {
                        block_index: bi,
                        remainder,
                    };
                    matched = Some((i, cmd, Some(target)));
                    break;
                }
            }
            if matched.is_some() {
                break;
            }
        }
    }

    let (msg_index, cmd, target) = matched?;
    apply_strip(messages, msg_index, target.expect("target set on match"));
    Some(cmd)
}

/// What to do with the matched message once the command is extracted.
enum StripTarget {
    /// Replace string content with this remainder text.
    String(String),
    /// Remove the entire message (string content was only the command).
    RemoveMessage,
    /// Update a text block within an array; remove the message if the array
    /// becomes empty after dropping an emptied block.
    Block {
        block_index: usize,
        remainder: Option<String>,
    },
}

fn apply_strip(messages: &mut Vec<Value>, msg_index: usize, target: StripTarget) {
    match target {
        StripTarget::String(remainder) => {
            messages[msg_index]["content"] = Value::String(remainder);
        }
        StripTarget::RemoveMessage => {
            messages.remove(msg_index);
        }
        StripTarget::Block {
            block_index,
            remainder,
        } => {
            let blocks = messages[msg_index]["content"]
                .as_array_mut()
                .expect("content is an array on a block match");
            match remainder {
                Some(rem) => {
                    blocks[block_index]["text"] = Value::String(rem);
                }
                None => {
                    blocks.remove(block_index);
                }
            }
            if messages[msg_index]["content"]
                .as_array()
                .map(|b| b.is_empty())
                .unwrap_or(false)
            {
                messages.remove(msg_index);
            }
        }
    }
}

/// If `text` begins with a `/model <provider>/<model>` command, return the
/// parsed command plus the remaining text (`None` when nothing else remains).
fn extract_command(text: &str) -> Option<(ModelCommand, Option<String>)> {
    // The command must lead the message (ignoring leading whitespace). The
    // identifier is the first token after `/model `; any following text is kept.
    let rest = text.trim_start().strip_prefix("/model ")?;
    let identifier = rest.split_whitespace().next()?;
    let (provider, model) = identifier.split_once('/')?;
    if provider.is_empty() || model.is_empty() {
        return None;
    }

    // Strip the exact `/model <identifier>` token from the original (untrimmed)
    // text, matching the original's `content.replace(...)` then `.strip()`.
    let token = format!("/model {identifier}");
    let cleaned = text.replace(&token, "");
    let cleaned = cleaned.trim().to_string();
    let remainder = if cleaned.is_empty() {
        None
    } else {
        Some(cleaned)
    };

    Some((
        ModelCommand {
            provider: provider.to_string(),
            model: model.to_string(),
        },
        remainder,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_string_command_and_removes_command_only_message() {
        let mut body = json!({
            "messages": [
                {"role": "user", "content": "/model deepseek/deepseek-chat"}
            ]
        });
        let cmd = parse_and_strip(&mut body).expect("command");
        assert_eq!(cmd.provider, "deepseek");
        assert_eq!(cmd.model, "deepseek-chat");
        // The message was only the command, so it is removed entirely.
        assert_eq!(body["messages"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn parses_string_command_and_keeps_remaining_text() {
        let mut body = json!({
            "messages": [
                {"role": "user", "content": "/model kimi/moonshot-v1-8k hello there"}
            ]
        });
        let cmd = parse_and_strip(&mut body).expect("command");
        assert_eq!(cmd.provider, "kimi");
        assert_eq!(cmd.model, "moonshot-v1-8k");
        assert_eq!(body["messages"][0]["content"], "hello there");
    }

    #[test]
    fn parses_command_in_block_array() {
        let mut body = json!({
            "messages": [
                {"role": "user", "content": [
                    {"type": "text", "text": "/model zai/glm-4"}
                ]}
            ]
        });
        let cmd = parse_and_strip(&mut body).expect("command");
        assert_eq!(cmd.provider, "zai");
        assert_eq!(cmd.model, "glm-4");
        // Block emptied -> block removed -> message removed.
        assert_eq!(body["messages"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn keeps_block_text_remainder() {
        let mut body = json!({
            "messages": [
                {"role": "user", "content": [
                    {"type": "text", "text": "/model zai/glm-4 summarize this"}
                ]}
            ]
        });
        let cmd = parse_and_strip(&mut body).expect("command");
        assert_eq!(cmd.model, "glm-4");
        assert_eq!(body["messages"][0]["content"][0]["text"], "summarize this");
    }

    #[test]
    fn ignores_identifier_without_slash() {
        let mut body = json!({
            "messages": [
                {"role": "user", "content": "/model deepseek"}
            ]
        });
        assert!(parse_and_strip(&mut body).is_none());
        // Message left untouched.
        assert_eq!(body["messages"][0]["content"], "/model deepseek");
    }

    #[test]
    fn no_command_leaves_messages_untouched() {
        let mut body = json!({
            "messages": [
                {"role": "user", "content": "just a normal question"},
                {"role": "assistant", "content": "an answer"}
            ]
        });
        assert!(parse_and_strip(&mut body).is_none());
        assert_eq!(body["messages"].as_array().unwrap().len(), 2);
        assert_eq!(body["messages"][0]["content"], "just a normal question");
    }

    #[test]
    fn only_acts_on_user_messages() {
        let mut body = json!({
            "messages": [
                {"role": "assistant", "content": "/model kimi/moonshot-v1-8k"}
            ]
        });
        assert!(parse_and_strip(&mut body).is_none());
    }

    #[test]
    fn command_must_lead_the_message() {
        let mut body = json!({
            "messages": [
                {"role": "user", "content": "please run /model kimi/moonshot-v1-8k"}
            ]
        });
        assert!(parse_and_strip(&mut body).is_none());
        assert_eq!(
            body["messages"][0]["content"],
            "please run /model kimi/moonshot-v1-8k"
        );
    }

    #[test]
    fn leading_whitespace_before_command_is_accepted() {
        let mut body = json!({
            "messages": [
                {"role": "user", "content": "  /model zai/glm-4"}
            ]
        });
        let cmd = parse_and_strip(&mut body).expect("command");
        assert_eq!(cmd.provider, "zai");
        assert_eq!(cmd.model, "glm-4");
    }

    #[test]
    fn ignores_empty_provider_or_model() {
        for text in ["/model /glm-4", "/model zai/"] {
            let mut body = json!({
                "messages": [{"role": "user", "content": text}]
            });
            assert!(
                parse_and_strip(&mut body).is_none(),
                "should ignore {text:?}"
            );
        }
    }

    #[test]
    fn skips_non_text_blocks_and_matches_a_later_text_block() {
        let mut body = json!({
            "messages": [
                {"role": "user", "content": [
                    {"type": "image", "source": {"type": "base64", "data": "..."}},
                    {"type": "text", "text": "/model zai/glm-4"}
                ]}
            ]
        });
        let cmd = parse_and_strip(&mut body).expect("command");
        assert_eq!(cmd.provider, "zai");
        // Only the emptied text block is removed; the image block stays.
        let blocks = body["messages"][0]["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["type"], "image");
    }

    #[test]
    fn first_matching_user_message_wins() {
        let mut body = json!({
            "messages": [
                {"role": "user", "content": "no command here"},
                {"role": "user", "content": "/model kimi/moonshot-v1-8k"},
                {"role": "user", "content": "/model zai/glm-4"}
            ]
        });
        let cmd = parse_and_strip(&mut body).expect("command");
        assert_eq!(cmd.provider, "kimi");
        // Only the matched (command-only) message is removed; the later
        // command is left for a subsequent pass.
        assert_eq!(body["messages"].as_array().unwrap().len(), 2);
    }
}
