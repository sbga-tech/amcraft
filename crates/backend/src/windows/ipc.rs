//! [`IpcPlatform`] on Win32: named mutexes, file mappings and overlapped named pipes.

use std::sync::Arc;

use crate::ipc::{IpcPlatform, Services};
use crate::mutex::NamedMutex;
use crate::pipe::{PipeEndpoint, PipeRole};
use crate::section::Section;
use crate::windows::mutex::WindowsMutex;
use crate::windows::pipe::WindowsPipe;
use crate::windows::protector::WindowsMemoryProtector;
use crate::windows::section::WindowsSection;

/// Win32 implementation of [`IpcPlatform`].
///
/// See the [module documentation](crate::windows) for the threading rules.
#[derive(Debug, Clone)]
pub struct WindowsIpcPlatform {
    protector: Arc<WindowsMemoryProtector>,
}

impl WindowsIpcPlatform {
    /// Creates the Win32 IPC platform.
    pub fn new() -> Self {
        Self { protector: Arc::new(WindowsMemoryProtector) }
    }
}

impl Default for WindowsIpcPlatform {
    fn default() -> Self {
        Self::new()
    }
}

impl IpcPlatform for WindowsIpcPlatform {
    fn services(&self) -> Services {
        Services::new(self.protector.clone())
    }

    fn open_mutex(&self, name: Option<&str>) -> Option<Box<dyn NamedMutex>> {
        WindowsMutex::open(name).map(|mutex| Box::new(mutex) as Box<dyn NamedMutex>)
    }

    fn create_section(&self, name: &str, mapping_len: usize) -> Option<(Box<dyn Section>, bool)> {
        WindowsSection::create(name, mapping_len)
            .map(|(section, created)| (Box::new(section) as Box<dyn Section>, created))
    }

    fn open_section(&self, name: &str) -> Option<Box<dyn Section>> {
        WindowsSection::open(name).map(|section| Box::new(section) as Box<dyn Section>)
    }

    fn open_pipe(&self, resource_name: &str, role: PipeRole, is_reader: bool) -> Box<dyn PipeEndpoint> {
        WindowsPipe::open(resource_name, role, is_reader)
    }
}
