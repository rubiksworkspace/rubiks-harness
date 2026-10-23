//! The empty-reply nudge for unattended runs (print mode): a reply that
//! stops with nothing but thinking (no text, no tool call) would end the run
//! as if the task were done. Some reasoning models (gpt-oss behind an
//! OpenAI-compatible API) stop that way mid-task; nobody is there to type
//! "continue", so the session sends it.

use pa_agent::types::{AgentMessage, AssistantContent, Message, StopReason};

use super::engine::SessionEngine;
use super::PromptOptions;

/// The user message that resumes a run whose reply came back empty.
pub const EMPTY_REPLY_NUDGE: &str = "Your last reply had no message and no tool call. Continue the task: call a tool, or give your final answer.";

impl SessionEngine {
    /// When the settled turn's reply stopped empty (only thinking), send
    /// [`EMPTY_REPLY_NUDGE`] as a new turn and wait for it to settle.
    /// Returns whether a nudge was sent; the caller bounds how many. An
    /// active goal owns continuing the run (an empty turn is its
    /// no-progress signal), so its turns are never nudged.
    ///
    /// # Errors
    ///
    /// Returns the session's error when the nudge turn cannot be admitted.
    pub async fn nudge_empty_reply(&self) -> anyhow::Result<bool> {
        if self.goal_state().await.active {
            return Ok(false);
        }
        let agent = self.session.agent();
        let empty = matches!(
            agent.state().await.messages.last(),
            Some(AgentMessage::Standard(Message::Assistant(reply)))
                if reply.stop_reason == StopReason::Stop
                    && !reply.content.iter().any(|block| match block {
                        AssistantContent::Text(text) => !text.text.trim().is_empty(),
                        AssistantContent::ToolCall(_) => true,
                        AssistantContent::Thinking(_) => false,
                    })
        );
        if !empty {
            return Ok(false);
        }
        self.session
            .prompt(EMPTY_REPLY_NUDGE, PromptOptions::default())
            .await?;
        agent.wait_for_idle().await;
        Ok(true)
    }
}
