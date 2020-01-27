use crate::{http_retry::Client, number_or_string, AccountDetails, AccountSettings, NodeStore};
use bytes::Bytes;
use futures::{future::join_all, TryFutureExt};
use interledger_btp::{connect_to_service_account, BtpAccount, BtpOutgoingService};
use interledger_ccp::{CcpRoutingAccount, Mode, RouteControlRequest, RoutingRelation};
use interledger_http::{deserialize_json, error::*, HttpAccount, HttpStore};
use interledger_ildcp::IldcpRequest;
use interledger_ildcp::IldcpResponse;
use interledger_router::RouterStore;
use interledger_service::{
    Account, AddressStore, IncomingService, OutgoingRequest, OutgoingService, Username,
};
use interledger_service_util::{BalanceStore, ExchangeRateStore};
use interledger_settlement::core::types::SettlementAccount;
use interledger_spsp::{pay, SpspResponder};
use interledger_stream::StreamNotificationsStore;
use log::{debug, error, trace};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::convert::TryFrom;
use uuid::Uuid;
use warp::{self, reply::Json, Filter, Rejection};

pub const BEARER_TOKEN_START: usize = 7;

#[derive(Deserialize, Debug)]
struct SpspPayRequest {
    receiver: String,
    #[serde(deserialize_with = "number_or_string")]
    source_amount: u64,
}

pub fn accounts_api<I, O, S, A, B>(
    server_secret: Bytes,
    admin_api_token: String,
    default_spsp_account: Option<Username>,
    incoming_handler: I,
    outgoing_handler: O,
    btp: BtpOutgoingService<B, A>,
    store: S,
) -> impl warp::Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    I: IncomingService<A> + Clone + Send + Sync + 'static,
    O: OutgoingService<A> + Clone + Send + Sync + 'static,
    B: OutgoingService<A> + Clone + Send + Sync + 'static,
    S: NodeStore<Account = A>
        + HttpStore<Account = A>
        + BalanceStore<Account = A>
        + StreamNotificationsStore<Account = A>
        + ExchangeRateStore
        + RouterStore,
    A: BtpAccount
        + CcpRoutingAccount
        + SettlementAccount
        + Account
        + HttpAccount
        + Serialize
        + Send
        + Sync
        + 'static,
{
    // TODO can we make any of the Filters const or put them in lazy_static?
    let with_store = warp::any().map(move || store.clone()).boxed();
    let with_incoming_handler = warp::any().map(move || incoming_handler.clone()).boxed();

    // Helper filters
    let admin_auth_header = format!("Bearer {}", admin_api_token);
    let admin_auth_header_clone = admin_auth_header.clone();
    let with_admin_auth_header = warp::any().map(move || admin_auth_header.clone()).boxed();
    let admin_only = warp::header::<SecretString>("authorization")
        .and_then(move |authorization: SecretString| {
            let admin_auth_header = admin_auth_header_clone.clone();
            async move {
                if authorization.expose_secret() == &admin_auth_header {
                    Ok::<(), Rejection>(())
                } else {
                    Err(Rejection::from(ApiError::unauthorized()))
                }
            }
        })
        // This call makes it so we do not pass on a () value on
        // success to the next filter, it just gets rid of it
        .untuple_one()
        .boxed();

    // Converts an account username to an account id or errors out
    let account_username_to_id = warp::path::param::<Username>()
        .and(with_store.clone())
        .and_then(move |username: Username, store: S| {
            async move {
                store
                    .get_account_id_from_username(&username)
                    .map_err(|_| {
                        // TODO differentiate between server error and not found
                        error!("Error getting account id from username: {}", username);
                        Rejection::from(ApiError::account_not_found())
                    })
                    .await
            }
        })
        .boxed();

    let is_authorized_user = move |store: S, path_username: Username, auth_string: SecretString| {
        async move {
            if auth_string.expose_secret().len() < BEARER_TOKEN_START {
                return Err(Rejection::from(ApiError::bad_request()));
            }

            // Try getting the account from the store
            let authorized_account = store
                .get_account_from_http_auth(
                    &path_username,
                    &auth_string.expose_secret()[BEARER_TOKEN_START..],
                )
                .map_err(|_| Rejection::from(ApiError::unauthorized()))
                .await?;

            // Only return the account if the provided username matched the fetched one
            // This maybe is redundant?
            if &path_username == authorized_account.username() {
                Ok(authorized_account)
            } else {
                Err(ApiError::unauthorized().into())
            }
        }
    };

    // Checks if the account is an admin or if they have provided a valid password
    let admin_or_authorized_user_only = warp::path::param::<Username>()
        .and(warp::header::<SecretString>("authorization"))
        .and(with_store.clone())
        .and(with_admin_auth_header)
        .and_then(
            move |path_username: Username,
                  auth_string: SecretString,
                  store: S,
                  admin_auth_header: String| {
                async move {
                    // If it's an admin, there's no need for more checks
                    if auth_string.expose_secret() == &admin_auth_header {
                        let account_id = store
                            .get_account_id_from_username(&path_username)
                            .map_err(|_| {
                                // TODO differentiate between server error and not found
                                error!("Error getting account id from username: {}", path_username);
                                Rejection::from(ApiError::account_not_found())
                            })
                            .await?;
                        return Ok(account_id);
                    }
                    let account = is_authorized_user(store, path_username, auth_string).await?;
                    Ok::<Uuid, Rejection>(account.id())
                }
            },
        )
        .boxed();

    // Checks if the account has provided a valid password (same as admin-or-auth call, minus one call, can we refactor them together?)
    let authorized_user_only = warp::path::param::<Username>()
        .and(warp::header::<SecretString>("authorization"))
        .and(with_store.clone())
        .and_then(
            move |path_username: Username, auth_string: SecretString, store: S| {
                async move {
                    let account = is_authorized_user(store, path_username, auth_string).await?;
                    Ok::<A, Rejection>(account)
                }
            },
        )
        .boxed();

    // POST /accounts
    let btp_clone = btp.clone();
    let outgoing_handler_clone = outgoing_handler.clone();
    let post_accounts = warp::post()
        .and(warp::path("accounts"))
        .and(warp::path::end())
        .and(admin_only.clone())
        .and(deserialize_json()) // Why does warp::body::json not work?
        .and(with_store.clone())
        .and_then(move |account_details: AccountDetails, store: S| {
            let store_clone = store.clone();
            let handler = outgoing_handler_clone.clone();
            let btp = btp_clone.clone();
            async move {
                let account = store
                    .insert_account(account_details.clone())
                    .map_err(move |_| {
                        error!("Error inserting account into store: {:?}", account_details);
                        // TODO need more information
                        Rejection::from(ApiError::internal_server_error())
                    })
                    .await?;

                connect_to_external_services(handler, account.clone(), store_clone, btp).await?;
                Ok::<Json, Rejection>(warp::reply::json(&account))
            }
        })
        .boxed();

    // GET /accounts
    let get_accounts = warp::get()
        .and(warp::path("accounts"))
        .and(warp::path::end())
        .and(admin_only.clone())
        .and(with_store.clone())
        .and_then(|store: S| {
            async move {
                let accounts = store
                    .get_all_accounts()
                    .map_err(|_| Rejection::from(ApiError::internal_server_error()))
                    .await?;
                Ok::<Json, Rejection>(warp::reply::json(&accounts))
            }
        })
        .boxed();

    // PUT /accounts/:username
    let btp_clone = btp.clone();
    let outgoing_handler_clone = outgoing_handler.clone();
    let put_account = warp::put()
        .and(warp::path("accounts"))
        .and(account_username_to_id.clone())
        .and(warp::path::end())
        .and(admin_only.clone())
        .and(deserialize_json()) // warp::body::json() is not able to decode this!
        .and(with_store.clone())
        .and_then(move |id: Uuid, account_details: AccountDetails, store: S| {
            let outgoing_handler = outgoing_handler_clone.clone();
            let btp = btp_clone.clone();
            if account_details.ilp_over_btp_incoming_token.is_some() {
                // if the BTP token was provided, assume that it's different
                // from the existing one and drop the connection
                // the saved websocket connection
                // a new one will be initialized in the `connect_to_external_services` call
                btp.close_connection(&id);
            }
            async move {
                let account = store
                    .update_account(id, account_details)
                    .map_err(|_| Rejection::from(ApiError::internal_server_error()))
                    .await?;
                connect_to_external_services(outgoing_handler, account.clone(), store, btp).await?;

                Ok::<Json, Rejection>(warp::reply::json(&account))
            }
        })
        .boxed();

    // GET /accounts/:username
    let get_account = warp::get()
        .and(warp::path("accounts"))
        // takes the username and the authorization header and checks if it's authorized, returns the uid
        .and(admin_or_authorized_user_only.clone())
        .and(warp::path::end())
        .and(with_store.clone())
        .and_then(|id: Uuid, store: S| {
            async move {
                let accounts = store
                    .get_accounts(vec![id])
                    .map_err(|_| Rejection::from(ApiError::account_not_found()))
                    .await?;

                Ok::<Json, Rejection>(warp::reply::json(&accounts[0]))
            }
        })
        .boxed();

    // GET /accounts/:username/balance
    let get_account_balance = warp::get()
        .and(warp::path("accounts"))
        // takes the username and the authorization header and checks if it's authorized, returns the uid
        .and(admin_or_authorized_user_only.clone())
        .and(warp::path("balance"))
        .and(warp::path::end())
        .and(with_store.clone())
        .and_then(|id: Uuid, store: S| {
            async move {
                // TODO reduce the number of store calls it takes to get the balance
                let mut accounts = store
                    .get_accounts(vec![id])
                    .map_err(|_| warp::reject::not_found())
                    .await?;
                let account = accounts.pop().unwrap();

                let balance = store
                    .get_balance(account.clone())
                    .map_err(move |_| {
                        error!("Error getting balance for account: {}", id);
                        Rejection::from(ApiError::internal_server_error())
                    })
                    .await?;

                let asset_scale = account.asset_scale();
                let asset_code = account.asset_code().to_owned();
                Ok::<Json, Rejection>(warp::reply::json(&json!({
                    // normalize to the base unit
                    "balance": balance as f64 / 10_u64.pow(asset_scale.into()) as f64,
                    "asset_code": asset_code,
                })))
            }
        })
        .boxed();

    // DELETE /accounts/:username
    let btp_clone = btp.clone();
    let delete_account = warp::delete()
        .and(warp::path("accounts"))
        .and(account_username_to_id.clone())
        .and(warp::path::end())
        .and(admin_only)
        .and(with_store.clone())
        .and_then(move |id: Uuid, store: S| {
            let btp = btp_clone.clone();
            async move {
                let account = store
                    .delete_account(id)
                    .map_err(|_| {
                        error!("Error deleting account {}", id);
                        Rejection::from(ApiError::internal_server_error())
                    })
                    .await?;
                // close the btp connection (if any)
                btp.close_connection(&id);
                Ok::<Json, Rejection>(warp::reply::json(&account))
            }
        })
        .boxed();

    // PUT /accounts/:username/settings
    let outgoing_handler_clone = outgoing_handler;
    let put_account_settings = warp::put()
        .and(warp::path("accounts"))
        .and(admin_or_authorized_user_only.clone())
        .and(warp::path("settings"))
        .and(warp::path::end())
        .and(deserialize_json())
        .and(with_store.clone())
        .and_then(move |id: Uuid, settings: AccountSettings, store: S| {
            let btp = btp.clone();
            let outgoing_handler = outgoing_handler_clone.clone();
            async move {
                if settings.ilp_over_btp_incoming_token.is_some() {
                    // if the BTP token was provided, assume that it's different
                    // from the existing one and drop the connection
                    // the saved websocket connection
                    btp.close_connection(&id);
                }
                let modified_account = store
                    .modify_account_settings(id, settings)
                    .map_err(move |_| {
                        error!("Error updating account settings {}", id);
                        Rejection::from(ApiError::internal_server_error())
                    })
                    .await?;

                // Since the account was modified, we should also try to
                // connect to the new account:
                connect_to_external_services(
                    outgoing_handler,
                    modified_account.clone(),
                    store,
                    btp,
                )
                .await?;

                Ok::<Json, Rejection>(warp::reply::json(&modified_account))
            }
        })
        .boxed();

    // (Websocket) /accounts/:username/payments/incoming
    let incoming_payment_notifications = warp::path("accounts")
        .and(admin_or_authorized_user_only)
        .and(warp::path("payments"))
        .and(warp::path("incoming"))
        .and(warp::path::end())
        .and(warp::ws())
        .and(with_store.clone())
        .map(|_id: Uuid, ws: warp::ws::Ws, _store: S| {
            ws.on_upgrade(move |_ws: warp::ws::WebSocket| {
                async {
                    // TODO: Implement this.
                    unimplemented!()
                    //     let (tx, rx) = futures::channel::mpsc::unbounded::<PaymentNotification>();
                    //     store.add_payment_notification_subscription(id, tx);
                    //     rx.map_err(|_| -> warp::Error { unreachable!("unbounded rx never errors") })
                    //         .map(|notification| {
                    //             warp::ws::Message::text(serde_json::to_string(&notification).unwrap())
                    //         })
                    //         .map(|_| ())
                    //         .map_err(|err| error!("Error forwarding notifications to websocket: {:?}", err))
                }
            })
        })
        .boxed();

    // POST /accounts/:username/payments
    let post_payments = warp::post()
        .and(warp::path("accounts"))
        .and(authorized_user_only)
        .and(warp::path("payments"))
        .and(warp::path::end())
        .and(deserialize_json())
        .and(with_incoming_handler)
        .and_then(
            move |account: A, pay_request: SpspPayRequest, incoming_handler: I| {
                async move {
                    let receipt = pay(
                        incoming_handler,
                        account.clone(),
                        &pay_request.receiver,
                        pay_request.source_amount,
                    )
                    .map_err(|err| {
                        error!("Error sending SPSP payment: {:?}", err);
                        // TODO give a different error message depending on what type of error it is
                        Rejection::from(ApiError::internal_server_error())
                    })
                    .await?;

                    debug!("Sent SPSP payment, receipt: {:?}", receipt);
                    Ok::<Json, Rejection>(warp::reply::json(&json!(receipt)))
                }
            },
        )
        .boxed();

    // GET /accounts/:username/spsp
    let server_secret_clone = server_secret.clone();
    let get_spsp = warp::get()
        .and(warp::path("accounts"))
        .and(account_username_to_id)
        .and(warp::path("spsp"))
        .and(warp::path::end())
        .and(with_store.clone())
        .and_then(move |id: Uuid, store: S| {
            let server_secret_clone = server_secret_clone.clone();
            async move {
                let accounts = store
                    .get_accounts(vec![id])
                    .map_err(|_| Rejection::from(ApiError::internal_server_error()))
                    .await?;
                // TODO return the response without instantiating an SpspResponder (use a simple fn)
                Ok::<_, Rejection>(
                    SpspResponder::new(
                        accounts[0].ilp_address().clone(),
                        server_secret_clone.clone(),
                    )
                    .generate_http_response(),
                )
            }
        })
        .boxed();

    // GET /.well-known/pay
    // This is the endpoint a [Payment Pointer](https://github.com/interledger/rfcs/blob/master/0026-payment-pointers/0026-payment-pointers.md)
    // with no path resolves to
    let get_spsp_well_known = warp::get()
        .and(warp::path(".well-known"))
        .and(warp::path("pay"))
        .and(warp::path::end())
        .and(with_store)
        .and_then(move |store: S| {
            let default_spsp_account = default_spsp_account.clone();
            let server_secret_clone = server_secret.clone();
            async move {
                // TODO don't clone this
                if let Some(username) = default_spsp_account.clone() {
                    let id = store
                        .get_account_id_from_username(&username)
                        .map_err(|_| {
                            error!("Account not found: {}", username);
                            warp::reject::not_found()
                        })
                        .await?;

                    // TODO this shouldn't take multiple store calls
                    let mut accounts = store
                        .get_accounts(vec![id])
                        .map_err(|_| Rejection::from(ApiError::internal_server_error()))
                        .await?;

                    let account = accounts.pop().unwrap();
                    // TODO return the response without instantiating an SpspResponder (use a simple fn)
                    Ok::<_, Rejection>(
                        SpspResponder::new(
                            account.ilp_address().clone(),
                            server_secret_clone.clone(),
                        )
                        .generate_http_response(),
                    )
                } else {
                    Err(Rejection::from(ApiError::not_found()))
                }
            }
        })
        .boxed();

    get_spsp
        .or(get_spsp_well_known)
        .or(post_accounts)
        .or(get_accounts)
        .or(put_account)
        .or(delete_account)
        .or(get_account)
        .or(get_account_balance)
        .or(put_account_settings)
        .or(incoming_payment_notifications) // Commented out until tungenstite ws support is added
        .or(post_payments)
        .boxed()
}

async fn get_address_from_parent_and_update_routes<O, A, S>(
    mut service: O,
    parent: A,
    store: S,
) -> Result<(), ()>
where
    O: OutgoingService<A> + Clone + Send + Sync + 'static,
    A: CcpRoutingAccount + Clone + Send + Sync + 'static,
    S: NodeStore<Account = A> + Clone + Send + Sync + 'static,
{
    debug!(
        "Getting ILP address from parent account: {} (id: {})",
        parent.username(),
        parent.id()
    );
    let prepare = IldcpRequest {}.to_prepare();
    let fulfill = service
        .send_request(OutgoingRequest {
            from: parent.clone(), // Does not matter what we put here, they will get the account from the HTTP/BTP credentials
            to: parent.clone(),
            prepare,
            original_amount: 0,
        })
        .map_err(|err| error!("Error getting ILDCP info: {:?}", err))
        .await?;

    let info = IldcpResponse::try_from(fulfill.into_data().freeze()).map_err(|err| {
        error!(
            "Unable to parse ILDCP response from fulfill packet: {:?}",
            err
        );
    })?;
    debug!("Got ILDCP response from parent: {:?}", info);
    let ilp_address = info.ilp_address();

    debug!("ILP address is now: {}", ilp_address);
    // TODO we may want to make this trigger the CcpRouteManager to request
    let prepare = RouteControlRequest {
        mode: Mode::Sync,
        last_known_epoch: 0,
        last_known_routing_table_id: [0; 16],
        features: Vec::new(),
    }
    .to_prepare();

    debug!("Asking for routes from {:?}", parent.clone());
    let ret = join_all(vec![
        // Set the parent to be the default route for everything
        // that starts with their global prefix
        store.set_default_route(parent.id()),
        // Update our store's address
        store.set_ilp_address(ilp_address),
        // Get the parent's routes for us
        Box::pin(
            service
                .send_request(OutgoingRequest {
                    from: parent.clone(),
                    to: parent.clone(),
                    original_amount: prepare.amount(),
                    prepare: prepare.clone(),
                })
                .map_err(|_| ())
                .map_ok(|_| ()),
        ),
    ])
    .await;
    // If any of the 3 futures errored, propagate the error outside
    if ret.into_iter().any(|r| r.is_err()) {
        return Err(());
    }
    Ok(())
}

// Helper function which gets called whenever a new account is added or
// modified.
// Performed actions:
// 1. If they have a BTP uri configured: connect to their BTP socket
// 2. If they are a parent:
// 2a. Perform an ILDCP Request to get the address assigned to us by them, and
// update our store's address to that value
// 2b. Perform a RouteControl Request to make them send us any new routes
// 3. If they have a settlement engine endpoitn configured: Make a POST to the
//    engine's account creation endpoint with the account's id
async fn connect_to_external_services<O, A, S, B>(
    service: O,
    account: A,
    store: S,
    btp: BtpOutgoingService<B, A>,
) -> Result<A, warp::reject::Rejection>
where
    O: OutgoingService<A> + Clone + Send + Sync + 'static,
    A: CcpRoutingAccount + BtpAccount + SettlementAccount + Clone + Send + Sync + 'static,
    S: NodeStore<Account = A> + AddressStore + Clone + Send + Sync + 'static,
    B: OutgoingService<A> + Clone + 'static,
{
    // Try to connect to the account's BTP socket if they have
    // one configured
    if account.get_ilp_over_btp_url().is_some() {
        trace!("Newly inserted account has a BTP URL configured, will try to connect");
        connect_to_service_account(account.clone(), true, btp)
            .map_err(|_| Rejection::from(ApiError::internal_server_error()))
            .await?
    }

    // If we added a parent, get the address assigned to us by
    // them and update all of our routes
    if account.routing_relation() == RoutingRelation::Parent {
        get_address_from_parent_and_update_routes(service, account.clone(), store.clone())
            .map_err(|_| Rejection::from(ApiError::internal_server_error()))
            .await?;
    }

    // Register the account with the settlement engine
    // if a settlement_engine_url was configured on the account
    // or if there is a settlement engine configured for that
    // account's asset_code
    let default_settlement_engine = store
        .get_asset_settlement_engine(account.asset_code())
        .map_err(|_| Rejection::from(ApiError::internal_server_error()))
        .await?;

    let settlement_engine_url = account
        .settlement_engine_details()
        .map(|details| details.url)
        .or(default_settlement_engine);
    if let Some(se_url) = settlement_engine_url {
        let id = account.id();
        let http_client = Client::default();
        trace!(
            "Sending account {} creation request to settlement engine: {:?}",
            id,
            se_url.clone()
        );

        let status_code = http_client
            .create_engine_account(se_url, id)
            .map_err(|_| Rejection::from(ApiError::internal_server_error()))
            .await?;

        if status_code.is_success() {
            trace!("Account {} created on the SE", id);
        } else {
            error!(
                "Error creating account. Settlement engine responded with HTTP code: {}",
                status_code
            );
        }
    }

    Ok(account)
}

#[cfg(test)]
mod tests {
    use crate::routes::test_helpers::*;
    // TODO: Add test for GET /accounts/:username/spsp and /.well_known

    #[tokio::test]
    async fn only_admin_can_create_account() {
        let api = test_accounts_api();
        let resp = api_call(&api, "POST", "/accounts", "admin", DETAILS.clone()).await;
        assert_eq!(resp.status().as_u16(), 200);

        let resp = api_call(&api, "POST", "/accounts", "wrong", DETAILS.clone()).await;
        assert_eq!(resp.status().as_u16(), 401);
    }

    #[tokio::test]
    async fn only_admin_can_delete_account() {
        let api = test_accounts_api();
        let resp = api_call(&api, "DELETE", "/accounts/alice", "admin", DETAILS.clone()).await;
        assert_eq!(resp.status().as_u16(), 200);

        let resp = api_call(&api, "DELETE", "/accounts/alice", "wrong", DETAILS.clone()).await;
        assert_eq!(resp.status().as_u16(), 401);
    }

    #[tokio::test]
    async fn only_admin_can_modify_whole_account() {
        let api = test_accounts_api();
        let resp = api_call(&api, "PUT", "/accounts/alice", "admin", DETAILS.clone()).await;
        assert_eq!(resp.status().as_u16(), 200);

        let resp = api_call(&api, "PUT", "/accounts/alice", "wrong", DETAILS.clone()).await;
        assert_eq!(resp.status().as_u16(), 401);
    }

    #[tokio::test]
    async fn only_admin_can_get_all_accounts() {
        let api = test_accounts_api();
        let resp = api_call(&api, "GET", "/accounts", "admin", None).await;
        assert_eq!(resp.status().as_u16(), 200);

        let resp = api_call(&api, "GET", "/accounts", "wrong", None).await;
        assert_eq!(resp.status().as_u16(), 401);
    }

    #[tokio::test]
    async fn only_admin_or_user_can_get_account() {
        let api = test_accounts_api();
        let resp = api_call(&api, "GET", "/accounts/alice", "admin", DETAILS.clone()).await;
        assert_eq!(resp.status().as_u16(), 200);

        // TODO: Make this not require the username in the token
        let resp = api_call(&api, "GET", "/accounts/alice", "password", DETAILS.clone()).await;
        assert_eq!(resp.status().as_u16(), 200);

        let resp = api_call(&api, "GET", "/accounts/alice", "wrong", DETAILS.clone()).await;
        assert_eq!(resp.status().as_u16(), 401);
    }

    #[tokio::test]
    async fn only_admin_or_user_can_get_accounts_balance() {
        let api = test_accounts_api();
        let resp = api_call(&api, "GET", "/accounts/alice/balance", "admin", None).await;
        assert_eq!(resp.status().as_u16(), 200);

        // TODO: Make this not require the username in the token
        let resp = api_call(&api, "GET", "/accounts/alice/balance", "password", None).await;
        assert_eq!(resp.status().as_u16(), 200);

        let resp = api_call(&api, "GET", "/accounts/alice/balance", "wrong", None).await;
        assert_eq!(resp.status().as_u16(), 401);
    }

    #[tokio::test]
    async fn only_admin_or_user_can_modify_accounts_settings() {
        let api = test_accounts_api();
        let resp = api_call(
            &api,
            "PUT",
            "/accounts/alice/settings",
            "admin",
            DETAILS.clone(),
        )
        .await;
        assert_eq!(resp.status().as_u16(), 200);

        // TODO: Make this not require the username in the token
        let resp = api_call(
            &api,
            "PUT",
            "/accounts/alice/settings",
            "password",
            DETAILS.clone(),
        )
        .await;
        assert_eq!(resp.status().as_u16(), 200);

        let resp = api_call(
            &api,
            "PUT",
            "/accounts/alice/settings",
            "wrong",
            DETAILS.clone(),
        )
        .await;
        assert_eq!(resp.status().as_u16(), 401);
    }

    #[tokio::test]
    async fn only_admin_or_user_can_send_payment() {
        let payment: Option<serde_json::Value> = Some(serde_json::json!({
            "receiver": "some_receiver",
            "source_amount" : 10,
        }));
        let api = test_accounts_api();
        let resp = api_call(
            &api,
            "POST",
            "/accounts/alice/payments",
            "password",
            payment.clone(),
        )
        .await;
        // This should return an internal server error since we're making an invalid payment request
        // We could have set up a mockito mock to set that pay is called correctly but we merely want
        // to check that authorization and paths work as expected
        assert_eq!(resp.status().as_u16(), 500);

        // Note that the operator has indirect access to the user's token since they control the store
        let resp = api_call(
            &api,
            "POST",
            "/accounts/alice/payments",
            "admin",
            payment.clone(),
        )
        .await;
        assert_eq!(resp.status().as_u16(), 401);

        let resp = api_call(
            &api,
            "POST",
            "/accounts/alice/payments",
            "wrong",
            payment.clone(),
        )
        .await;
        assert_eq!(resp.status().as_u16(), 401);
    }
}
