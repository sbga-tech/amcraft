//! [`Section`] double.

use std::sync::{Arc, Mutex};

use crate::mock::ipc::hub::locked;
use crate::section::Section;

#[derive(Debug)]
pub(super) struct LoopbackSection {
    pub(super) bytes: Arc<Mutex<Vec<u8>>>,
    pub(super) view_len: usize,
}

impl Section for LoopbackSection {
    fn byte_len(&self) -> usize {
        self.view_len
    }

    fn read(&self, offset: usize, out: &mut [u8]) -> bool {
        let Some(end) = offset.checked_add(out.len()).filter(|&end| end <= self.view_len) else { return false };
        out.copy_from_slice(&locked(&self.bytes)[offset..end]);
        true
    }

    fn write(&mut self, offset: usize, data: &[u8]) -> bool {
        let Some(end) = offset.checked_add(data.len()).filter(|&end| end <= self.view_len) else { return false };
        locked(&self.bytes)[offset..end].copy_from_slice(data);
        true
    }
}
