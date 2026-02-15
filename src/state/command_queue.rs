use crate::types::{CommandPriority, CommandType, QueuedCommand};
use parking_lot::RwLock;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};
use uuid::Uuid;

const BAZAAR_RECOMMENDATION_MAX_AGE_MS: u64 = 60_000; // 60 seconds
#[allow(dead_code)]
const COMMAND_TIMEOUT_MS: u64 = 30_000; // 30 seconds

#[derive(Clone)]
pub struct CommandQueue {
    queue: Arc<RwLock<VecDeque<QueuedCommand>>>,
    current_command: Arc<RwLock<Option<QueuedCommand>>>,
}

impl CommandQueue {
    pub fn new() -> Self {
        Self {
            queue: Arc::new(RwLock::new(VecDeque::new())),
            current_command: Arc::new(RwLock::new(None)),
        }
    }

    /// Add a command to the queue
    pub fn enqueue(&self, command_type: CommandType, priority: CommandPriority, interruptible: bool) -> Uuid {
        let id = Uuid::new_v4();
        let cmd = QueuedCommand {
            id,
            priority,
            command_type: command_type.clone(),
            queued_at: Instant::now(),
            interruptible,
        };

        let mut queue = self.queue.write();
        
        // Find insertion point based on priority
        let pos = queue
            .iter()
            .position(|c| c.priority > priority)
            .unwrap_or(queue.len());
        
        queue.insert(pos, cmd);
        
        debug!("Enqueued command {:?} with priority {:?} at position {}", id, priority, pos);
        id
    }

    /// Get the next command without removing it
    pub fn peek(&self) -> Option<QueuedCommand> {
        // First check if there's a current command
        if let Some(current) = self.current_command.read().as_ref() {
            return Some(current.clone());
        }

        // Remove stale bazaar commands (older than 60 seconds)
        self.remove_stale_commands();

        self.queue.read().front().cloned()
    }

    /// Mark the current command as started
    pub fn start_current(&self) -> Option<QueuedCommand> {
        let mut queue = self.queue.write();
        if let Some(cmd) = queue.pop_front() {
            *self.current_command.write() = Some(cmd.clone());
            info!("Starting command {:?} (priority: {:?})", cmd.id, cmd.priority);
            Some(cmd)
        } else {
            None
        }
    }

    /// Complete the current command
    pub fn complete_current(&self) {
        if let Some(cmd) = self.current_command.write().take() {
            info!("Completed command {:?}", cmd.id);
        }
    }

    /// Check if current command can be interrupted
    pub fn can_interrupt_current(&self) -> bool {
        self.current_command
            .read()
            .as_ref()
            .map(|c| c.interruptible)
            .unwrap_or(true)
    }

    /// Interrupt current command
    pub fn interrupt_current(&self) {
        if let Some(cmd) = self.current_command.write().take() {
            warn!("Interrupted command {:?}", cmd.id);
        }
    }

    /// Clear all bazaar orders from queue
    pub fn clear_bazaar_orders(&self) {
        let mut queue = self.queue.write();
        queue.retain(|cmd| {
            !matches!(
                cmd.command_type,
                CommandType::BazaarBuyOrder { .. } | CommandType::BazaarSellOrder { .. }
            )
        });
        info!("Cleared all bazaar orders from queue");
    }

    /// Remove stale commands (bazaar orders older than 60 seconds)
    fn remove_stale_commands(&self) {
        let mut queue = self.queue.write();
        let now = Instant::now();
        let max_age = Duration::from_millis(BAZAAR_RECOMMENDATION_MAX_AGE_MS);

        let original_len = queue.len();
        queue.retain(|cmd| {
            let age = now.duration_since(cmd.queued_at);
            
            // Only remove stale bazaar commands
            if matches!(
                cmd.command_type,
                CommandType::BazaarBuyOrder { .. } | CommandType::BazaarSellOrder { .. }
            ) && age > max_age
            {
                debug!("Removing stale command {:?} (age: {:?})", cmd.id, age);
                false
            } else {
                true
            }
        });

        if queue.len() < original_len {
            info!(
                "Removed {} stale command(s) from queue",
                original_len - queue.len()
            );
        }
    }

    /// Get queue size
    pub fn len(&self) -> usize {
        self.queue.read().len()
    }

    /// Check if queue is empty
    pub fn is_empty(&self) -> bool {
        self.queue.read().is_empty() && self.current_command.read().is_none()
    }
}

impl Default for CommandQueue {
    fn default() -> Self {
        Self::new()
    }
}
