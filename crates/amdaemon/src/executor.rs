//! `amdaemon::server::RequestExecutor<Response, Request>`.
//!
//! A request is pending while the client's request generation (first `u32` of the request) differs from
//! the server's response generation (first `u32` of the response). Evidence: constructor `0x140024FB0`,
//! accept edge (BoardIo `0x140024710`), `checkResult` `0x140019410` (with `succeeded`) and
//! `0x1400255E0` (generation-only responses: BoardIO LED, JVS).

/// `amdaemon::server::RequestState`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(u32)]
pub enum RequestState {
    /// No request owned by the executor.
    #[default]
    Idle = 0,
    /// Accepted; completion is polled through the checker.
    Pending = 1,
    /// Completed successfully.
    Succeeded = 2,
    /// Completed with failure.
    Failed = 3,
}

/// Completion record written into the response when a request finishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Completion {
    /// Generation mirrored from the request.
    pub generation: u32,
    /// `succeeded` byte value, `None` for generation-only responses.
    pub succeeded: Option<bool>,
}

/// Outcome of one `checkResult` call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckOutcome {
    /// No request in flight (generations equal or executor idle).
    Idle,
    /// Still pending.
    Pending,
    /// Request finished; the caller must write the completion into the response.
    Completed(Completion),
    /// The checker reported `Idle` while pending (the original logs an error and completes with failure).
    CompletedAfterInvalidCheck(Completion),
}

/// Executor state (`_state` at executor `+0x50`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RequestExecutor {
    state: RequestState,
}

impl RequestExecutor {
    /// Current `_state`.
    pub fn state(&self) -> RequestState {
        self.state
    }

    /// `isBusy` contribution: a request is outstanding and owned.
    pub fn is_busy(&self, request_generation: u32, response_generation: u32) -> bool {
        request_generation != response_generation && self.state != RequestState::Idle
    }

    /// Accept edge: when a new request generation is visible and the executor is idle, run the acceptor.
    pub fn accept(
        &mut self,
        request_generation: u32,
        response_generation: u32,
        acceptor: impl FnOnce() -> RequestState,
    ) -> RequestState {
        if request_generation != response_generation && self.state == RequestState::Idle {
            self.state = acceptor();
        }
        self.state
    }

    /// `checkResult` for responses carrying a `succeeded` byte (`0x140019410`).
    pub fn check_result(
        &mut self,
        request_generation: u32,
        response_generation: u32,
        checker: impl FnOnce() -> RequestState,
    ) -> CheckOutcome {
        if request_generation == response_generation || self.state == RequestState::Idle {
            return CheckOutcome::Idle;
        }
        let mut invalid = false;
        let result = match self.state {
            RequestState::Pending => match checker() {
                RequestState::Pending => return CheckOutcome::Pending,
                RequestState::Idle => {
                    invalid = true;
                    RequestState::Failed
                }
                other => other,
            },
            other => other,
        };
        self.state = RequestState::Idle;
        let completion =
            Completion { generation: request_generation, succeeded: Some(result == RequestState::Succeeded) };
        if invalid { CheckOutcome::CompletedAfterInvalidCheck(completion) } else { CheckOutcome::Completed(completion) }
    }

    /// `checkResult` for generation-only responses (`0x1400255E0`): no `succeeded` byte is written.
    pub fn check_result_generation_only(
        &mut self,
        request_generation: u32,
        response_generation: u32,
        checker: Option<&mut dyn FnMut() -> RequestState>,
    ) -> CheckOutcome {
        if request_generation == response_generation || self.state == RequestState::Idle {
            return CheckOutcome::Idle;
        }
        let mut invalid = false;
        if self.state == RequestState::Pending
            && let Some(checker) = checker
        {
            match checker() {
                RequestState::Pending => return CheckOutcome::Pending,
                RequestState::Idle => invalid = true,
                RequestState::Succeeded | RequestState::Failed => {}
            }
        }
        self.state = RequestState::Idle;
        let completion = Completion { generation: request_generation, succeeded: None };
        if invalid { CheckOutcome::CompletedAfterInvalidCheck(completion) } else { CheckOutcome::Completed(completion) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accept_runs_only_on_new_generation_while_idle() {
        let mut exec = RequestExecutor::default();
        assert_eq!(exec.accept(4, 4, || RequestState::Pending), RequestState::Idle);
        assert_eq!(exec.accept(5, 4, || RequestState::Pending), RequestState::Pending);
        assert_eq!(exec.accept(5, 4, || RequestState::Failed), RequestState::Pending);
    }

    #[test]
    fn invalid_checker_result_completes_as_failure() {
        let mut exec = RequestExecutor::default();
        exec.accept(2, 1, || RequestState::Pending);
        let out = exec.check_result(2, 1, || RequestState::Idle);
        assert_eq!(out, CheckOutcome::CompletedAfterInvalidCheck(Completion { generation: 2, succeeded: Some(false) }));
    }

    #[test]
    fn generation_only_completion_omits_succeeded() {
        let mut exec = RequestExecutor::default();
        exec.accept(3, 0, || RequestState::Failed);
        let out = exec.check_result_generation_only(3, 0, None);
        assert_eq!(
            (out, exec.state()),
            (CheckOutcome::Completed(Completion { generation: 3, succeeded: None }), RequestState::Idle)
        );
    }
}
