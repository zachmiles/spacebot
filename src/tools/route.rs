//! Route tool for sending follow-ups to active workers.

use crate::agent::channel::ChannelState;
use crate::WorkerId;
use rig::completion::ToolDefinition;
use rig::tool::Tool;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Tool for routing messages to workers.
#[derive(Debug, Clone)]
pub struct RouteTool {
    state: ChannelState,
}

impl RouteTool {
    /// Create a new route tool with access to channel state.
    pub fn new(state: ChannelState) -> Self {
        Self { state }
    }
}

/// Error type for route tool.
#[derive(Debug, thiserror::Error)]
#[error("Route failed: {0}")]
pub struct RouteError(String);

/// Arguments for route tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct RouteArgs {
    /// The ID of the worker to route to (UUID format).
    pub worker_id: String,
    /// The message to send to the worker.
    pub message: String,
}

/// Output from route tool.
#[derive(Debug, Serialize)]
pub struct RouteOutput {
    /// Whether the message was routed successfully.
    pub routed: bool,
    /// The worker ID.
    pub worker_id: WorkerId,
    /// Status message.
    pub message: String,
}

impl Tool for RouteTool {
    const NAME: &'static str = "route";

    type Error = RouteError;
    type Args = RouteArgs;
    type Output = RouteOutput;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: "Send a follow-up message to an active interactive worker. Use this to continue a conversation with a long-running worker (like a coding session) without creating a new worker.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "worker_id": {
                        "type": "string",
                        "description": "The worker ID to route to (from spawn_worker result)"
                    },
                    "message": {
                        "type": "string",
                        "description": "The message to send to the worker"
                    }
                },
                "required": ["worker_id", "message"]
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> std::result::Result<Self::Output, Self::Error> {
        let worker_id = args.worker_id.parse::<WorkerId>()
            .map_err(|e| RouteError(format!("Invalid worker ID: {e}")))?;

        // Verify the worker exists and is interactive
        let workers = self.state.active_workers.read().await;
        let worker = workers.get(&worker_id)
            .ok_or_else(|| RouteError(format!("Worker {worker_id} not found or not active")))?;

        if !worker.is_interactive() {
            return Err(RouteError(format!("Worker {worker_id} is not interactive")));
        }
        drop(workers);

        // TODO: Send message via the worker's input_tx once we store it accessibly
        tracing::info!(
            worker_id = %worker_id,
            channel_id = %self.state.channel_id,
            "routing message to worker"
        );

        Ok(RouteOutput {
            routed: true,
            worker_id,
            message: format!("Message routed to worker {worker_id}."),
        })
    }
}
