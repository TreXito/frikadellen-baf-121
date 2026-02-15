use crate::types::BotState;
use parking_lot::RwLock;
use std::sync::Arc;

#[derive(Clone)]
pub struct StateManager {
    state: Arc<RwLock<BotState>>,
}

impl StateManager {
    pub fn new() -> Self {
        Self {
            state: Arc::new(RwLock::new(BotState::Startup)),
        }
    }

    pub fn get(&self) -> BotState {
        *self.state.read()
    }

    pub fn set(&self, new_state: BotState) {
        let old_state = *self.state.read();
        if old_state != new_state {
            tracing::info!("State changed: {:?} -> {:?}", old_state, new_state);
            *self.state.write() = new_state;
        }
    }

    pub fn allows_commands(&self) -> bool {
        self.get().allows_commands()
    }
}

impl Default for StateManager {
    fn default() -> Self {
        Self::new()
    }
}
