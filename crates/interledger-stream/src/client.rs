use super::congestion::CongestionController;
use super::crypto::*;
use super::error::Error;
use super::packet::*;
use bytes::Bytes;
use bytes::BytesMut;
use futures::stream::{FuturesUnordered, StreamExt};
use interledger_packet::{
    Address, ErrorClass, ErrorCode as IlpErrorCode, PacketType as IlpPacketType, PrepareBuilder,
    Reject,
};
use interledger_rates::ExchangeRateStore;
use interledger_service::*;
use log::{debug, error, warn};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tokio::time::timeout;
use tokio::time::{Duration, Instant};

use std::cmp::{max, min};
use std::marker::{Send, Sync};
use std::str;
use std::sync::Arc;
use std::time::SystemTime;

/// Maximum time we should wait since last fulfill before we error out to avoid
/// getting into an infinite loop of sending packets and effectively DoSing ourselves
const MAX_TIME_SINCE_LAST_FULFILL: Duration = Duration::from_secs(30);

/// Percentage subtracted from the calculated exchange rate to determine the minimum acceptable rate
/// Used to account for fees and rate variations between connectors
const DEFAULT_MAX_SLIPPAGE: f64 = 0.01;

/// Receipt for STREAM payment to account for how much and what assets were sent & delivered
#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub struct StreamDelivery {
    /// Sender's ILP Address
    pub from: Address,
    /// Receiver's ILP Address
    pub to: Address,
    /// Asset scale of sender
    pub source_asset_scale: u8,
    /// Asset code of sender
    pub source_asset_code: String,
    /// Total amount *intended* to be sent, in source units
    pub source_amount: u64,
    /// Amount fulfilled or currently in-flight, in source units
    pub sent_amount: u64,
    /// Amount in-flight (yet to be fulfilled or rejected), in source units
    pub in_flight_amount: u64,
    /// Amount fulfilled and received by the recipient, in destination units
    pub delivered_amount: u64,
    /// Receiver's asset scale (this may change depending on the granularity of accounts across nodes)
    /// Updated after we received a `ConnectionAssetDetails` frame.
    pub destination_asset_scale: Option<u8>,
    /// Receiver's asset code
    /// Updated after we received a `ConnectionAssetDetails` frame.
    pub destination_asset_code: Option<String>,
}

/// Stream payment mutable state: amounts & assets sent and received, sequence, packet counts, and flow control parameters
struct StreamPayment {
    /// The [congestion controller](./../congestion/struct.CongestionController.html) to adjust flow control and the in-flight amount
    congestion_controller: CongestionController,
    /// The [StreamDelivery](./struct.StreamDelivery.html) receipt to account for the delivered amounts
    receipt: StreamDelivery,
    /// Do we need to send our source account information to the recipient?
    should_send_source_account: bool,
    /// Monotonically increaing sequence number for this STREAM payment
    sequence: u64,
    /// Number of fulfilled packets throughout the STREAM payment
    fulfilled_packets: u64,
    /// Number of rejected packets throughout the STREAM payment
    rejected_packets: u64,
    /// Timestamp when a packet was last fulfilled for this payment
    last_fulfill_time: Instant,
}

impl StreamPayment {
    /// Account for and return amount to send in the next Prepare
    fn apply_prepare(&mut self) -> u64 {
        let amount = min(
            self.get_amount_available_to_send(),
            self.congestion_controller.get_max_amount(),
        );

        self.congestion_controller.prepare(amount);

        self.receipt.sent_amount += amount;
        self.receipt.in_flight_amount += amount;
        amount
    }

    /// Account for a fulfilled packet and update flow control
    fn apply_fulfill(&mut self, source_amount: u64, destination_amount: u64) {
        self.congestion_controller.fulfill(source_amount);

        self.receipt.in_flight_amount -= source_amount;
        self.receipt.delivered_amount += destination_amount;

        self.last_fulfill_time = Instant::now();
        self.fulfilled_packets += 1;
    }

    /// Account for a rejected packet and update flow control
    fn apply_reject(&mut self, amount: u64, reject: &Reject) {
        self.congestion_controller.reject(amount, reject);

        self.receipt.sent_amount -= amount;
        self.receipt.in_flight_amount -= amount;

        self.rejected_packets += 1;
    }

    /// Save the recipient's destination asset details for calculating minimum exchange rates
    fn set_destination_asset_details(&mut self, asset_code: String, asset_scale: u8) {
        self.receipt.destination_asset_code = Some(asset_code);
        self.receipt.destination_asset_scale = Some(asset_scale);
    }

    /// Return the current sequence number and increment the value for subsequent packets
    fn next_sequence(&mut self) -> u64 {
        let seq = self.sequence;
        self.sequence += 1;
        seq
    }

    /// Return the amount of money remaining to be sent in the payment
    fn get_amount_available_to_send(&self) -> u64 {
        // Sent amount also includes the amount in-flight, which should be subtracted from the amount available
        self.receipt.source_amount - self.receipt.sent_amount
    }

    /// Has the entire intended source amount been fulfilled to the recipient?
    fn is_complete(&self) -> bool {
        (self.receipt.sent_amount - self.receipt.in_flight_amount) >= self.receipt.source_amount
    }

    /// Is the congestion controller restricting us from sending more money?
    fn is_max_in_flight(&self) -> bool {
        self.congestion_controller.get_max_amount() == 0
    }
}

/// Send the given source amount with packetized Interledger payments using the STREAM transport protocol
/// Returns the receipt with sent & delivered amounts, asset & account details
pub async fn send_money<I, A, S>(
    service: I,
    from_account: &A,
    destination_account: Address,
    shared_secret: &[u8],
    source_amount: u64,
    store: S,
) -> Result<StreamDelivery, Error>
where
    I: IncomingService<A> + Clone + Send + Sync + 'static,
    A: Account + Send + Sync + 'static,
    S: ExchangeRateStore + Send + Sync + 'static,
{
    // TODO Can we avoid copying here?
    let shared_secret = Bytes::from(shared_secret);

    let from_account = from_account.clone();
    let from = from_account.ilp_address().clone();
    let to = destination_account.clone();

    if from.scheme() != to.scheme() {
        warn!(
            "Destination ILP address starts with a different scheme prefix (\"{}\') than ours (\"{}\'), this probably won't work",
            to.scheme(),
            from.scheme()
        );
    }

    let payment = StreamPayment {
        // TODO Make configurable to get money flowing ASAP vs as much as possible per-packet
        congestion_controller: CongestionController::new(source_amount, source_amount / 10, 2.0),
        receipt: StreamDelivery {
            from,
            to,
            source_asset_scale: from_account.asset_scale(),
            source_asset_code: from_account.asset_code().to_string(),
            source_amount,
            sent_amount: 0,
            in_flight_amount: 0,
            destination_asset_scale: None,
            destination_asset_code: None,
            delivered_amount: 0,
        },
        should_send_source_account: true,
        sequence: 1,
        fulfilled_packets: 0,
        rejected_packets: 0,
        last_fulfill_time: Instant::now(),
    };

    let sender = StreamSender {
        next: service,
        from_account,
        shared_secret,
        store,
        slippage: DEFAULT_MAX_SLIPPAGE,
        payment: Arc::new(Mutex::new(payment)),
    };

    let mut pending_requests = FuturesUnordered::new();

    /// Actions corresponding to the state of the payment
    enum PaymentEvent {
        /// Send more money: send a packet with the given amount
        SendMoney(u64),
        /// Congestion controller has limited the amount in flight: wait for pending request to complete
        MaxInFlight,
        /// Send full source amount: close the connection and return success
        CloseConnection,
        /// Maximum timeout since last fulfill has elapsed: terminate the payment
        Timeout,
    }

    loop {
        let event = {
            let mut payment = sender.payment.lock().await;

            if payment.last_fulfill_time.elapsed() >= MAX_TIME_SINCE_LAST_FULFILL {
                PaymentEvent::Timeout
            } else if payment.is_complete() {
                PaymentEvent::CloseConnection
            } else if payment.is_max_in_flight() {
                PaymentEvent::MaxInFlight
            } else {
                PaymentEvent::SendMoney(payment.apply_prepare())
            }
        };

        match event {
            PaymentEvent::SendMoney(packet_amount) => {
                let sender = sender.clone();
                pending_requests.push(tokio::spawn(async move {
                    sender.send_money_packet(packet_amount).await
                }));
            }
            PaymentEvent::MaxInFlight => {
                // Wait for 100ms for any request to complete, otherwise try running loop again
                // to see if we reached the timeout since last fulfill
                let fut = timeout(
                    Duration::from_millis(100),
                    pending_requests.select_next_some(),
                )
                .await;

                if let Ok(Ok(Err(error))) = fut {
                    error!("Send money stopped because of error: {:?}", error);
                    return Err(error);
                }
            }
            PaymentEvent::CloseConnection => {
                // Wait for all pending requests to complete before closing the connection
                pending_requests.map(|_| ()).collect::<()>().await;

                // Try to the tell the recipient the connection is closed
                sender.try_send_connection_close().await;

                // Return final receipt
                let payment = sender.payment.lock().await;
                debug!(
                    "Send money future finished. Delivered: {} ({} packets fulfilled, {} packets rejected)",
                    payment.receipt.delivered_amount,
                    payment.fulfilled_packets,
                    payment.rejected_packets,
                );
                return Ok(payment.receipt.clone());
            }
            PaymentEvent::Timeout => {
                // Error if we haven't received a fulfill over a timeout period
                return Err(Error::TimeoutError(
                    "Time since last fulfill exceeded the maximum time limit".to_string(),
                ));
            }
        }
    }
}

/// Sends and handles all ILP & STREAM packets, encapsulating all payment state
#[derive(Clone)]
struct StreamSender<
    I: IncomingService<A> + Clone + Send + Sync + 'static,
    A: Account + Send + Sync + 'static,
    S: ExchangeRateStore + Send + Sync + 'static,
> {
    /// Next service to send and forward Interledger packets to the network
    next: I,
    /// The account sending the STREAM payment
    from_account: A,
    /// Symmetric secret generated by receiver to encrypt and authenticate this connections' packets
    shared_secret: Bytes,
    /// Store for fetching and enforcing minimum exchange rates
    store: S,
    /// Maximum acceptable slippage percentage below calculated minimum exchange rate
    slippage: f64,
    /// Mutable payment state
    payment: Arc<Mutex<StreamPayment>>,
}

impl<I, A, S> StreamSender<I, A, S>
where
    I: IncomingService<A> + Clone + Send + Sync + 'static,
    A: Account + Send + Sync + 'static,
    S: ExchangeRateStore + Send + Sync + 'static,
{
    /// Send a Prepare for the given source amount and apply the resulting Fulfill or Reject
    #[inline]
    pub async fn send_money_packet(&self, source_amount: u64) -> Result<(), Error> {
        let (prepare, sequence, min_destination_amount) = {
            let mut payment = self.payment.lock().await;

            // Build the STREAM packet
            let sequence = payment.next_sequence();
            let mut frames = vec![Frame::StreamMoney(StreamMoneyFrame {
                stream_id: 1,
                shares: 1,
            })];
            if payment.should_send_source_account {
                frames.push(Frame::ConnectionNewAddress(ConnectionNewAddressFrame {
                    source_account: payment.receipt.from.clone(),
                }));
            }
            let min_destination_amount =
                self.get_min_destination_amount(source_amount, &payment.receipt);
            let stream_request_packet = StreamPacketBuilder {
                ilp_packet_type: IlpPacketType::Prepare,
                prepare_amount: min_destination_amount,
                sequence,
                frames: &frames,
            }
            .build();

            debug!(
                "Sending packet {} with amount: {} and encrypted STREAM packet: {:?}",
                sequence, source_amount, stream_request_packet
            );

            let prepare_data = stream_request_packet.into_encrypted(&self.shared_secret);

            // If we couldn't calculate a minimum destination amount (e.g. don't know asset details yet),
            // packet MUST be unfulfillable so no money is at risk
            let execution_condition = if min_destination_amount > 0 {
                generate_condition(&self.shared_secret, &prepare_data)
            } else {
                random_condition()
            };

            // Build the Prepare
            let prepare = PrepareBuilder {
                destination: payment.receipt.to.clone(),
                amount: source_amount,
                execution_condition: &execution_condition,
                expires_at: SystemTime::now() + Duration::from_secs(30),
                // TODO Don't copy the data
                data: &prepare_data[..],
            }
            .build();

            (prepare, sequence, min_destination_amount)
        };

        // Send it!
        let reply = self
            .next
            .clone()
            .handle_request(IncomingRequest {
                from: self.from_account.clone(),
                prepare,
            })
            .await;

        let (packet_type, reply_data) = match &reply {
            Ok(fulfill) => (IlpPacketType::Fulfill, fulfill.data()),
            Err(reject) => (IlpPacketType::Reject, reject.data()),
        };

        let stream_reply_packet =
            StreamPacket::from_encrypted(&self.shared_secret, BytesMut::from(reply_data));

        let mut payment = self.payment.lock().await;

        // Parse the stream packet and determine the amount the recipient claims they received
        let claimed_amount: u64 = match stream_reply_packet {
            Ok(stream_reply_packet) => {
                if stream_reply_packet.sequence() != sequence {
                    warn!(
                        "Discarding replayed STREAM packet (expected sequence {}, but received {})",
                        sequence,
                        stream_reply_packet.sequence()
                    );
                    0
                } else if stream_reply_packet.ilp_packet_type() == IlpPacketType::Reject
                    && packet_type == IlpPacketType::Fulfill
                {
                    // If receiver claimed they sent a Reject but we got a Fulfill, they lied!
                    // If receiver said they sent a Fulfill but we got a Reject, that's possible
                    warn!("Discarding STREAM packet (received Fulfill, but recipient said they sent a Reject)");
                    0
                } else {
                    // Since we decrypted the response, the recipient read the request packet and knows our account
                    payment.should_send_source_account = false;

                    // Update the destination asset scale & code
                    // https://github.com/interledger/rfcs/pull/551 ensures that this won't change
                    if payment.receipt.destination_asset_scale.is_none() {
                        for frame in stream_reply_packet.frames() {
                            if let Frame::ConnectionAssetDetails(frame) = frame {
                                let asset_code = frame.source_asset_code.to_string();
                                let asset_scale = frame.source_asset_scale;
                                debug!(
                                    "Setting remote asset details ({} with scale {})",
                                    asset_code, asset_scale
                                );
                                payment.set_destination_asset_details(asset_code, asset_scale);
                            }
                        }
                    }

                    stream_reply_packet.prepare_amount()
                }
            }
            Err(_) => {
                warn!(
                    "Unable to parse STREAM packet from response data for sequence {}",
                    sequence
                );
                0
            }
        };

        match reply {
            // Handle ILP Fulfill
            Ok(_) => {
                // Delivered amount must be *at least* the minimum acceptable amount we told the receiver
                // Even if the data was invalid, since it was fulfilled, we must assume they got at least the minimum
                let delivered_amount = max(min_destination_amount, claimed_amount);

                payment.apply_fulfill(source_amount, delivered_amount);

                debug!(
                    "Prepare {} with amount {} was fulfilled ({} left to send)",
                    sequence,
                    source_amount,
                    payment.get_amount_available_to_send()
                );

                Ok(())
            }
            // Handle ILP Reject
            Err(reject) => {
                payment.apply_reject(source_amount, &reject);

                debug!(
                    "Prepare {} with amount {} was rejected with code: {} ({} left to send)",
                    sequence,
                    source_amount,
                    reject.code(),
                    payment.get_amount_available_to_send()
                );

                match (reject.code().class(), reject.code()) {
                    (ErrorClass::Temporary, _) => Ok(()),
                    (_, IlpErrorCode::F08_AMOUNT_TOO_LARGE) => Ok(()), // Handled by the congestion controller
                    (_, IlpErrorCode::F99_APPLICATION_ERROR) => Ok(()),
                    // Any other error will stop the rest of the payment
                    _ => Err(Error::SendMoneyError(format!(
                        "Packet was rejected with error: {} {}",
                        reject.code(),
                        str::from_utf8(reject.message()).unwrap_or_default(),
                    ))),
                }
            }
        }
    }

    /// Send an unfulfillable Prepare with a ConnectionClose frame to the peer
    /// There's no ACK from the recipient, so we can't confirm it closed
    #[inline]
    async fn try_send_connection_close(&self) {
        let prepare = {
            let mut payment = self.payment.lock().await;
            let sequence = payment.next_sequence();

            let stream_packet = StreamPacketBuilder {
                ilp_packet_type: IlpPacketType::Prepare,
                prepare_amount: 0,
                sequence,
                frames: &[Frame::ConnectionClose(ConnectionCloseFrame {
                    code: ErrorCode::NoError,
                    message: "",
                })],
            }
            .build();

            // Create the ILP Prepare packet
            let data = stream_packet.into_encrypted(&self.shared_secret);
            PrepareBuilder {
                destination: payment.receipt.to.clone(),
                amount: 0,
                execution_condition: &random_condition(),
                expires_at: SystemTime::now() + Duration::from_secs(30),
                data: &data[..],
            }
            .build()
        };

        // Send it!
        // Packet will always be rejected since the condition is random
        debug!("Closing connection");
        self.next
            .clone()
            .handle_request(IncomingRequest {
                from: self.from_account.clone(),
                prepare,
            })
            .await
            .ok();
    }

    // TODO Abstract duplicated conversion logic from interledger-settlement &
    //      exchange rate service into interledger-rates

    /// Convert the given source amount into a destination amount, pulling from a provider's exchange rates
    /// and subtracting slippage to determine a minimum destination amount.
    /// Returns 0 if destination asset details are unknown or rate cannot be calculated.
    #[inline]
    fn get_min_destination_amount(&self, source_amount: u64, receipt: &StreamDelivery) -> u64 {
        let source_code = &receipt.source_asset_code;
        let source_scale = receipt.source_asset_scale;

        let (dest_code, dest_scale) = match (
            &receipt.destination_asset_code,
            receipt.destination_asset_scale,
        ) {
            (Some(dest_code), Some(dest_scale)) => (dest_code, dest_scale),
            _ => {
                return 0; // If we don't know the destination code *or* scale, default to 0
            }
        };

        let rate: f64 = if source_code == dest_code {
            1f64
        } else if let Ok(prices) = self.store.get_exchange_rates(&[&source_code, &dest_code]) {
            prices[0] / prices[1]
        } else {
            return 0; // Default to 0 if the rate is unavailable
        };

        let rate = rate * (1.0 - self.slippage);
        let rate = if rate.is_finite() && rate.is_sign_positive() {
            rate
        } else {
            0.0
        };

        // First, convert scaled source amount to base unit
        let source_amount = (source_amount as f64) * 10f64.powi(-(source_scale as i32));

        // Apply exchange rate
        let mut dest_amount = source_amount * rate;

        // Convert destination amount in base units to scaled units
        dest_amount *= 10f64.powi(dest_scale as i32);

        // For safety, always round up
        dest_amount = dest_amount.ceil();
        dest_amount as u64
    }
}

#[cfg(test)]
mod send_money_tests {
    use super::*;
    use crate::test_helpers::{
        install_tracing_subscriber, TestAccount, TestRateStore, TestStore, EXAMPLE_CONNECTOR,
    };
    use async_trait::async_trait;
    use interledger_ildcp::IldcpService;
    use interledger_packet::{ErrorCode as IlpErrorCode, RejectBuilder};
    use interledger_service::incoming_service_fn;
    use interledger_service_util::MaxPacketAmountService;
    use parking_lot::Mutex;
    use std::str::FromStr;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tokio::time::timeout;
    use uuid::Uuid;

    #[tokio::test]
    async fn stops_at_final_errors() {
        install_tracing_subscriber();

        let account = TestAccount {
            id: Uuid::new_v4(),
            asset_code: "XYZ".to_string(),
            asset_scale: 9,
            ilp_address: Address::from_str("example.destination").unwrap(),
            max_packet_amount: None,
        };
        let requests = Arc::new(Mutex::new(Vec::new()));
        let requests_clone = requests.clone();
        let result = send_money(
            IldcpService::new(incoming_service_fn(move |request| {
                requests_clone.lock().push(request);
                Err(RejectBuilder {
                    code: IlpErrorCode::F00_BAD_REQUEST,
                    message: b"just some final error",
                    triggered_by: Some(&EXAMPLE_CONNECTOR),
                    data: &[],
                }
                .build())
            })),
            &account,
            Address::from_str("example.destination").unwrap(),
            &[0; 32][..],
            100,
            TestRateStore {},
        )
        .await;
        assert!(result.is_err());
        assert_eq!(requests.lock().len(), 1);
    }

    #[tokio::test]
    async fn sends_concurrent_packets() {
        install_tracing_subscriber();

        let destination_address = Address::from_str("example.receiver").unwrap();
        let account = TestAccount {
            id: Uuid::new_v4(),
            asset_code: "XYZ".to_string(),
            asset_scale: 9,
            ilp_address: destination_address.clone(),
            max_packet_amount: Some(10),
        };
        let store = TestStore {
            route: (destination_address.to_string(), account),
        };

        #[derive(Clone)]
        struct CounterService {
            pub num_requests_in_flight: Arc<AtomicUsize>,
        }

        impl CounterService {
            pub fn new(num_requests_in_flight: Arc<AtomicUsize>) -> Self {
                CounterService {
                    num_requests_in_flight,
                }
            }
        }

        #[async_trait]
        impl<A> IncomingService<A> for CounterService
        where
            A: Account + 'static,
        {
            async fn handle_request(&mut self, _: IncomingRequest<A>) -> IlpResult {
                self.num_requests_in_flight.fetch_add(1, Ordering::Relaxed);

                // Wait for 100ms while all requests are received, then reject with final error to terminate stream
                timeout(
                    Duration::from_millis(100),
                    futures::future::pending::<IlpResult>(),
                )
                .await
                .unwrap_or_else(|_| {
                    Err(RejectBuilder {
                        code: IlpErrorCode::F00_BAD_REQUEST,
                        message: b"some final error",
                        triggered_by: Some(&EXAMPLE_CONNECTOR),
                        data: &[],
                    }
                    .build())
                })
            }
        }

        let num_requests_in_flight = Arc::new(AtomicUsize::new(0));
        let forever = CounterService::new(num_requests_in_flight.clone());

        let result = send_money(
            IldcpService::new(MaxPacketAmountService::new(store, forever)),
            &TestAccount {
                id: Uuid::new_v4(),
                asset_code: "XYZ".to_string(),
                asset_scale: 9,
                ilp_address: destination_address.clone(),
                max_packet_amount: Some(10), // Requires at least 5 packets
            },
            destination_address.clone(),
            &[0; 32][..],
            50,
            TestRateStore {},
        )
        .await;

        assert!(result.is_err());
        assert_eq!(num_requests_in_flight.load(Ordering::Relaxed), 5);
    }

    // TODO Add other min destination amount tests

    // TODO Add 2 exchange rate tests
    // (1) If middleware takes out too much slippage... payment fails
    // (2) If middleware is acceptable slippage, cross-currency payment succeeds
}
