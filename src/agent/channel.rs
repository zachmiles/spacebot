//! Channel: User-facing conversation process.

use crate::error::{AgentError, Result};
use crate::llm::SpacebotModel;
use crate::{ChannelId, WorkerId, BranchId, ProcessId, ProcessType, AgentDeps, InboundMessage, ProcessEvent, OutboundResponse};
use crate::hooks::SpacebotHook;
use crate::agent::status::StatusBlock;
use crate::agent::worker::Worker;
use crate::agent::branch::Branch;
use rig::agent::AgentBuilder;
use rig::completion::{CompletionModel, Prompt};
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};
use tokio::sync::broadcast;
use std::collections::HashMap;

/// Channel configuration.
#[derive(Debug, Clone)]
pub struct ChannelConfig {
    /// Maximum concurrent branches.
    pub max_concurrent_branches: usize,
    /// Maximum turns for channel LLM calls.
    pub max_turns: usize,
}

impl Default for ChannelConfig {
    fn default() -> Self {
        Self {
            max_concurrent_branches: 5,
            max_turns: 5,
        }
    }
}

/// Shared state that channel tools need to act on the channel.
///
/// Wrapped in Arc and passed to tools (branch, spawn_worker, route, cancel)
/// so they can create real Branch/Worker processes when the LLM invokes them.
#[derive(Clone)]
pub struct ChannelState {
    pub channel_id: ChannelId,
    pub history: Arc<RwLock<Vec<rig::message::Message>>>,
    pub active_branches: Arc<RwLock<HashMap<BranchId, tokio::task::JoinHandle<()>>>>,
    pub active_workers: Arc<RwLock<HashMap<WorkerId, Worker>>>,
    pub status_block: Arc<RwLock<StatusBlock>>,
    pub deps: AgentDeps,
    pub branch_system_prompt: String,
    pub worker_system_prompt: String,
    pub max_concurrent_branches: usize,
}

impl std::fmt::Debug for ChannelState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChannelState")
            .field("channel_id", &self.channel_id)
            .finish_non_exhaustive()
    }
}

/// User-facing conversation process.
pub struct Channel {
    pub id: ChannelId,
    pub title: Option<String>,
    pub config: ChannelConfig,
    pub deps: AgentDeps,
    pub hook: SpacebotHook,
    pub state: ChannelState,
    /// System prompt loaded from prompts/CHANNEL.md.
    pub system_prompt: String,
    /// Input channel for receiving messages.
    pub message_rx: mpsc::Receiver<InboundMessage>,
    /// Event receiver for process events.
    pub event_rx: broadcast::Receiver<ProcessEvent>,
    /// Outbound response sender for the messaging layer.
    pub response_tx: mpsc::Sender<OutboundResponse>,
}

impl Channel {
    /// Create a new channel.
    pub fn new(
        id: ChannelId,
        deps: AgentDeps,
        config: ChannelConfig,
        system_prompt: impl Into<String>,
        branch_system_prompt: impl Into<String>,
        worker_system_prompt: impl Into<String>,
        response_tx: mpsc::Sender<OutboundResponse>,
        event_rx: broadcast::Receiver<ProcessEvent>,
    ) -> (Self, mpsc::Sender<InboundMessage>) {
        let process_id = ProcessId::Channel(id.clone());
        let hook = SpacebotHook::new(deps.agent_id.clone(), process_id, ProcessType::Channel, deps.event_tx.clone());
        let status_block = Arc::new(RwLock::new(StatusBlock::new()));
        let history = Arc::new(RwLock::new(Vec::new()));
        let active_branches = Arc::new(RwLock::new(HashMap::new()));
        let active_workers = Arc::new(RwLock::new(HashMap::new()));
        let (message_tx, message_rx) = mpsc::channel(64);

        let state = ChannelState {
            channel_id: id.clone(),
            history: history.clone(),
            active_branches: active_branches.clone(),
            active_workers: active_workers.clone(),
            status_block: status_block.clone(),
            deps: deps.clone(),
            branch_system_prompt: branch_system_prompt.into(),
            worker_system_prompt: worker_system_prompt.into(),
            max_concurrent_branches: config.max_concurrent_branches,
        };

        let channel = Self {
            id: id.clone(),
            title: None,
            config,
            deps,
            hook,
            state,
            system_prompt: system_prompt.into(),
            message_rx,
            event_rx,
            response_tx,
        };
        
        (channel, message_tx)
    }
    
    /// Run the channel event loop.
    pub async fn run(mut self) -> Result<()> {
        tracing::info!(channel_id = %self.id, "channel started");
        
        loop {
            tokio::select! {
                Some(message) = self.message_rx.recv() => {
                    if let Err(error) = self.handle_message(message).await {
                        tracing::error!(%error, channel_id = %self.id, "error handling message");
                    }
                }
                Ok(event) = self.event_rx.recv() => {
                    if let Err(error) = self.handle_event(event).await {
                        tracing::error!(%error, channel_id = %self.id, "error handling event");
                    }
                }
                else => break,
            }
        }
        
        tracing::info!(channel_id = %self.id, "channel stopped");
        Ok(())
    }
    
    /// Handle an incoming message by running the channel's LLM agent loop.
    ///
    /// The LLM decides which tools to call: reply (to respond), branch (to think),
    /// spawn_worker (to delegate), route (to follow up with a worker), cancel, or
    /// memory_save. The tools act on the channel's shared state directly.
    async fn handle_message(&self, message: InboundMessage) -> Result<()> {
        tracing::info!(
            channel_id = %self.id,
            message_id = %message.id,
            "handling message"
        );
        
        let user_text = match &message.content {
            crate::MessageContent::Text(text) => text.clone(),
            crate::MessageContent::Media { text, .. } => text.clone().unwrap_or_default(),
        };

        // Build the system prompt with status block and identity context
        let status_text = {
            let status = self.state.status_block.read().await;
            status.render()
        };
        let mut system_prompt = self.system_prompt.clone();
        if !status_text.is_empty() {
            system_prompt.push_str("\n\n## Current Status\n\n");
            system_prompt.push_str(&status_text);
        }

        // Register per-turn channel tools (reply, branch, spawn_worker, route, cancel)
        let conversation_id = message.conversation_id.clone();
        if let Err(error) = crate::tools::add_channel_tools(
            &self.deps.tool_server,
            self.state.clone(),
            self.response_tx.clone(),
            &conversation_id,
        ).await {
            tracing::error!(%error, "failed to add channel tools");
            return Err(AgentError::Other(error.into()).into());
        }

        // Construct the model and agent for this turn
        let model_name = self.deps.routing.resolve(ProcessType::Channel, None);
        let model = SpacebotModel::make(&self.deps.llm_manager, model_name)
            .with_routing(self.deps.routing.clone());

        let agent = AgentBuilder::new(model)
            .preamble(&system_prompt)
            .default_max_turns(self.config.max_turns)
            .tool_server_handle(self.deps.tool_server.clone())
            .build();

        // Run the agent loop with the channel's persistent history
        let mut history = self.state.history.write().await;
        let result = agent.prompt(&user_text)
            .with_history(&mut history)
            .with_hook(self.hook.clone())
            .await;
        drop(history);

        // Clean up per-turn tools
        if let Err(error) = crate::tools::remove_channel_tools(&self.deps.tool_server).await {
            tracing::warn!(%error, "failed to remove channel tools");
        }

        match result {
            Ok(_response) => {
                // The reply tool already sent the response to the user.
                // The text response here is what the LLM said after all tool calls.
                tracing::debug!(channel_id = %self.id, "channel turn completed");
            }
            Err(rig::completion::PromptError::MaxTurnsError { .. }) => {
                tracing::warn!(channel_id = %self.id, "channel hit max turns");
            }
            Err(rig::completion::PromptError::PromptCancelled { reason, .. }) => {
                tracing::info!(channel_id = %self.id, %reason, "channel turn cancelled");
            }
            Err(error) => {
                tracing::error!(channel_id = %self.id, %error, "channel LLM call failed");
            }
        }
        
        Ok(())
    }
    
    /// Handle a process event (branch results, worker completions, status updates).
    async fn handle_event(&self, event: ProcessEvent) -> Result<()> {
        // Update status block
        {
            let mut status = self.state.status_block.write().await;
            status.update(&event);
        }
        
        match &event {
            ProcessEvent::BranchResult { branch_id, conclusion, .. } => {
                // Remove from active branches
                let mut branches = self.state.active_branches.write().await;
                branches.remove(branch_id);
                
                // Inject branch conclusion into history as a user message so the
                // channel LLM sees it on the next turn and can formulate a response.
                let mut history = self.state.history.write().await;
                let branch_message = format!("[Branch result]: {conclusion}");
                history.push(rig::message::Message::from(branch_message));
                
                tracing::info!(branch_id = %branch_id, "branch result incorporated");
            }
            ProcessEvent::WorkerComplete { worker_id, result, notify, .. } => {
                let mut workers = self.state.active_workers.write().await;
                workers.remove(worker_id);

                if *notify {
                    let mut history = self.state.history.write().await;
                    let worker_message = format!("[Worker completed]: {result}");
                    history.push(rig::message::Message::from(worker_message));
                }
                
                tracing::info!(worker_id = %worker_id, "worker completed");
            }
            _ => {}
        }
        
        Ok(())
    }
    
    /// Get the current status block as a string.
    pub async fn get_status(&self) -> String {
        let status = self.state.status_block.read().await;
        status.render()
    }
}

/// Spawn a branch from a ChannelState. Used by the BranchTool.
pub async fn spawn_branch_from_state(
    state: &ChannelState,
    description: impl Into<String>,
) -> std::result::Result<BranchId, AgentError> {
    let description = description.into();

    // Check branch limit
    {
        let branches = state.active_branches.read().await;
        if branches.len() >= state.max_concurrent_branches {
            return Err(AgentError::BranchLimitReached {
                channel_id: state.channel_id.to_string(),
                max: state.max_concurrent_branches,
            });
        }
    }
    
    // Clone history for the branch
    let history = {
        let h = state.history.read().await;
        h.clone()
    };
    
    let prompt = description.clone();
    let branch = Branch::new(
        state.channel_id.clone(),
        &description,
        state.deps.clone(),
        &state.branch_system_prompt,
        history,
    );
    
    let branch_id = branch.id;
    
    // Spawn the branch as a tokio task
    let handle = tokio::spawn(async move {
        if let Err(error) = branch.run(&prompt).await {
            tracing::error!(branch_id = %branch_id, %error, "branch failed");
        }
    });
    
    {
        let mut branches = state.active_branches.write().await;
        branches.insert(branch_id, handle);
    }
    
    {
        let mut status = state.status_block.write().await;
        status.add_branch(branch_id, "thinking...");
    }
    
    tracing::info!(branch_id = %branch_id, "branch spawned");
    
    Ok(branch_id)
}

/// Spawn a worker from a ChannelState. Used by the SpawnWorkerTool.
pub async fn spawn_worker_from_state(
    state: &ChannelState,
    task: impl Into<String>,
    interactive: bool,
) -> std::result::Result<WorkerId, AgentError> {
    let task = task.into();
    
    let worker = if interactive {
        let (worker, _input_tx) = Worker::new_interactive(
            Some(state.channel_id.clone()),
            &task,
            &state.worker_system_prompt,
            state.deps.clone(),
        );
        // TODO: Store input_tx somewhere accessible for routing follow-ups
        worker
    } else {
        Worker::new(
            Some(state.channel_id.clone()),
            &task,
            &state.worker_system_prompt,
            state.deps.clone(),
        )
    };
    
    let worker_id = worker.id;
    
    // Spawn the worker as a tokio task
    let deps_event_tx = state.deps.event_tx.clone();
    let agent_id = state.deps.agent_id.clone();
    let channel_id = Some(state.channel_id.clone());
    tokio::spawn(async move {
        let result = worker.run().await;
        match result {
            Ok(result_text) => {
                let _ = deps_event_tx.send(ProcessEvent::WorkerComplete {
                    agent_id,
                    worker_id,
                    channel_id,
                    result: result_text,
                    notify: true,
                }).await;
            }
            Err(error) => {
                tracing::error!(worker_id = %worker_id, %error, "worker failed");
                let _ = deps_event_tx.send(ProcessEvent::WorkerComplete {
                    agent_id,
                    worker_id,
                    channel_id,
                    result: format!("Worker failed: {error}"),
                    notify: true,
                }).await;
            }
        }
    });
    
    {
        let mut status = state.status_block.write().await;
        status.add_worker(worker_id, &task, false);
    }
    
    tracing::info!(worker_id = %worker_id, task = %task, "worker spawned");
    
    Ok(worker_id)
}
