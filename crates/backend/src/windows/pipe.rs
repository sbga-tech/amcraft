//! [`PipeEndpoint`] on overlapped Win32 named pipes (`spec/ipc/transport.md` §2).

use std::cell::{Cell, UnsafeCell};
use std::fmt;
use std::iter;
use std::mem::ManuallyDrop;
use std::os::windows::io::{AsRawHandle, OwnedHandle};
use std::ptr;

use windows_sys::Win32::Foundation::{
    ERROR_IO_PENDING, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, FALSE, GENERIC_READ, HANDLE, TRUE, WAIT_IO_COMPLETION,
    WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, FILE_WRITE_DATA, OPEN_EXISTING,
    PIPE_ACCESS_INBOUND, PIPE_ACCESS_OUTBOUND, ReadFileEx, WriteFileEx,
};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Pipes::{ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, WaitNamedPipeW};
use windows_sys::Win32::System::Threading::{ResetEvent, WaitForSingleObject, WaitForSingleObjectEx};

use crate::pipe::{PipeEndpoint, PipeRole, WaitResult};
use crate::windows::security::{EVENT_ACCESS_MASK, OBJECT_ACCESS_MASK, SecurityHelper, create_event};
use crate::windows::util::{last_error, own_or_invalid};

/// Prefix of every pipe name (`\\.\pipe\`).
const PIPE_PREFIX: &str = r"\\.\pipe\";
/// Maximum number of alertable waits spent draining a cancelled transfer when an endpoint is destroyed.
const DRAIN_ROUNDS: u32 = 100;
/// Length of each of those waits, in milliseconds.
const DRAIN_STEP_MS: u32 = 1;

/// `ResetEvent(hEvent)` and zeroing of everything in `overlapped` except `hEvent`.
///
/// # Safety
///
/// `overlapped` points to a live `OVERLAPPED` whose `hEvent` is NULL or a live event, and no I/O is using it.
unsafe fn reset_overlapped(overlapped: *mut OVERLAPPED) {
    // SAFETY: guaranteed by the caller.
    unsafe {
        let event = (*overlapped).hEvent;
        ResetEvent(event);
        overlapped.write(OVERLAPPED { hEvent: event, ..OVERLAPPED::default() });
    }
}

/// State of one `ReadFileEx` / `WriteFileEx` request, shared with the kernel and the completion routine.
///
/// It is the `lpOverlapped` of the request, so the `OVERLAPPED` must stay the first field. The value is
/// heap-pinned for as long as a request can be outstanding.
#[derive(Debug)]
#[repr(C)]
struct AsyncIo {
    overlapped: UnsafeCell<OVERLAPPED>,
    /// A request is outstanding (`ioBusy`).
    busy: Cell<bool>,
    /// The last finished request transferred exactly the requested byte count (`lastIoOk`).
    ok: Cell<bool>,
    /// Byte count of the current request (`ioBytesRequested`).
    requested: Cell<u32>,
    /// The last request failed and its disconnect has not been applied yet.
    failed: Cell<bool>,
}

impl AsyncIo {
    /// An idle request state whose `OVERLAPPED` refers to `event`.
    fn new(event: HANDLE) -> Box<Self> {
        Box::new(Self {
            overlapped: UnsafeCell::new(OVERLAPPED { hEvent: event, ..OVERLAPPED::default() }),
            busy: Cell::new(false),
            ok: Cell::new(false),
            requested: Cell::new(0),
            failed: Cell::new(false),
        })
    }

    /// The pointer handed to the kernel as `lpOverlapped`; it addresses the whole `AsyncIo`.
    fn as_overlapped(&self) -> *mut OVERLAPPED {
        ptr::from_ref(self).cast::<OVERLAPPED>().cast_mut()
    }

    /// Marks a request of `requested` bytes as outstanding and prepares the `OVERLAPPED` for it.
    fn begin(&self, requested: u32) {
        self.busy.set(true);
        self.requested.set(requested);
        // SAFETY: callers only start a request when none is outstanding, so nothing else uses the `OVERLAPPED`; its
        // event is the live I/O event of the owning endpoint.
        unsafe { reset_overlapped(self.overlapped.get()) };
    }
}

/// `CompletionRoutine` (`0x140083800`): records the result of a finished request.
///
/// Runs as an APC inside an alertable wait of the thread that issued the request.
unsafe extern "system" fn completion_routine(error: u32, transferred: u32, overlapped: *mut OVERLAPPED) {
    // SAFETY: `overlapped` is the pointer given to `ReadFileEx` / `WriteFileEx`, i.e. `AsyncIo::as_overlapped`, so it
    // addresses the first field of a live `repr(C)` `AsyncIo`. `WindowsPipe::drop` keeps the value alive (or leaks
    // it) until this routine has run. Only `Cell`s are touched.
    let io = unsafe { &*overlapped.cast::<AsyncIo>() };
    let ok = error == 0 && transferred == io.requested.get();
    io.ok.set(ok);
    io.busy.set(false);
    if !ok {
        io.failed.set(true);
    }
}

/// Server-only state: the overlapped `ConnectNamedPipe` (`ServerImpl` +0x158, spec §2.2).
#[derive(Debug)]
struct Listener {
    event: OwnedHandle,
    /// Heap-pinned because the kernel holds its address while a connect is pending.
    overlapped: Box<UnsafeCell<OVERLAPPED>>,
    /// `connectPending`.
    pending: bool,
}

impl Listener {
    fn new(event: OwnedHandle) -> Self {
        let overlapped =
            Box::new(UnsafeCell::new(OVERLAPPED { hEvent: event.as_raw_handle(), ..OVERLAPPED::default() }));
        Self { event, overlapped, pending: false }
    }
}

/// One named-pipe endpoint (`NamedPipe::Impl` with its `ServerImpl` / `ClientImpl` behaviour, spec §2).
///
/// An endpoint whose name is empty, or whose security attributes or event cannot be created, is a dead endpoint:
/// it owns no OS object and refuses every operation.
pub(super) struct WindowsPipe {
    role: PipeRole,
    /// This endpoint reads (`true`) or writes (`false`).
    is_reader: bool,
    name_valid: bool,
    /// NUL-terminated `\\.\pipe\<resource name>`; empty for a dead endpoint.
    name: Vec<u16>,
    connected: bool,
    /// The pipe: the single server instance, or the client's connection while connected.
    handle: Option<OwnedHandle>,
    /// Manual-reset event named by the I/O `OVERLAPPED`; the completion routines never signal it.
    io_event: Option<OwnedHandle>,
    /// Never dropped while the kernel may still reference it; see [`Drop`].
    io: ManuallyDrop<Box<AsyncIo>>,
    /// Source or destination of the outstanding transfer; never reallocated while one is outstanding.
    buffer: Vec<u8>,
    /// Present for a server whose pipe instance exists.
    listener: Option<Listener>,
}

impl fmt::Debug for WindowsPipe {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = String::from_utf16_lossy(self.name.strip_suffix(&[0]).unwrap_or(&self.name));
        f.debug_struct("WindowsPipe")
            .field("name", &name)
            .field("role", &self.role)
            .field("is_reader", &self.is_reader)
            .field("name_valid", &self.name_valid)
            .field("connected", &self.connected)
            .field("handle", &self.handle)
            .field("io", &self.io)
            .field("buffer_len", &self.buffer.len())
            .field("listener", &self.listener)
            .finish_non_exhaustive()
    }
}

impl WindowsPipe {
    /// Creates the endpoint (`NamedPipe::Impl::Impl`, `ServerImpl::ServerImpl`, `ClientImpl` factory).
    ///
    /// A server creates its pipe instance and issues the first `ConnectNamedPipe`; a client makes one connect
    /// attempt. The endpoint is boxed before the first overlapped call so that every address the kernel keeps is
    /// final.
    pub(super) fn open(resource_name: &str, role: PipeRole, is_reader: bool) -> Box<Self> {
        let event_security = if resource_name.is_empty() { None } else { SecurityHelper::new(EVENT_ACCESS_MASK) };
        let io_event = event_security.as_ref().and_then(create_event);
        let name_valid = io_event.is_some();
        let name = if name_valid {
            PIPE_PREFIX.encode_utf16().chain(resource_name.encode_utf16()).chain(iter::once(0)).collect()
        } else {
            Vec::new()
        };
        let event = io_event.as_ref().map_or(ptr::null_mut(), |event| event.as_raw_handle());
        let mut pipe = Box::new(Self {
            role,
            is_reader,
            name_valid,
            name,
            connected: false,
            handle: None,
            io_event,
            io: ManuallyDrop::new(AsyncIo::new(event)),
            buffer: Vec::new(),
            listener: None,
        });
        if let Some(security) = event_security.filter(|_| name_valid) {
            match role {
                PipeRole::Server => pipe.create_server(&security),
                PipeRole::Client => {
                    pipe.pump();
                }
            }
        }
        pipe
    }

    /// Creates the first and only pipe instance and starts listening (spec §2.3).
    ///
    /// `ERROR_ACCESS_DENIED` (the first instance belongs to another process) and every other failure leave the
    /// endpoint without a handle, which makes it invalid.
    fn create_server(&mut self, event_security: &SecurityHelper) {
        let Some(pipe_security) = SecurityHelper::new(OBJECT_ACCESS_MASK) else {
            return;
        };
        let Some(connect_event) = create_event(event_security) else {
            return;
        };
        let direction = if self.is_reader { PIPE_ACCESS_INBOUND } else { PIPE_ACCESS_OUTBOUND };
        // SAFETY: `self.name` is NUL-terminated and the attributes are valid for the call. Byte mode, one instance,
        // default buffer sizes and time-out; the result is `INVALID_HANDLE_VALUE` or a fresh handle.
        let created = unsafe {
            own_or_invalid(CreateNamedPipeW(
                self.name.as_ptr(),
                direction | FILE_FLAG_OVERLAPPED | FILE_FLAG_FIRST_PIPE_INSTANCE,
                0,
                1,
                0,
                0,
                0,
                pipe_security.attributes(),
            ))
        };
        let Some(handle) = created else {
            return;
        };
        self.handle = Some(handle);
        self.listener = Some(Listener::new(connect_event));
        self.pump();
    }

    /// The raw pipe handle, if the endpoint has one.
    fn raw_pipe(&self) -> Option<HANDLE> {
        self.handle.as_ref().map(AsRawHandle::as_raw_handle)
    }

    /// `connected && handle && isValid` (`isConnected`).
    fn is_connected_now(&self) -> bool {
        self.connected && self.handle.is_some() && self.is_valid()
    }

    /// `ioBusy && connected && handle && isValid` (`isIoPending`).
    fn is_io_pending_now(&self) -> bool {
        self.io.busy.get() && self.is_connected_now()
    }

    /// `isReader && !ioBusy && isConnected`.
    fn can_read_now(&self) -> bool {
        self.is_reader && !self.io.busy.get() && self.is_connected_now()
    }

    /// `!isReader && !ioBusy && isConnected`.
    fn can_write_now(&self) -> bool {
        !self.is_reader && !self.io.busy.get() && self.is_connected_now()
    }

    /// Applies the disconnect of a request whose completion routine reported a failure.
    ///
    /// The original does this inside the routine; here the routine only records it and every `&mut self` method
    /// calls this first.
    fn reap(&mut self) {
        if self.io.failed.replace(false) {
            self.connected = false;
            self.disconnect();
        }
    }

    /// Connect procedure of the role (`Impl` vtable slot 2).
    fn connect(&mut self) -> bool {
        match self.role {
            PipeRole::Client => self.connect_client(),
            PipeRole::Server => self.connect_server(),
        }
    }

    /// `ClientImpl::connect` (spec §2.5): `false` only on an unexpected error; an absent or busy pipe is retried by
    /// a later pump.
    fn connect_client(&mut self) -> bool {
        // SAFETY: `self.name` is NUL-terminated; the default time-out (0) is requested.
        if unsafe { WaitNamedPipeW(self.name.as_ptr(), 0) } == 0 {
            return true;
        }
        let access = if self.is_reader { GENERIC_READ } else { FILE_WRITE_DATA };
        // SAFETY: `self.name` is NUL-terminated; no security attributes, no template; the result is
        // `INVALID_HANDLE_VALUE` or a fresh handle.
        let opened = unsafe {
            own_or_invalid(CreateFileW(
                self.name.as_ptr(),
                access,
                0,
                ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                ptr::null_mut(),
            ))
        };
        let Some(handle) = opened else {
            return last_error() == ERROR_PIPE_BUSY;
        };
        // Assigning closes a previous handle.
        self.handle = Some(handle);
        self.connected = true;
        true
    }

    /// `ServerImpl::connect` (spec §2.6): issues or polls the overlapped `ConnectNamedPipe`.
    fn connect_server(&mut self) -> bool {
        let Some(pipe) = self.raw_pipe() else {
            return false;
        };
        let Some(listener) = self.listener.as_mut() else {
            return false;
        };
        let overlapped = listener.overlapped.get();
        if !listener.pending {
            // SAFETY: no connect is pending, so nothing else uses the heap-pinned `OVERLAPPED`; its event is owned
            // by the listener.
            unsafe { reset_overlapped(overlapped) };
            // SAFETY: `pipe` is the open overlapped server pipe; `overlapped` stays valid until the connect has
            // completed or been cancelled (see `drop`).
            if unsafe { ConnectNamedPipe(pipe, overlapped) } != 0 {
                return false;
            }
            return match last_error() {
                ERROR_PIPE_CONNECTED => {
                    self.connected = true;
                    true
                }
                ERROR_IO_PENDING => {
                    listener.pending = true;
                    true
                }
                _ => false,
            };
        }
        // SAFETY: waits on the event owned by the listener; the wait is alertable so completion routines may run.
        if unsafe { WaitForSingleObjectEx(listener.event.as_raw_handle(), 0, TRUE) } == WAIT_OBJECT_0 {
            let mut transferred = 0u32;
            // SAFETY: `pipe` and `overlapped` are the ones passed to `ConnectNamedPipe`; `transferred` is writable.
            if unsafe { GetOverlappedResult(pipe, overlapped, &mut transferred, FALSE) } == 0 {
                return false;
            }
            listener.pending = false;
            self.connected = true;
        }
        true
    }

    /// Disconnect procedure of the role (`Impl` vtable slot 3).
    ///
    /// A server keeps its instance and re-arms it with the next connect; a client closes its handle.
    fn disconnect(&mut self) {
        match self.role {
            PipeRole::Client => self.handle = None,
            PipeRole::Server => {
                if let Some(pipe) = self.raw_pipe() {
                    // SAFETY: `pipe` is the open server pipe instance owned by `self`.
                    unsafe { DisconnectNamedPipe(pipe) };
                }
                if let Some(listener) = self.listener.as_mut() {
                    listener.pending = false;
                }
            }
        }
    }

    /// Handles the result of `ReadFileEx` / `WriteFileEx`: on failure the request is abandoned and the endpoint
    /// disconnected at once (spec §2.8).
    fn finish_start(&mut self, started: bool) -> bool {
        if !started {
            self.io.busy.set(false);
            // Nothing was delivered: keep `read_data` from exposing the previous contents of the buffer.
            self.io.requested.set(0);
            self.connected = false;
            self.disconnect();
        }
        started
    }

    /// Cancels the outstanding request and runs alertable waits until its completion routine has run.
    ///
    /// Returns `false` when it has still not run after the bounded drain.
    fn cancel_io(&self) -> bool {
        if !self.io.busy.get() {
            return true;
        }
        if let Some(pipe) = self.raw_pipe() {
            // SAFETY: `pipe` is open and the `OVERLAPPED` identifies the outstanding request.
            unsafe { CancelIoEx(pipe, self.io.as_overlapped()) };
        }
        let event = self.io_event.as_ref().map_or(ptr::null_mut(), |event| event.as_raw_handle());
        for _ in 0..DRAIN_ROUNDS {
            if !self.io.busy.get() {
                return true;
            }
            // SAFETY: alertable wait on the owned I/O event (never signalled), which lets the APC run.
            unsafe { WaitForSingleObjectEx(event, DRAIN_STEP_MS, TRUE) };
        }
        !self.io.busy.get()
    }

    /// Server part of the destruction: disconnects, or cancels the pending connect.
    fn shut_down_listener(&mut self) {
        let Some(pipe) = self.raw_pipe() else {
            return;
        };
        if self.is_connected_now() {
            // SAFETY: `pipe` is the open server pipe instance owned by `self`.
            unsafe { DisconnectNamedPipe(pipe) };
            return;
        }
        let Some(listener) = self.listener.take_if(|listener| listener.pending) else {
            return;
        };
        // SAFETY: `pipe` is open and `listener.overlapped` identifies the pending connect.
        unsafe { CancelIoEx(pipe, listener.overlapped.get()) };
        // SAFETY: waits (not alertable) on the listener's event, which the cancelled connect signals.
        let settled = unsafe { WaitForSingleObject(listener.event.as_raw_handle(), DRAIN_ROUNDS * DRAIN_STEP_MS) };
        if settled != WAIT_OBJECT_0 {
            // The kernel may still write the result; leak the `OVERLAPPED` instead of freeing it.
            std::mem::forget(listener.overlapped);
        }
    }
}

impl Drop for WindowsPipe {
    /// Spec §2.8 "Destruction", plus a drain of the cancelled transfer so that no completion routine can run
    /// against freed memory.
    fn drop(&mut self) {
        self.reap();
        let io_released = self.cancel_io();
        if self.role == PipeRole::Server {
            self.shut_down_listener();
        }
        // CloseHandle(pipe), before the events go.
        self.handle = None;
        if io_released {
            // SAFETY: no request is outstanding and no completion routine can run any more; `self.io` is not used
            // after this point.
            unsafe { ManuallyDrop::drop(&mut self.io) };
        } else {
            // A request may still complete into the buffer or reach the completion routine: leak both.
            std::mem::forget(std::mem::take(&mut self.buffer));
        }
    }
}

impl PipeEndpoint for WindowsPipe {
    fn is_valid(&self) -> bool {
        self.name_valid && (self.role == PipeRole::Client || self.handle.is_some())
    }

    fn pump(&mut self) -> bool {
        self.reap();
        if !self.is_valid() {
            return false;
        }
        if self.is_connected_now() {
            self.wait(0);
            true
        } else {
            self.connect()
        }
    }

    fn is_connected(&mut self) -> bool {
        self.reap();
        self.is_connected_now()
    }

    fn is_io_pending(&mut self) -> bool {
        self.reap();
        self.is_io_pending_now()
    }

    fn last_io_ok(&mut self) -> bool {
        self.reap();
        self.io.ok.get()
    }

    fn can_read(&mut self) -> bool {
        self.reap();
        self.can_read_now()
    }

    fn can_write(&mut self) -> bool {
        self.reap();
        self.can_write_now()
    }

    fn read(&mut self, len: usize) -> bool {
        self.reap();
        let Ok(requested) = u32::try_from(len) else {
            return false;
        };
        let Some(pipe) = self.raw_pipe().filter(|_| self.can_read_now()) else {
            return false;
        };
        // No request is outstanding (`can_read_now`), so the buffer may be reallocated.
        self.buffer.resize(len, 0);
        self.io.begin(requested);
        // SAFETY: `pipe` is an open overlapped handle; the buffer holds `requested` bytes and, like the `AsyncIo`
        // behind `as_overlapped`, is neither moved nor freed until the completion routine has run (`drop`).
        let started = unsafe {
            ReadFileEx(pipe, self.buffer.as_mut_ptr(), requested, self.io.as_overlapped(), Some(completion_routine))
        };
        self.finish_start(started != 0)
    }

    fn read_data(&self) -> &[u8] {
        if !self.is_reader || self.io.busy.get() || !self.io.ok.get() {
            return &[];
        }
        self.buffer.get(..self.io.requested.get() as usize).unwrap_or_default()
    }

    fn write(&mut self, data: &[u8]) -> bool {
        self.reap();
        let Ok(requested) = u32::try_from(data.len()) else {
            return false;
        };
        let Some(pipe) = self.raw_pipe().filter(|_| self.can_write_now()) else {
            return false;
        };
        // No request is outstanding (`can_write_now`), so the buffer may be reallocated.
        self.buffer.clear();
        self.buffer.extend_from_slice(data);
        self.io.begin(requested);
        // SAFETY: `pipe` is an open overlapped handle; the buffer holds `requested` bytes and, like the `AsyncIo`
        // behind `as_overlapped`, is neither moved nor freed until the completion routine has run (`drop`).
        let started = unsafe {
            WriteFileEx(pipe, self.buffer.as_ptr(), requested, self.io.as_overlapped(), Some(completion_routine))
        };
        self.finish_start(started != 0)
    }

    fn wait(&mut self, timeout_ms: u32) -> WaitResult {
        self.reap();
        if !self.is_io_pending_now() {
            return WaitResult::Idle;
        }
        let event = self.io_event.as_ref().map_or(ptr::null_mut(), |event| event.as_raw_handle());
        // SAFETY: alertable wait on the owned I/O event; completion routines of this thread may run during it.
        match unsafe { WaitForSingleObjectEx(event, timeout_ms, TRUE) } {
            WAIT_OBJECT_0 => WaitResult::Completed,
            WAIT_IO_COMPLETION => {
                self.reap();
                if self.is_io_pending_now() { WaitResult::CompletionRan } else { WaitResult::Completed }
            }
            WAIT_TIMEOUT => WaitResult::Timeout,
            _ => WaitResult::Failed,
        }
    }
}
