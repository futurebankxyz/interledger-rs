// Adapted from the futures-retry example: https://gitlab.com/mexus/futures-retry/blob/master/examples/tcp-client-complex.rs
use futures::TryFutureExt;
use futures_retry::{ErrorHandler, FutureRetry, RetryPolicy};
use http::StatusCode;
use log::trace;
use reqwest::Client as HttpClient;
use serde_json::json;
use std::{default::Default, fmt::Display, time::Duration};
use url::Url;

// The account creation endpoint set by the engines in the [RFC](https://github.com/interledger/rfcs/pull/536)
static ACCOUNTS_ENDPOINT: &str = "accounts";
const MAX_RETRIES: usize = 10;
const DEFAULT_HTTP_TIMEOUT: Duration = Duration::from_millis(5000);

#[derive(Clone)]
pub struct Client {
    max_retries: usize,
    client: HttpClient,
}

impl Client {
    /// Timeout duration is in millisecodns
    pub fn new(timeout: Duration, max_retries: usize) -> Self {
        Client {
            client: HttpClient::builder().timeout(timeout).build().unwrap(),
            max_retries,
        }
    }

    pub async fn create_engine_account<T: Display>(
        &self,
        engine_url: Url,
        id: T,
    ) -> Result<StatusCode, reqwest::Error> {
        let mut se_url = engine_url.clone();
        let id: String = id.to_string();
        se_url
            .path_segments_mut()
            .expect("Invalid settlement engine URL")
            .push(ACCOUNTS_ENDPOINT);
        trace!(
            "Sending account {} creation request to settlement engine: {:?}",
            id,
            se_url.clone()
        );

        // The actual HTTP request which gets made to the engine
        let client = self.client.clone();

        // If the account is not found on the peer's connector, the
        // retry logic will not get triggered. When the counterparty
        // tries to add the account, they will complete the handshake.

        let msg = format!("[Engine: {}, Account: {}]", engine_url, id);
        let res = FutureRetry::new(
            move || {
                client
                    .post(se_url.as_ref())
                    .json(&json!({ "id": id }))
                    .send()
                    .map_ok(move |response| response.status())
            },
            IoHandler::new(self.max_retries, msg),
        )
        .await?;
        Ok(res)
    }
}

/// An I/O handler that counts attempts.
struct IoHandler<D> {
    max_attempts: usize,
    current_attempt: usize,
    display_name: D,
}

impl<D> IoHandler<D> {
    fn new(max_attempts: usize, display_name: D) -> Self {
        IoHandler {
            max_attempts,
            current_attempt: 0,
            display_name,
        }
    }
}

// The error handler trait implements the Retry logic based on the received
// Error Status Code.
impl<D> ErrorHandler<reqwest::Error> for IoHandler<D>
where
    D: ::std::fmt::Display,
{
    type OutError = reqwest::Error;

    fn handle(&mut self, e: reqwest::Error) -> RetryPolicy<reqwest::Error> {
        self.current_attempt += 1;
        if self.current_attempt > self.max_attempts {
            trace!(
                "[{}] All attempts ({}) have been used",
                self.display_name,
                self.max_attempts
            );
            return RetryPolicy::ForwardError(e);
        }
        trace!(
            "[{}] Attempt {}/{} has failed",
            self.display_name,
            self.current_attempt,
            self.max_attempts
        );

        // TODO: Should we make this policy more sophisticated?

        // Retry timeouts every 5s
        if e.is_timeout() {
            RetryPolicy::WaitRetry(Duration::from_secs(5))
        } else if let Some(status) = e.status() {
            if status.is_client_error() {
                // do not retry 4xx
                RetryPolicy::ForwardError(e)
            } else if status.is_server_error() {
                // Retry 5xx every 5 seconds
                RetryPolicy::WaitRetry(Duration::from_secs(5))
            } else {
                // Otherwise just retry every second
                RetryPolicy::WaitRetry(Duration::from_secs(1))
            }
        } else {
            // Retry other errors slightly more frequently since they may be
            // related to the engine not having started yet
            RetryPolicy::WaitRetry(Duration::from_secs(1))
        }
    }
}

impl Default for Client {
    fn default() -> Self {
        Client::new(DEFAULT_HTTP_TIMEOUT, MAX_RETRIES)
    }
}
