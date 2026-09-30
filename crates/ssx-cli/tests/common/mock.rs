//! A local HTTP server (wiremock on its own runtime) for upload tests: no network involved.

use wiremock::{Mock, MockServer, Request, ResponseTemplate, matchers::method};

/// The server plus the runtime that drives it (the tests themselves are synchronous).
pub struct HttpMock {
    rt: tokio::runtime::Runtime,
    server: MockServer,
}

impl HttpMock {
    /// Starts a server on a free localhost port.
    pub fn start() -> Self {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("runtime");
        let server = rt.block_on(MockServer::start());
        Self { rt, server }
    }

    /// `http://127.0.0.1:PORT`.
    pub fn url(&self) -> String {
        self.server.uri()
    }

    /// Answers every request with `status` and `body`.
    pub fn respond_to_all(&self, status: u16, body: &str) {
        let mock = |m: &str| {
            Mock::given(method(m))
                .respond_with(ResponseTemplate::new(status).set_body_string(body.to_owned()))
        };
        for m in ["GET", "POST", "PUT"] {
            self.rt.block_on(mock(m).mount(&self.server));
        }
    }

    /// Every request received so far, oldest first.
    pub fn requests(&self) -> Vec<Request> {
        self.rt.block_on(self.server.received_requests()).unwrap_or_default()
    }
}
