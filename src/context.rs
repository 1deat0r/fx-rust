//! Context accounting: rough token estimation + configurable limits, standing
//! in for fx's `core/config/context_limits.zig` + `token_estimate.zig` until
//! the full model-context-encoding machinery is ported.

use serde::{Deserialize, Serialize};

/// Heuristic token count: ~4 characters per token on mixed text, images
/// count as a fixed budget (they are base64 in our transcript).
pub fn estimate_tokens(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    text.len() / 4 + 1
}

/// Estimate the token footprint of a message list (the in-memory transcript).
pub fn estimate_messages<M>(messages: &[M]) -> usize
where
    M: MessageLike,
{
    let mut total = 0usize;
    for m in messages {
        total += estimate_tokens(&m.role_text());
        total += m.weigh();
    }
    total
}

/// A message we can weigh for context accounting.
pub trait MessageLike {
    fn role_text(&self) -> String;
    /// Additional weight (image blocks etc.) beyond the role text.
    fn weigh(&self) -> usize;
}

impl MessageLike for crate::providers::Message {
    fn role_text(&self) -> String {
        // Straight serialization is the closest cheap proxy for what actually
        // gets sent to the API (roles, tool call JSON, tool results).
        serde_json::to_string(self).unwrap_or_default()
    }
    fn weigh(&self) -> usize {
        self.content.iter().fold(0, |acc, b| match b {
            crate::providers::ContentBlock::Image { .. } => acc + 1200,
            _ => acc,
        })
    }
}

/// User-configurable context budget. Defaults aim at broad 200K-context
/// models; `FX_CONTEXT_LIMIT` / `FX_CONTEXT_WARN_AT` override in tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextLimits {
    /// Estimated tokens at which we warn (soft ceiling).
    pub warn_at_tokens: usize,
    /// Estimated tokens at which the agent refuses another round.
    pub max_tokens: usize,
}

impl Default for ContextLimits {
    fn default() -> Self {
        Self {
            warn_at_tokens: 180_000,
            max_tokens: 220_000,
        }
    }
}

impl ContextLimits {
    pub fn from_env() -> Self {
        let mut l = Self::default();
        if let Ok(v) = std::env::var("FX_CONTEXT_LIMIT") {
            if let Ok(n) = v.trim().parse::<usize>() {
                l.max_tokens = n;
            }
        }
        if let Ok(v) = std::env::var("FX_CONTEXT_WARN_AT") {
            if let Ok(n) = v.trim().parse::<usize>() {
                l.warn_at_tokens = n;
            }
        }
        if l.warn_at_tokens > l.max_tokens {
            l.warn_at_tokens = l.max_tokens;
        }
        l
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{ContentBlock, Message};

    #[test]
    fn rough_estimate_is_nonzero_for_text() {
        assert_eq!(estimate_tokens(""), 0);
        let m = estimate_tokens(
            "hello world this is a test sentence that should be roughly a few tokens",
        );
        assert!(m > 0);
    }

    #[test]
    fn messages_weigh_images() {
        let msg = Message {
            role: "user".into(),
            content: vec![
                ContentBlock::Text("look at this".into()),
                ContentBlock::Image {
                    media_type: "image/png".into(),
                    data: "AAAA".into(),
                },
            ],
        };
        let with_img = estimate_messages(std::slice::from_ref(&msg));
        let text_only = estimate_messages(&[Message {
            content: vec![ContentBlock::Text("look at this".into())],
            ..msg
        }]);
        assert!(with_img > text_only);
    }

    #[test]
    fn env_overrides() {
        let _g = crate::test_env::lock().lock().unwrap();
        std::env::set_var("FX_CONTEXT_LIMIT", "100000");
        let l = ContextLimits::from_env();
        assert_eq!(l.max_tokens, 100000);
        std::env::remove_var("FX_CONTEXT_LIMIT");
        drop(_g);
    }
}
