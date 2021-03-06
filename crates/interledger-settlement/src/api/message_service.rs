use crate::core::types::{SettlementAccount, SE_ILP_ADDRESS};
use async_trait::async_trait;
use futures::{compat::Future01CompatExt, TryFutureExt};
use interledger_packet::{ErrorCode, FulfillBuilder, RejectBuilder};
use interledger_service::{Account, IlpResult, IncomingRequest, IncomingService};
use log::error;
use reqwest::Client;
use std::marker::PhantomData;
use tokio_retry::{strategy::ExponentialBackoff, Retry};

const PEER_FULFILLMENT: [u8; 32] = [0; 32];

/// Service which implements [`IncomingService`](../../interledger_service/trait.IncomingService.html).
/// Responsible for catching incoming requests which are sent to `peer.settle` and forward them to
/// the node's settlement engine via HTTP
#[derive(Clone)]
pub struct SettlementMessageService<I, A> {
    /// The next incoming service which requests that don't get caught get sent to
    next: I,
    /// HTTP client used to notify the engine corresponding to the account about
    /// an incoming message from a peer's engine
    http_client: Client,
    account_type: PhantomData<A>,
}

impl<I, A> SettlementMessageService<I, A>
where
    I: IncomingService<A>,
    A: SettlementAccount + Account,
{
    pub fn new(next: I) -> Self {
        SettlementMessageService {
            next,
            http_client: Client::new(),
            account_type: PhantomData,
        }
    }
}

#[async_trait]
impl<I, A> IncomingService<A> for SettlementMessageService<I, A>
where
    I: IncomingService<A> + Send,
    A: SettlementAccount + Account + Send + Sync,
{
    async fn handle_request(&mut self, request: IncomingRequest<A>) -> IlpResult {
        // Only handle the request if the destination address matches the ILP address
        // of the settlement engine being used for this account
        if let Some(settlement_engine_details) = request.from.settlement_engine_details() {
            if request.prepare.destination() == SE_ILP_ADDRESS.clone() {
                let mut settlement_engine_url = settlement_engine_details.url;
                // The `Prepare` packet's data was sent by the peer's settlement
                // engine so we assume it is in a format that our settlement engine
                // will understand
                // format. `to_vec()` needed to work around lifetime error
                let message = request.prepare.data().to_vec();

                settlement_engine_url
                    .path_segments_mut()
                    .expect("Invalid settlement engine URL")
                    .push("accounts")
                    .push(&request.from.id().to_string())
                    .push("messages");
                let idempotency_uuid = uuid::Uuid::new_v4().to_hyphenated().to_string();
                let http_client = self.http_client.clone();
                let action = move || {
                    http_client
                        .post(settlement_engine_url.as_ref())
                        .header("Content-Type", "application/octet-stream")
                        .header("Idempotency-Key", idempotency_uuid.clone())
                        .body(message.clone())
                        .send()
                        .compat() // Wrap to a 0.1 future
                };

                // TODO: tokio-retry is still not on futures 0.3. As a result, we wrap our action in a
                // 0.1 future, and then wrap the Retry future in a 0.3 future to use async/await.
                let response = Retry::spawn(ExponentialBackoff::from_millis(10).take(10), action)
                    .compat()
                    .map_err(move |error| {
                        error!("Error sending message to settlement engine: {:?}", error);
                        RejectBuilder {
                            code: ErrorCode::T00_INTERNAL_ERROR,
                            message: b"Error sending message to settlement engine",
                            data: &[],
                            triggered_by: Some(&SE_ILP_ADDRESS),
                        }
                        .build()
                    })
                    .await?;
                let status = response.status();
                if status.is_success() {
                    let body = response
                        .bytes()
                        .map_err(|err| {
                            error!(
                                "Error concatenating settlement engine response body: {:?}",
                                err
                            );
                            RejectBuilder {
                                code: ErrorCode::T00_INTERNAL_ERROR,
                                message: b"Error getting settlement engine response",
                                data: &[],
                                triggered_by: Some(&SE_ILP_ADDRESS),
                            }
                            .build()
                        })
                        .await?;

                    return Ok(FulfillBuilder {
                        fulfillment: &PEER_FULFILLMENT,
                        data: body.as_ref(),
                    }
                    .build());
                } else {
                    error!(
                        "Settlement engine rejected message with HTTP error code: {}",
                        response.status()
                    );
                    let code = if status.is_client_error() {
                        ErrorCode::F00_BAD_REQUEST
                    } else {
                        ErrorCode::T00_INTERNAL_ERROR
                    };

                    return Err(RejectBuilder {
                        code,
                        message: format!(
                            "Settlement engine rejected request with error code: {}",
                            response.status()
                        )
                        .as_str()
                        .as_ref(),
                        data: &[],
                        triggered_by: Some(&SE_ILP_ADDRESS),
                    }
                    .build());
                }
            }
        }
        self.next.handle_request(request).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::fixtures::{BODY, DATA, SERVICE_ADDRESS, TEST_ACCOUNT_0};
    use crate::api::test_helpers::{mock_message, test_service};
    use interledger_packet::{Address, Fulfill, PrepareBuilder, Reject};
    use std::str::FromStr;
    use std::time::SystemTime;

    #[tokio::test]
    async fn settlement_ok() {
        // happy case
        let m = mock_message(200).create();
        let mut settlement = test_service();
        let fulfill: Fulfill = settlement
            .handle_request(IncomingRequest {
                from: TEST_ACCOUNT_0.clone(),
                prepare: PrepareBuilder {
                    amount: 0,
                    expires_at: SystemTime::now(),
                    destination: SE_ILP_ADDRESS.clone(),
                    data: DATA.as_bytes(),
                    execution_condition: &[0; 32],
                }
                .build(),
            })
            .await
            .unwrap();

        m.assert();
        assert_eq!(fulfill.data(), BODY.as_bytes());
        assert_eq!(fulfill.fulfillment(), &[0; 32]);
    }

    #[tokio::test]
    async fn gets_forwarded_if_destination_not_engine_() {
        let m = mock_message(200).create().expect(0);
        let mut settlement = test_service();
        let destination = Address::from_str("example.some.address").unwrap();
        let reject: Reject = settlement
            .handle_request(IncomingRequest {
                from: TEST_ACCOUNT_0.clone(),
                prepare: PrepareBuilder {
                    amount: 0,
                    expires_at: SystemTime::now(),
                    destination,
                    data: DATA.as_bytes(),
                    execution_condition: &[0; 32],
                }
                .build(),
            })
            .await
            .unwrap_err();

        m.assert();
        assert_eq!(reject.code(), ErrorCode::F02_UNREACHABLE);
        assert_eq!(reject.triggered_by().unwrap(), SERVICE_ADDRESS.clone());
        assert_eq!(reject.message(), b"No other incoming handler!" as &[u8],);
    }

    #[tokio::test]
    async fn account_does_not_have_settlement_engine() {
        let m = mock_message(200).create().expect(0);
        let mut settlement = test_service();
        let mut acc = TEST_ACCOUNT_0.clone();
        acc.no_details = true; // Hide the settlement engine data from the account
        let reject: Reject = settlement
            .handle_request(IncomingRequest {
                from: acc.clone(),
                prepare: PrepareBuilder {
                    amount: 0,
                    expires_at: SystemTime::now(),
                    destination: acc.ilp_address,
                    data: DATA.as_bytes(),
                    execution_condition: &[0; 32],
                }
                .build(),
            })
            .await
            .unwrap_err();

        m.assert();
        assert_eq!(reject.code(), ErrorCode::F02_UNREACHABLE);
        assert_eq!(reject.triggered_by().unwrap(), SERVICE_ADDRESS.clone());
        assert_eq!(reject.message(), b"No other incoming handler!");
    }

    #[tokio::test]
    async fn settlement_engine_rejects() {
        // for whatever reason the engine rejects our request with a 500 code
        let error_code = 500;
        let error_str = "Internal Server Error";
        let m = mock_message(error_code).create();
        let mut settlement = test_service();
        let reject: Reject = settlement
            .handle_request(IncomingRequest {
                from: TEST_ACCOUNT_0.clone(),
                prepare: PrepareBuilder {
                    amount: 0,
                    expires_at: SystemTime::now(),
                    destination: SE_ILP_ADDRESS.clone(),
                    data: DATA.as_bytes(),
                    execution_condition: &[0; 32],
                }
                .build(),
            })
            .await
            .unwrap_err();

        m.assert();
        assert_eq!(reject.code(), ErrorCode::T00_INTERNAL_ERROR);
        // The engine rejected the message, not the connector's service,
        // so the triggered by should be the ilp address of th engine - I think.
        assert_eq!(reject.triggered_by().unwrap(), SE_ILP_ADDRESS.clone());
        assert_eq!(
            reject.message(),
            format!(
                "Settlement engine rejected request with error code: {} {}",
                error_code, error_str
            )
            .as_bytes(),
        );
    }
}
