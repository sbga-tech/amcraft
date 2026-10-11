//! The scripted HTTP client.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use crate::{HttpClient, HttpError, HttpRequest, HttpResponse};

#[derive(Debug, Default)]
struct SimHttpState {
    requests: Vec<HttpRequest>,
    answers: VecDeque<Result<HttpResponse, HttpError>>,
}

/// Scripted HTTP client: answers queued with [`SimHttp::push_answer`] are returned in order; a request with no
/// queued answer fails with [`HttpError::Connect`]. Every request is recorded.
#[derive(Debug, Clone, Default)]
pub struct SimHttp {
    state: Rc<RefCell<SimHttpState>>,
}

impl SimHttp {
    /// A client with no scripted answers.
    pub fn new() -> Self {
        Self::default()
    }

    /// Queues the answer to the next request.
    pub fn push_answer(&self, answer: Result<HttpResponse, HttpError>) {
        self.state.borrow_mut().answers.push_back(answer);
    }

    /// Every request made so far.
    pub fn requests(&self) -> Vec<HttpRequest> {
        self.state.borrow().requests.clone()
    }
}

impl HttpClient for SimHttp {
    fn request(&mut self, request: &HttpRequest) -> Result<HttpResponse, HttpError> {
        let mut state = self.state.borrow_mut();
        state.requests.push(request.clone());
        state.answers.pop_front().unwrap_or(Err(HttpError::Connect))
    }
}
