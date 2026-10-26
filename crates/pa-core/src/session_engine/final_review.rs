//! The final review of an unattended run (`headless.finalReview`): once the
//! task's turn settles, the model checks its work against the task before it
//! answers. Nobody reads an unattended run's reply before it ends, so a
//! requirement the model skipped is only caught if the model looks again.

use pa_agent::types::{AgentMessage, AssistantContent, Message, ThinkingLevel};

use super::engine::SessionEngine;
use super::PromptOptions;

/// The user message that starts the final review.
pub const FINAL_REVIEW_PROMPT: &str = "Before you finish, review your work against the task:
1. Go through every requirement and interface item in the task one by one, and name the code that satisfies each. Implement anything missing.
2. For behaviour you have not exercised yet, write or run a small, focused test, and fix what fails.
3. Run `git status` and `git diff`, and remove debugging code, stray files and changes the task did not ask for.
Then give your final answer.";

impl SessionEngine {
    /// Run the final review as a new turn when the run did some work (at
    /// least one tool call) and no goal owns the run, at `thinking_level`
    /// when given (the session's level is restored afterwards). Returns
    /// whether the review ran.
    ///
    /// # Errors
    ///
    /// Returns the session's error when the review turn cannot be admitted.
    pub async fn run_final_review(
        &self,
        thinking_level: Option<ThinkingLevel>,
    ) -> anyhow::Result<bool> {
        if self.goal_state().await.active {
            return Ok(false);
        }
        let agent = self.session.agent();
        let state = agent.state().await;
        let did_work = state.messages.iter().any(|message| {
            matches!(
                message,
                AgentMessage::Standard(Message::Assistant(reply))
                    if reply.content.iter().any(|block| matches!(block, AssistantContent::ToolCall(_)))
            )
        });
        if !did_work {
            return Ok(false);
        }
        let session_level = state.thinking_level;
        if let Some(level) = thinking_level {
            agent.set_thinking_level(level).await;
        }
        let admitted = self
            .session
            .prompt(FINAL_REVIEW_PROMPT, PromptOptions::default())
            .await;
        agent.wait_for_idle().await;
        agent.set_thinking_level(session_level).await;
        admitted?;
        Ok(true)
    }
}
