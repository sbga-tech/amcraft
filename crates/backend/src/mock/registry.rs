//! The simulated registry.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use crate::{KeyMode, RegValue, Registry, RegistryError};

#[derive(Debug, Default)]
struct SimRegistryState {
    keys: HashMap<String, HashMap<String, RegValue>>,
    volatile: Vec<String>,
    fail_writes: bool,
}

/// In-memory registry shared between libam's backend and the test that inspects it.
#[derive(Debug, Clone, Default)]
pub struct SimRegistry {
    state: Rc<RefCell<SimRegistryState>>,
}

impl SimRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Stores a value, creating the key (test set-up).
    pub fn set(&self, key: &str, name: &str, value: RegValue) {
        let mut state = self.state.borrow_mut();
        state.keys.entry(key.to_ascii_lowercase()).or_default().insert(name.to_ascii_lowercase(), value);
    }

    /// The value as libam left it.
    pub fn get(&self, key: &str, name: &str) -> Option<RegValue> {
        let state = self.state.borrow();
        state.keys.get(&key.to_ascii_lowercase())?.get(&name.to_ascii_lowercase()).cloned()
    }

    /// Deletes a value (test set-up for a cabinet that lacks it).
    pub fn remove(&self, key: &str, name: &str) {
        if let Some(values) = self.state.borrow_mut().keys.get_mut(&key.to_ascii_lowercase()) {
            values.remove(&name.to_ascii_lowercase());
        }
    }

    /// Whether the key exists and was created volatile by libam.
    pub fn is_volatile(&self, key: &str) -> bool {
        self.state.borrow().volatile.contains(&key.to_ascii_lowercase())
    }

    /// Makes every write fail (the "registry failure" paths).
    pub fn fail_writes(&self, fail: bool) {
        self.state.borrow_mut().fail_writes = fail;
    }
}

impl Registry for SimRegistry {
    fn read(&mut self, key: &str, mode: KeyMode, name: &str) -> Result<RegValue, RegistryError> {
        let lower = key.to_ascii_lowercase();
        let mut state = self.state.borrow_mut();
        if !state.keys.contains_key(&lower) {
            if mode == KeyMode::CreateVolatile {
                state.keys.insert(lower.clone(), HashMap::new());
                state.volatile.push(lower);
            }
            return Err(RegistryError::NotFound);
        }
        state.keys[&lower].get(&name.to_ascii_lowercase()).cloned().ok_or(RegistryError::NotFound)
    }

    fn write(&mut self, key: &str, mode: KeyMode, name: &str, value: &RegValue) -> Result<(), RegistryError> {
        let lower = key.to_ascii_lowercase();
        let mut state = self.state.borrow_mut();
        if state.fail_writes {
            return Err(RegistryError::Failed(5));
        }
        if !state.keys.contains_key(&lower) {
            if mode == KeyMode::Open {
                return Err(RegistryError::NotFound);
            }
            state.keys.insert(lower.clone(), HashMap::new());
            state.volatile.push(lower.clone());
        }
        if let Some(values) = state.keys.get_mut(&lower) {
            values.insert(name.to_ascii_lowercase(), value.clone());
        }
        Ok(())
    }
}
