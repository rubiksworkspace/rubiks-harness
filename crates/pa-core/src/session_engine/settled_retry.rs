//! The settled-turn provider retry: the TS session's quick-retry loop
//! (`_handleRetryableError`) for embeddings that drive turns through the
//! session itself instead of the daemon's turn driver (print mode). A turn
//! that settled on a real provider failure is re-issued under the shared
//! [`ProviderRetryPolicy`](super::provider_retry::ProviderRetryPolicy).

use pa_agent::types::{AgentMessage, AssistantMessage, Message, StopReason};

use super::auto_retry::{run_turn_with_auto_retry, AutoRetryEvent};
use super::engine::SessionEngine;
use super::provider_retry::has_provider_stream_failure;
use super::TrailingAssistantFilter;

impl SessionEngine {
    /// Re-issue the settled turn while it ends on a retryable provider
    /// failure: each retry waits the policy's backoff (or the provider's
    /// `Retry-After`), drops the failed assistant from the loop context
    /// (the session file keeps it) and continues the run. Only a failure
    /// carrying the `provider_stream_failure` diagnostic enters the loop;
    /// permanent kinds (payment, auth, invalid request) and a context
    /// overflow settle at once. `observe` sees every retry event; the
    /// research log, semantic-edge recorder and telemetry see them too.
    ///
    /// # Errors
    ///
    /// Returns the agent's error when a retried run cannot continue.
    pub async fn retry_settled_provider_failure(
        &self,
        context_window: u64,
        mut observe: impl FnMut(&AutoRetryEvent) + Send,
    ) -> anyhow::Result<()> {
        let agent = self.session.agent();
        let Some(failed) = trailing_assistant(agent).await else {
            return Ok(());
        };
        if failed.stop_reason != StopReason::Error || !has_provider_stream_failure(&failed) {
            return Ok(());
        }
        let policy = self.session.provider_retry_policy();
        let research = self.session.research_log();
        let semantic_edges = self.session.semantic_edges();
        // The first attempt is the turn that already settled.
        let mut settled = Some(failed);
        run_turn_with_auto_retry(
            &policy,
            context_window,
            None,
            || {
                let settled = settled.take();
                async move {
                    if let Some(settled) = settled {
                        return Ok(settled);
                    }
                    self.session
                        .drop_trailing_assistant(TrailingAssistantFilter::ErrorOnly)
                        .await;
                    agent.continue_run().await?;
                    agent.wait_for_idle().await;
                    trailing_assistant(agent)
                        .await
                        .ok_or_else(|| anyhow::anyhow!("No response produced."))
                }
            },
            |event| {
                // A retried call reuses the failed attempt's idempotency key
                // (TS `prepareTurnRetry` / `clearTurnRetry`).
                if let Some(recorder) = &semantic_edges {
                    match &event {
                        AutoRetryEvent::Start { .. } => recorder.prepare_turn_retry(),
                        AutoRetryEvent::End { .. } => recorder.clear_turn_retry(),
                    }
                }
                if let Some(telemetry) = &self.telemetry {
                    telemetry.note_auto_retry_event(&event);
                }
                if let Some(recorder) = &research {
                    recorder.note_auto_retry(&event);
                }
                observe(&event);
                std::future::ready(Ok(()))
            },
            |delay| async move {
                tokio::time::sleep(delay).await;
                true
            },
            None,
        )
        .await?;
        Ok(())
    }
}

/// The loop context's trailing assistant message, when the last row is one.
async fn trailing_assistant(agent: &pa_agent::agent::Agent) -> Option<AssistantMessage> {
    if let Some(AgentMessage::Standard(Message::Assistant(assistant))) =
        agent.state().await.messages.last()
    {
        return Some(assistant.clone());
    }
    None
}
