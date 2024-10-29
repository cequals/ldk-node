// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use crate::types::{DynStore, Sweeper, Wallet};

use crate::{
	hex_utils, BumpTransactionEventHandler, ChannelManager, Config, Error, Graph, PeerInfo,
	PeerStore, UserChannelId,
};

use crate::connection::ConnectionManager;
use crate::fee_estimator::ConfirmationTarget;

use crate::payment::store::{
	PaymentDetails, PaymentDetailsUpdate, PaymentDirection, PaymentKind, PaymentStatus,
	PaymentStore,
};

use crate::io::{
	EVENT_QUEUE_PERSISTENCE_KEY, EVENT_QUEUE_PERSISTENCE_PRIMARY_NAMESPACE,
	EVENT_QUEUE_PERSISTENCE_SECONDARY_NAMESPACE,
};
use crate::logger::{log_debug, log_error, log_info, Logger};

use lightning::events::bump_transaction::BumpTransactionEvent;
use lightning::events::{ClosureReason, PaymentPurpose, ReplayEvent};
use lightning::events::{Event as LdkEvent, PaymentFailureReason};
use lightning::impl_writeable_tlv_based_enum;
use lightning::ln::channelmanager::PaymentId;
use lightning::ln::types::ChannelId;
use lightning::ln::PaymentHash;
use lightning::routing::gossip::NodeId;
use lightning::util::errors::APIError;
use lightning::util::ser::{Readable, ReadableArgs, Writeable, Writer};

use lightning_liquidity::lsps2::utils::compute_opening_fee;

use bitcoin::blockdata::locktime::absolute::LockTime;
use bitcoin::secp256k1::PublicKey;
use bitcoin::{Amount, OutPoint};

use rand::{thread_rng, Rng};

use core::future::Future;
use core::task::{Poll, Waker};
use std::collections::VecDeque;
use std::ops::Deref;
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::time::Duration;

/// An event emitted by [`Node`], which should be handled by the user.
///
/// [`Node`]: [`crate::Node`]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
	/// A sent payment was successful.
	PaymentSuccessful {
		/// A local identifier used to track the payment.
		///
		/// Will only be `None` for events serialized with LDK Node v0.2.1 or prior.
		payment_id: Option<PaymentId>,
		/// The hash of the payment.
		payment_hash: PaymentHash,
		/// The total fee which was spent at intermediate hops in this payment.
		fee_paid_msat: Option<u64>,
	},
	/// A sent payment has failed.
	PaymentFailed {
		/// A local identifier used to track the payment.
		///
		/// Will only be `None` for events serialized with LDK Node v0.2.1 or prior.
		payment_id: Option<PaymentId>,
		/// The hash of the payment.
		///
		/// This will be `None` if the payment failed before receiving an invoice when paying a
		/// BOLT12 [`Offer`].
		///
		/// [`Offer`]: lightning::offers::offer::Offer
		payment_hash: Option<PaymentHash>,
		/// The reason why the payment failed.
		///
		/// This will be `None` for events serialized by LDK Node v0.2.1 and prior.
		reason: Option<PaymentFailureReason>,
	},
	/// A payment has been received.
	PaymentReceived {
		/// A local identifier used to track the payment.
		///
		/// Will only be `None` for events serialized with LDK Node v0.2.1 or prior.
		payment_id: Option<PaymentId>,
		/// The hash of the payment.
		payment_hash: PaymentHash,
		/// The value, in thousandths of a satoshi, that has been received.
		amount_msat: u64,
	},
	/// A payment for a previously-registered payment hash has been received.
	///
	/// This needs to be manually claimed by supplying the correct preimage to [`claim_for_hash`].
	///
	/// If the the provided parameters don't match the expectations or the preimage can't be
	/// retrieved in time, should be failed-back via [`fail_for_hash`].
	///
	/// Note claiming will necessarily fail after the `claim_deadline` has been reached.
	///
	/// [`claim_for_hash`]: crate::payment::Bolt11Payment::claim_for_hash
	/// [`fail_for_hash`]: crate::payment::Bolt11Payment::fail_for_hash
	PaymentClaimable {
		/// A local identifier used to track the payment.
		payment_id: PaymentId,
		/// The hash of the payment.
		payment_hash: PaymentHash,
		/// The value, in thousandths of a satoshi, that is claimable.
		claimable_amount_msat: u64,
		/// The block height at which this payment will be failed back and will no longer be
		/// eligible for claiming.
		claim_deadline: Option<u32>,
	},
	/// A channel has been created and is pending confirmation on-chain.
	ChannelPending {
		/// The `channel_id` of the channel.
		channel_id: ChannelId,
		/// The `user_channel_id` of the channel.
		user_channel_id: UserChannelId,
		/// The `temporary_channel_id` this channel used to be known by during channel establishment.
		former_temporary_channel_id: ChannelId,
		/// The `node_id` of the channel counterparty.
		counterparty_node_id: PublicKey,
		/// The outpoint of the channel's funding transaction.
		funding_txo: OutPoint,
	},
	/// A channel is ready to be used.
	ChannelReady {
		/// The `channel_id` of the channel.
		channel_id: ChannelId,
		/// The `user_channel_id` of the channel.
		user_channel_id: UserChannelId,
		/// The `node_id` of the channel counterparty.
		///
		/// This will be `None` for events serialized by LDK Node v0.1.0 and prior.
		counterparty_node_id: Option<PublicKey>,
	},
	/// A channel has been closed.
	ChannelClosed {
		/// The `channel_id` of the channel.
		channel_id: ChannelId,
		/// The `user_channel_id` of the channel.
		user_channel_id: UserChannelId,
		/// The `node_id` of the channel counterparty.
		///
		/// This will be `None` for events serialized by LDK Node v0.1.0 and prior.
		counterparty_node_id: Option<PublicKey>,
		/// This will be `None` for events serialized by LDK Node v0.2.1 and prior.
		reason: Option<ClosureReason>,
	},
	/// Indicates that a probe payment we sent returned successful, i.e., only failed at the destination.
	ProbeSuccessful {
		/// A local identifier used to track the payment.
		payment_id: PaymentId,
		/// The hash of the payment.
		payment_hash: PaymentHash,
		// The path that the probe took.
		// path: Path,
	},
	/// Indicates that a probe payment we sent failed at an intermediary node on the path.
	ProbeFailed {
		/// A local identifier used to track the payment.
		payment_id: PaymentId,
		/// The hash of the payment.
		payment_hash: PaymentHash,
		// The payment path that failed.
		// path: Path,
		/// The channel responsible for the failed probe.
		///
		/// Note that for route hints or for the first hop in a path this may be an SCID alias and
		/// may not refer to a channel in the public network graph. These aliases may also collide
		/// with channels in the public network graph.
		short_channel_id: Option<u64>,
	},
}

// TODO (amackillop): Get the path field from the Probe* events working with this.
impl_writeable_tlv_based_enum!(Event,
	(0, PaymentSuccessful) => {
		(0, payment_hash, required),
		(1, fee_paid_msat, option),
		(3, payment_id, option),
	},
	(1, PaymentFailed) => {
		(0, payment_hash, option),
		(1, reason, upgradable_option),
		(3, payment_id, option),
	},
	(2, PaymentReceived) => {
		(0, payment_hash, required),
		(1, payment_id, option),
		(2, amount_msat, required),
	},
	(3, ChannelReady) => {
		(0, channel_id, required),
		(1, counterparty_node_id, option),
		(2, user_channel_id, required),
	},
	(4, ChannelPending) => {
		(0, channel_id, required),
		(2, user_channel_id, required),
		(4, former_temporary_channel_id, required),
		(6, counterparty_node_id, required),
		(8, funding_txo, required),
	},
	(5, ChannelClosed) => {
		(0, channel_id, required),
		(1, counterparty_node_id, option),
		(2, user_channel_id, required),
		(3, reason, upgradable_option),
	},
	(6, PaymentClaimable) => {
		(0, payment_hash, required),
		(2, payment_id, required),
		(4, claimable_amount_msat, required),
		(6, claim_deadline, option),
	},
	(7, ProbeSuccessful) => {
		(0, payment_id, required),
		(1, payment_hash, required),
		// (2, path.hops, required_vec),
		// (3, path.blinded_tail, option),
	},
	(8, ProbeFailed) => {
		(0, payment_id, required),
		(1, payment_hash, required),
		// (2, path.hops, required_vec),
		(3, short_channel_id, option),
	},
);

pub struct EventQueue<L: Deref>
where
	L::Target: Logger,
{
	queue: Arc<Mutex<VecDeque<Event>>>,
	waker: Arc<Mutex<Option<Waker>>>,
	notifier: Condvar,
	kv_store: Arc<DynStore>,
	logger: L,
}

impl<L: Deref> EventQueue<L>
where
	L::Target: Logger,
{
	pub(crate) fn new(kv_store: Arc<DynStore>, logger: L) -> Self {
		let queue = Arc::new(Mutex::new(VecDeque::new()));
		let waker = Arc::new(Mutex::new(None));
		let notifier = Condvar::new();
		Self { queue, waker, notifier, kv_store, logger }
	}

	pub(crate) fn add_event(&self, event: Event) -> Result<(), Error> {
		{
			let mut locked_queue = self.queue.lock().unwrap();
			locked_queue.push_back(event);
			self.persist_queue(&locked_queue)?;
		}

		self.notifier.notify_one();

		if let Some(waker) = self.waker.lock().unwrap().take() {
			waker.wake();
		}
		Ok(())
	}

	pub(crate) fn next_event(&self) -> Option<Event> {
		let locked_queue = self.queue.lock().unwrap();
		locked_queue.front().cloned()
	}

	pub(crate) async fn next_event_async(&self) -> Event {
		EventFuture { event_queue: Arc::clone(&self.queue), waker: Arc::clone(&self.waker) }.await
	}

	pub(crate) fn wait_next_event(&self) -> Event {
		let locked_queue =
			self.notifier.wait_while(self.queue.lock().unwrap(), |queue| queue.is_empty()).unwrap();
		locked_queue.front().unwrap().clone()
	}

	pub(crate) fn event_handled(&self) -> Result<(), Error> {
		{
			let mut locked_queue = self.queue.lock().unwrap();
			locked_queue.pop_front();
			self.persist_queue(&locked_queue)?;
		}
		self.notifier.notify_one();

		if let Some(waker) = self.waker.lock().unwrap().take() {
			waker.wake();
		}
		Ok(())
	}

	fn persist_queue(&self, locked_queue: &VecDeque<Event>) -> Result<(), Error> {
		let data = EventQueueSerWrapper(locked_queue).encode();
		self.kv_store
			.write(
				EVENT_QUEUE_PERSISTENCE_PRIMARY_NAMESPACE,
				EVENT_QUEUE_PERSISTENCE_SECONDARY_NAMESPACE,
				EVENT_QUEUE_PERSISTENCE_KEY,
				&data,
			)
			.map_err(|e| {
				log_error!(
					self.logger,
					"Write for key {}/{}/{} failed due to: {}",
					EVENT_QUEUE_PERSISTENCE_PRIMARY_NAMESPACE,
					EVENT_QUEUE_PERSISTENCE_SECONDARY_NAMESPACE,
					EVENT_QUEUE_PERSISTENCE_KEY,
					e
				);
				Error::PersistenceFailed
			})?;
		Ok(())
	}
}

impl<L: Deref> ReadableArgs<(Arc<DynStore>, L)> for EventQueue<L>
where
	L::Target: Logger,
{
	#[inline]
	fn read<R: lightning::io::Read>(
		reader: &mut R, args: (Arc<DynStore>, L),
	) -> Result<Self, lightning::ln::msgs::DecodeError> {
		let (kv_store, logger) = args;
		let read_queue: EventQueueDeserWrapper = Readable::read(reader)?;
		let queue = Arc::new(Mutex::new(read_queue.0));
		let waker = Arc::new(Mutex::new(None));
		let notifier = Condvar::new();
		Ok(Self { queue, waker, notifier, kv_store, logger })
	}
}

struct EventQueueDeserWrapper(VecDeque<Event>);

impl Readable for EventQueueDeserWrapper {
	fn read<R: lightning::io::Read>(
		reader: &mut R,
	) -> Result<Self, lightning::ln::msgs::DecodeError> {
		let len: u16 = Readable::read(reader)?;
		let mut queue = VecDeque::with_capacity(len as usize);
		for _ in 0..len {
			queue.push_back(Readable::read(reader)?);
		}
		Ok(Self(queue))
	}
}

struct EventQueueSerWrapper<'a>(&'a VecDeque<Event>);

impl Writeable for EventQueueSerWrapper<'_> {
	fn write<W: Writer>(&self, writer: &mut W) -> Result<(), lightning::io::Error> {
		(self.0.len() as u16).write(writer)?;
		for e in self.0.iter() {
			e.write(writer)?;
		}
		Ok(())
	}
}

struct EventFuture {
	event_queue: Arc<Mutex<VecDeque<Event>>>,
	waker: Arc<Mutex<Option<Waker>>>,
}

impl Future for EventFuture {
	type Output = Event;

	fn poll(
		self: core::pin::Pin<&mut Self>, cx: &mut core::task::Context<'_>,
	) -> core::task::Poll<Self::Output> {
		if let Some(event) = self.event_queue.lock().unwrap().front() {
			Poll::Ready(event.clone())
		} else {
			*self.waker.lock().unwrap() = Some(cx.waker().clone());
			Poll::Pending
		}
	}
}

pub(crate) struct EventHandler<L: Deref + Clone + Sync + Send + 'static>
where
	L::Target: Logger,
{
	event_queue: Arc<EventQueue<L>>,
	wallet: Arc<Wallet>,
	bump_tx_event_handler: Arc<BumpTransactionEventHandler>,
	channel_manager: Arc<ChannelManager>,
	connection_manager: Arc<ConnectionManager<L>>,
	output_sweeper: Arc<Sweeper>,
	network_graph: Arc<Graph>,
	payment_store: Arc<PaymentStore<L>>,
	peer_store: Arc<PeerStore<L>>,
	runtime: Arc<RwLock<Option<Arc<tokio::runtime::Runtime>>>>,
	logger: L,
	config: Arc<Config>,
}

impl<L: Deref + Clone + Sync + Send + 'static> EventHandler<L>
where
	L::Target: Logger,
{
	pub fn new(
		event_queue: Arc<EventQueue<L>>, wallet: Arc<Wallet>,
		bump_tx_event_handler: Arc<BumpTransactionEventHandler>,
		channel_manager: Arc<ChannelManager>, connection_manager: Arc<ConnectionManager<L>>,
		output_sweeper: Arc<Sweeper>, network_graph: Arc<Graph>,
		payment_store: Arc<PaymentStore<L>>, peer_store: Arc<PeerStore<L>>,
		runtime: Arc<RwLock<Option<Arc<tokio::runtime::Runtime>>>>, logger: L, config: Arc<Config>,
	) -> Self {
		Self {
			event_queue,
			wallet,
			bump_tx_event_handler,
			channel_manager,
			connection_manager,
			output_sweeper,
			network_graph,
			payment_store,
			peer_store,
			logger,
			runtime,
			config,
		}
	}

	pub async fn handle_event(&self, event: LdkEvent) -> Result<(), ReplayEvent> {
		match event {
			LdkEvent::FundingGenerationReady {
				temporary_channel_id,
				counterparty_node_id,
				channel_value_satoshis,
				output_script,
				..
			} => {
				// Construct the raw transaction with the output that is paid the amount of the
				// channel.
				let confirmation_target = ConfirmationTarget::ChannelFunding;

				// We set nLockTime to the current height to discourage fee sniping.
				let cur_height = self.channel_manager.current_best_block().height;
				let locktime = LockTime::from_height(cur_height).unwrap_or(LockTime::ZERO);

				// Sign the final funding transaction and broadcast it.
				let channel_amount = Amount::from_sat(channel_value_satoshis);
				match self.wallet.create_funding_transaction(
					output_script,
					channel_amount,
					confirmation_target,
					locktime,
				) {
					Ok(final_tx) => {
						// Give the funding transaction back to LDK for opening the channel.
						match self.channel_manager.funding_transaction_generated(
							temporary_channel_id,
							counterparty_node_id,
							final_tx,
						) {
							Ok(()) => {},
							Err(APIError::APIMisuseError { err }) => {
								log_error!(self.logger, "Panicking due to APIMisuseError: {}", err);
								panic!("APIMisuseError: {}", err);
							},
							Err(APIError::ChannelUnavailable { err }) => {
								log_error!(
									self.logger,
									"Failed to process funding transaction as channel went away before we could fund it: {}",
									err
								)
							},
							Err(err) => {
								log_error!(
									self.logger,
									"Failed to process funding transaction: {:?}",
									err
								)
							},
						}
					},
					Err(err) => {
						log_error!(self.logger, "Failed to create funding transaction: {}", err);
						self.channel_manager
							.force_close_without_broadcasting_txn(
								&temporary_channel_id,
								&counterparty_node_id,
								"Failed to create funding transaction".to_string(),
							)
							.unwrap_or_else(|e| {
								log_error!(self.logger, "Failed to force close channel after funding generation failed: {:?}", e);
								panic!(
									"Failed to force close channel after funding generation failed"
								);
							});
					},
				}
			},
			LdkEvent::FundingTxBroadcastSafe { .. } => {
				debug_assert!(false, "We currently only support safe funding, so this event should never be emitted.");
			},
			LdkEvent::PaymentClaimable {
				payment_hash,
				purpose,
				amount_msat,
				receiver_node_id: _,
				via_channel_id: _,
				via_user_channel_id: _,
				claim_deadline,
				onion_fields: _,
				counterparty_skimmed_fee_msat,
			} => {
				let payment_id = PaymentId(payment_hash.0);
				if let Some(info) = self.payment_store.get(&payment_id) {
					if info.direction == PaymentDirection::Outbound {
						log_info!(
							self.logger,
							"Refused inbound payment with ID {}: circular payments are unsupported.",
							payment_id
						);
						self.channel_manager.fail_htlc_backwards(&payment_hash);

						let update = PaymentDetailsUpdate {
							status: Some(PaymentStatus::Failed),
							..PaymentDetailsUpdate::new(payment_id)
						};
						match self.payment_store.update(&update) {
							Ok(_) => return Ok(()),
							Err(e) => {
								log_error!(self.logger, "Failed to access payment store: {}", e);
								return Err(ReplayEvent());
							},
						};
					}

					if info.status == PaymentStatus::Succeeded
						|| matches!(info.kind, PaymentKind::Spontaneous { .. })
					{
						log_info!(
							self.logger,
							"Refused duplicate inbound payment from payment hash {} of {}msat",
							hex_utils::to_string(&payment_hash.0),
							amount_msat,
						);
						self.channel_manager.fail_htlc_backwards(&payment_hash);

						let update = PaymentDetailsUpdate {
							status: Some(PaymentStatus::Failed),
							..PaymentDetailsUpdate::new(payment_id)
						};
						match self.payment_store.update(&update) {
							Ok(_) => return Ok(()),
							Err(e) => {
								log_error!(self.logger, "Failed to access payment store: {}", e);
								return Err(ReplayEvent());
							},
						};
					}

					let max_total_opening_fee_msat = match info.kind {
						PaymentKind::Bolt11Jit { lsp_fee_limits, .. } => {
							lsp_fee_limits
								.max_total_opening_fee_msat
								.or_else(|| {
									lsp_fee_limits.max_proportional_opening_fee_ppm_msat.and_then(
										|max_prop_fee| {
											// If it's a variable amount payment, compute the actual fee.
											compute_opening_fee(amount_msat, 0, max_prop_fee)
										},
									)
								})
								.unwrap_or(0)
						},
						_ => 0,
					};

					if counterparty_skimmed_fee_msat > max_total_opening_fee_msat {
						log_info!(
							self.logger,
							"Refusing inbound payment with hash {} as the counterparty-withheld fee of {}msat exceeds our limit of {}msat",
							hex_utils::to_string(&payment_hash.0),
							counterparty_skimmed_fee_msat,
							max_total_opening_fee_msat,
						);
						self.channel_manager.fail_htlc_backwards(&payment_hash);

						let update = PaymentDetailsUpdate {
							hash: Some(Some(payment_hash)),
							status: Some(PaymentStatus::Failed),
							..PaymentDetailsUpdate::new(payment_id)
						};
						match self.payment_store.update(&update) {
							Ok(_) => return Ok(()),
							Err(e) => {
								log_error!(self.logger, "Failed to access payment store: {}", e);
								return Err(ReplayEvent());
							},
						};
					}

					// If this is known by the store but ChannelManager doesn't know the preimage,
					// the payment has been registered via `_for_hash` variants and needs to be manually claimed via
					// user interaction.
					match info.kind {
						PaymentKind::Bolt11 { preimage, .. } => {
							if purpose.preimage().is_none() {
								debug_assert!(
									preimage.is_none(),
									"We would have registered the preimage if we knew"
								);

								let event = Event::PaymentClaimable {
									payment_id,
									payment_hash,
									claimable_amount_msat: amount_msat,
									claim_deadline,
								};
								match self.event_queue.add_event(event) {
									Ok(_) => return Ok(()),
									Err(e) => {
										log_error!(
											self.logger,
											"Failed to push to event queue: {}",
											e
										);
										return Err(ReplayEvent());
									},
								};
							}
						},
						_ => {},
					}
				}

				log_info!(
					self.logger,
					"Received payment from payment hash {} of {}msat",
					hex_utils::to_string(&payment_hash.0),
					amount_msat,
				);
				let payment_preimage = match purpose {
					PaymentPurpose::Bolt11InvoicePayment { payment_preimage, .. } => {
						payment_preimage
					},
					PaymentPurpose::Bolt12OfferPayment {
						payment_preimage,
						payment_secret,
						payment_context,
						..
					} => {
						let payer_note = payment_context.invoice_request.payer_note_truncated;
						let offer_id = payment_context.offer_id;
						let quantity = payment_context.invoice_request.quantity;
						let kind = PaymentKind::Bolt12Offer {
							hash: Some(payment_hash),
							preimage: payment_preimage,
							secret: Some(payment_secret),
							offer_id,
							payer_note,
							quantity,
						};

						let payment = PaymentDetails::new(
							payment_id,
							kind,
							Some(amount_msat),
							PaymentDirection::Inbound,
							PaymentStatus::Pending,
						);

						match self.payment_store.insert(payment) {
							Ok(false) => (),
							Ok(true) => {
								log_error!(
									self.logger,
									"Bolt12OfferPayment with ID {} was previously known",
									payment_id,
								);
								debug_assert!(false);
							},
							Err(e) => {
								log_error!(
									self.logger,
									"Failed to insert payment with ID {}: {}",
									payment_id,
									e
								);
								debug_assert!(false);
							},
						}
						payment_preimage
					},
					PaymentPurpose::Bolt12RefundPayment { payment_preimage, .. } => {
						payment_preimage
					},
					PaymentPurpose::SpontaneousPayment(preimage) => {
						// Since it's spontaneous, we insert it now into our store.
						let kind = PaymentKind::Spontaneous {
							hash: payment_hash,
							preimage: Some(preimage),
						};

						let payment = PaymentDetails::new(
							payment_id,
							kind,
							Some(amount_msat),
							PaymentDirection::Inbound,
							PaymentStatus::Pending,
						);

						match self.payment_store.insert(payment) {
							Ok(false) => (),
							Ok(true) => {
								log_error!(
									self.logger,
									"Spontaneous payment with ID {} was previously known",
									payment_id,
								);
								debug_assert!(false);
							},
							Err(e) => {
								log_error!(
									self.logger,
									"Failed to insert payment with ID {}: {}",
									payment_id,
									e
								);
								debug_assert!(false);
							},
						}

						Some(preimage)
					},
				};

				if let Some(preimage) = payment_preimage {
					self.channel_manager.claim_funds(preimage);
				} else {
					log_error!(
						self.logger,
						"Failed to claim payment with ID {}: preimage unknown.",
						payment_id,
					);
					self.channel_manager.fail_htlc_backwards(&payment_hash);

					let update = PaymentDetailsUpdate {
						hash: Some(Some(payment_hash)),
						status: Some(PaymentStatus::Failed),
						..PaymentDetailsUpdate::new(payment_id)
					};
					match self.payment_store.update(&update) {
						Ok(_) => return Ok(()),
						Err(e) => {
							log_error!(self.logger, "Failed to access payment store: {}", e);
							return Err(ReplayEvent());
						},
					};
				}
			},
			LdkEvent::PaymentClaimed {
				payment_hash,
				purpose,
				amount_msat,
				receiver_node_id: _,
				htlcs: _,
				sender_intended_total_msat: _,
				onion_fields: _,
			} => {
				let payment_id = PaymentId(payment_hash.0);
				log_info!(
					self.logger,
					"Claimed payment with ID {} from payment hash {} of {}msat.",
					payment_id,
					hex_utils::to_string(&payment_hash.0),
					amount_msat,
				);

				let update = match purpose {
					PaymentPurpose::Bolt11InvoicePayment {
						payment_preimage,
						payment_secret,
						..
					} => PaymentDetailsUpdate {
						preimage: Some(payment_preimage),
						secret: Some(Some(payment_secret)),
						amount_msat: Some(Some(amount_msat)),
						status: Some(PaymentStatus::Succeeded),
						..PaymentDetailsUpdate::new(payment_id)
					},
					PaymentPurpose::Bolt12OfferPayment {
						payment_preimage, payment_secret, ..
					} => PaymentDetailsUpdate {
						preimage: Some(payment_preimage),
						secret: Some(Some(payment_secret)),
						amount_msat: Some(Some(amount_msat)),
						status: Some(PaymentStatus::Succeeded),
						..PaymentDetailsUpdate::new(payment_id)
					},
					PaymentPurpose::Bolt12RefundPayment {
						payment_preimage,
						payment_secret,
						..
					} => PaymentDetailsUpdate {
						preimage: Some(payment_preimage),
						secret: Some(Some(payment_secret)),
						amount_msat: Some(Some(amount_msat)),
						status: Some(PaymentStatus::Succeeded),
						..PaymentDetailsUpdate::new(payment_id)
					},
					PaymentPurpose::SpontaneousPayment(preimage) => PaymentDetailsUpdate {
						preimage: Some(Some(preimage)),
						amount_msat: Some(Some(amount_msat)),
						status: Some(PaymentStatus::Succeeded),
						..PaymentDetailsUpdate::new(payment_id)
					},
				};

				match self.payment_store.update(&update) {
					Ok(true) => (),
					Ok(false) => {
						log_error!(
							self.logger,
							"Payment with ID {} couldn't be found in store",
							payment_id,
						);
						debug_assert!(false);
					},
					Err(e) => {
						log_error!(
							self.logger,
							"Failed to update payment with ID {}: {}",
							payment_id,
							e
						);
						return Err(ReplayEvent());
					},
				}

				let event = Event::PaymentReceived {
					payment_id: Some(payment_id),
					payment_hash,
					amount_msat,
				};
				match self.event_queue.add_event(event) {
					Ok(_) => return Ok(()),
					Err(e) => {
						log_error!(self.logger, "Failed to push to event queue: {}", e);
						return Err(ReplayEvent());
					},
				};
			},
			LdkEvent::PaymentSent {
				payment_id,
				payment_preimage,
				payment_hash,
				fee_paid_msat,
				..
			} => {
				let payment_id = if let Some(id) = payment_id {
					id
				} else {
					debug_assert!(false, "payment_id should always be set.");
					return Ok(());
				};

				let update = PaymentDetailsUpdate {
					hash: Some(Some(payment_hash)),
					preimage: Some(Some(payment_preimage)),
					status: Some(PaymentStatus::Succeeded),
					..PaymentDetailsUpdate::new(payment_id)
				};

				match self.payment_store.update(&update) {
					Ok(_) => {},
					Err(e) => {
						log_error!(self.logger, "Failed to access payment store: {}", e);
						return Err(ReplayEvent());
					},
				};

				self.payment_store.get(&payment_id).map(|payment| {
					log_info!(
						self.logger,
						"Successfully sent payment of {}msat{} from \
						payment hash {:?} with preimage {:?}",
						payment.amount_msat.unwrap(),
						if let Some(fee) = fee_paid_msat {
							format!(" (fee {} msat)", fee)
						} else {
							"".to_string()
						},
						hex_utils::to_string(&payment_hash.0),
						hex_utils::to_string(&payment_preimage.0)
					);
				});
				let event = Event::PaymentSuccessful {
					payment_id: Some(payment_id),
					payment_hash,
					fee_paid_msat,
				};

				match self.event_queue.add_event(event) {
					Ok(_) => return Ok(()),
					Err(e) => {
						log_error!(self.logger, "Failed to push to event queue: {}", e);
						return Err(ReplayEvent());
					},
				};
			},
			LdkEvent::PaymentFailed { payment_id, payment_hash, reason, .. } => {
				log_info!(
					self.logger,
					"Failed to send payment with ID {} due to {:?}.",
					payment_id,
					reason
				);

				let update = PaymentDetailsUpdate {
					hash: Some(payment_hash),
					status: Some(PaymentStatus::Failed),
					..PaymentDetailsUpdate::new(payment_id)
				};
				match self.payment_store.update(&update) {
					Ok(_) => {},
					Err(e) => {
						log_error!(self.logger, "Failed to access payment store: {}", e);
						return Err(ReplayEvent());
					},
				};

				let event =
					Event::PaymentFailed { payment_id: Some(payment_id), payment_hash, reason };
				match self.event_queue.add_event(event) {
					Ok(_) => return Ok(()),
					Err(e) => {
						log_error!(self.logger, "Failed to push to event queue: {}", e);
						return Err(ReplayEvent());
					},
				};
			},

			LdkEvent::PaymentPathSuccessful { .. } => {},
			LdkEvent::PaymentPathFailed { .. } => {},
			LdkEvent::ProbeSuccessful { payment_id, payment_hash, .. } => {
				self.event_queue
					.add_event(Event::ProbeSuccessful { payment_id, payment_hash })
					.unwrap_or_else(|e| {
						log_error!(self.logger, "Failed to push to event queue: {}", e);
						panic!("Failed to push to event queue");
					});
			},
			LdkEvent::ProbeFailed { payment_id, payment_hash, short_channel_id, .. } => {
				self.event_queue
					.add_event(Event::ProbeFailed { payment_id, payment_hash, short_channel_id })
					.unwrap_or_else(|e| {
						log_error!(self.logger, "Failed to push to event queue: {}", e);
						panic!("Failed to push to event queue");
					});
			},
			LdkEvent::HTLCHandlingFailed { .. } => {},
			LdkEvent::PendingHTLCsForwardable { time_forwardable } => {
				let forwarding_channel_manager = self.channel_manager.clone();
				let min = time_forwardable.as_millis() as u64;

				let runtime_lock = self.runtime.read().unwrap();
				debug_assert!(runtime_lock.is_some());

				if let Some(runtime) = runtime_lock.as_ref() {
					runtime.spawn(async move {
						let millis_to_sleep = thread_rng().gen_range(min..min * 5) as u64;
						tokio::time::sleep(Duration::from_millis(millis_to_sleep)).await;

						forwarding_channel_manager.process_pending_htlc_forwards();
					});
				}
			},
			LdkEvent::SpendableOutputs { outputs, channel_id } => {
				match self.output_sweeper.track_spendable_outputs(outputs, channel_id, true, None) {
					Ok(_) => return Ok(()),
					Err(_) => {
						log_error!(self.logger, "Failed to track spendable outputs");
						return Err(ReplayEvent());
					},
				};
			},
			LdkEvent::OpenChannelRequest {
				temporary_channel_id,
				counterparty_node_id,
				funding_satoshis,
				channel_type,
				push_msat: _,
				is_announced: _,
				params: _,
			} => {
				let anchor_channel = channel_type.requires_anchors_zero_fee_htlc_tx();

				// TODO: We should use `is_announced` flag above and reject announced channels if
				// we're not a forwading node, once we add a 'forwarding mode' based on listening
				// address / node alias being set.

				if anchor_channel {
					if let Some(anchor_channels_config) =
						self.config.anchor_channels_config.as_ref()
					{
						let cur_anchor_reserve_sats = crate::total_anchor_channels_reserve_sats(
							&self.channel_manager,
							&self.config,
						);
						let spendable_amount_sats = self
							.wallet
							.get_spendable_amount_sats(cur_anchor_reserve_sats)
							.unwrap_or(0);

						let required_amount_sats = if anchor_channels_config
							.trusted_peers_no_reserve
							.contains(&counterparty_node_id)
						{
							0
						} else {
							anchor_channels_config.per_channel_reserve_sats
						};

						if spendable_amount_sats < required_amount_sats {
							log_error!(
								self.logger,
								"Rejecting inbound Anchor channel from peer {} due to insufficient available on-chain reserves.",
								counterparty_node_id,
							);
							self.channel_manager
								.force_close_without_broadcasting_txn(
									&temporary_channel_id,
									&counterparty_node_id,
									"Channel request rejected".to_string(),
								)
								.unwrap_or_else(|e| {
									log_error!(self.logger, "Failed to reject channel: {:?}", e)
								});
							return Ok(());
						}
					} else {
						log_error!(
							self.logger,
							"Rejecting inbound channel from peer {} due to Anchor channels being disabled.",
							counterparty_node_id,
						);
						self.channel_manager
							.force_close_without_broadcasting_txn(
								&temporary_channel_id,
								&counterparty_node_id,
								"Channel request rejected".to_string(),
							)
							.unwrap_or_else(|e| {
								log_error!(self.logger, "Failed to reject channel: {:?}", e)
							});
						return Ok(());
					}
				}

				let user_channel_id: u128 = rand::thread_rng().gen::<u128>();
				let allow_0conf = self.config.trusted_peers_0conf.contains(&counterparty_node_id);
				let res = if allow_0conf {
					self.channel_manager.accept_inbound_channel_from_trusted_peer_0conf(
						&temporary_channel_id,
						&counterparty_node_id,
						user_channel_id,
					)
				} else {
					self.channel_manager.accept_inbound_channel(
						&temporary_channel_id,
						&counterparty_node_id,
						user_channel_id,
					)
				};

				match res {
					Ok(()) => {
						log_info!(
							self.logger,
							"Accepting inbound{}{} channel of {}sats from{} peer {}",
							if allow_0conf { " 0conf" } else { "" },
							if anchor_channel { " Anchor" } else { "" },
							funding_satoshis,
							if allow_0conf { " trusted" } else { "" },
							counterparty_node_id,
						);
					},
					Err(e) => {
						log_error!(
							self.logger,
							"Error while accepting inbound{}{} channel from{} peer {}: {:?}",
							if allow_0conf { " 0conf" } else { "" },
							if anchor_channel { " Anchor" } else { "" },
							counterparty_node_id,
							if allow_0conf { " trusted" } else { "" },
							e,
						);
					},
				}
			},
			LdkEvent::PaymentForwarded {
				prev_channel_id,
				next_channel_id,
				total_fee_earned_msat,
				claim_from_onchain_tx,
				outbound_amount_forwarded_msat,
				..
			} => {
				let read_only_network_graph = self.network_graph.read_only();
				let nodes = read_only_network_graph.nodes();
				let channels = self.channel_manager.list_channels();

				let node_str = |channel_id: &Option<ChannelId>| {
					channel_id
						.and_then(|channel_id| channels.iter().find(|c| c.channel_id == channel_id))
						.and_then(|channel| {
							nodes.get(&NodeId::from_pubkey(&channel.counterparty.node_id))
						})
						.map_or("private_node".to_string(), |node| {
							node.announcement_info
								.as_ref()
								.map_or("unnamed node".to_string(), |ann| {
									format!("node {}", ann.alias())
								})
						})
				};
				let channel_str = |channel_id: &Option<ChannelId>| {
					channel_id
						.map(|channel_id| format!(" with channel {}", channel_id))
						.unwrap_or_default()
				};
				let from_prev_str = format!(
					" from {}{}",
					node_str(&prev_channel_id),
					channel_str(&prev_channel_id)
				);
				let to_next_str =
					format!(" to {}{}", node_str(&next_channel_id), channel_str(&next_channel_id));

				let fee_earned = total_fee_earned_msat.unwrap_or(0);
				let outbound_amount_forwarded_msat = outbound_amount_forwarded_msat.unwrap_or(0);
				if claim_from_onchain_tx {
					log_info!(
						self.logger,
						"Forwarded payment{}{} of {}msat, earning {}msat in fees from claiming onchain.",
						from_prev_str,
						to_next_str,
						outbound_amount_forwarded_msat,
						fee_earned,
					);
				} else {
					log_info!(
						self.logger,
						"Forwarded payment{}{} of {}msat, earning {}msat in fees.",
						from_prev_str,
						to_next_str,
						outbound_amount_forwarded_msat,
						fee_earned,
					);
				}
			},
			LdkEvent::ChannelPending {
				channel_id,
				user_channel_id,
				former_temporary_channel_id,
				counterparty_node_id,
				funding_txo,
				..
			} => {
				log_info!(
					self.logger,
					"New channel {} with counterparty {} has been created and is pending confirmation on chain.",
					channel_id,
					counterparty_node_id,
				);

				let event = Event::ChannelPending {
					channel_id,
					user_channel_id: UserChannelId(user_channel_id),
					former_temporary_channel_id: former_temporary_channel_id.unwrap(),
					counterparty_node_id,
					funding_txo,
				};
				match self.event_queue.add_event(event) {
					Ok(_) => {},
					Err(e) => {
						log_error!(self.logger, "Failed to push to event queue: {}", e);
						return Err(ReplayEvent());
					},
				};

				let network_graph = self.network_graph.read_only();
				let channels =
					self.channel_manager.list_channels_with_counterparty(&counterparty_node_id);
				if let Some(pending_channel) =
					channels.into_iter().find(|c| c.channel_id == channel_id)
				{
					if !pending_channel.is_outbound
						&& self.peer_store.get_peer(&counterparty_node_id).is_none()
					{
						if let Some(address) = network_graph
							.nodes()
							.get(&NodeId::from_pubkey(&counterparty_node_id))
							.and_then(|node_info| node_info.announcement_info.as_ref())
							.and_then(|ann_info| ann_info.addresses().first())
						{
							let peer = PeerInfo {
								node_id: counterparty_node_id,
								address: address.clone(),
							};

							self.peer_store.add_peer(peer).unwrap_or_else(|e| {
								log_error!(
									self.logger,
									"Failed to add peer {} to peer store: {}",
									counterparty_node_id,
									e
								);
							});
						}
					}
				}
			},
			LdkEvent::ChannelReady {
				channel_id, user_channel_id, counterparty_node_id, ..
			} => {
				log_info!(
					self.logger,
					"Channel {} with counterparty {} ready to be used.",
					channel_id,
					counterparty_node_id,
				);

				let event = Event::ChannelReady {
					channel_id,
					user_channel_id: UserChannelId(user_channel_id),
					counterparty_node_id: Some(counterparty_node_id),
				};
				match self.event_queue.add_event(event) {
					Ok(_) => {},
					Err(e) => {
						log_error!(self.logger, "Failed to push to event queue: {}", e);
						return Err(ReplayEvent());
					},
				};
			},
			LdkEvent::ChannelClosed {
				channel_id,
				reason,
				user_channel_id,
				counterparty_node_id,
				..
			} => {
				log_info!(self.logger, "Channel {} closed due to: {}", channel_id, reason);

				let event = Event::ChannelClosed {
					channel_id,
					user_channel_id: UserChannelId(user_channel_id),
					counterparty_node_id,
					reason: Some(reason),
				};

				match self.event_queue.add_event(event) {
					Ok(_) => {},
					Err(e) => {
						log_error!(self.logger, "Failed to push to event queue: {}", e);
						return Err(ReplayEvent());
					},
				};
			},
			LdkEvent::DiscardFunding { .. } => {},
			LdkEvent::HTLCIntercepted { .. } => {},
			LdkEvent::InvoiceReceived { .. } => {
				debug_assert!(false, "We currently don't handle BOLT12 invoices manually, so this event should never be emitted.");
			},
			LdkEvent::ConnectionNeeded { node_id, addresses } => {
				let runtime_lock = self.runtime.read().unwrap();
				debug_assert!(runtime_lock.is_some());

				if let Some(runtime) = runtime_lock.as_ref() {
					let spawn_logger = self.logger.clone();
					let spawn_cm = Arc::clone(&self.connection_manager);
					runtime.spawn(async move {
						for addr in &addresses {
							match spawn_cm.connect_peer_if_necessary(node_id, addr.clone()).await {
								Ok(()) => {
									return;
								},
								Err(e) => {
									log_error!(
										spawn_logger,
										"Failed to establish connection to peer {}@{}: {}",
										node_id,
										addr,
										e
									);
								},
							}
						}
					});
				}
			},
			LdkEvent::BumpTransaction(bte) => {
				let (channel_id, counterparty_node_id) = match bte {
					BumpTransactionEvent::ChannelClose {
						ref channel_id,
						ref counterparty_node_id,
						..
					} => (channel_id, counterparty_node_id),
					BumpTransactionEvent::HTLCResolution {
						ref channel_id,
						ref counterparty_node_id,
						..
					} => (channel_id, counterparty_node_id),
				};

				if let Some(anchor_channels_config) = self.config.anchor_channels_config.as_ref() {
					if anchor_channels_config
						.trusted_peers_no_reserve
						.contains(counterparty_node_id)
					{
						log_debug!(self.logger,
							"Ignoring BumpTransactionEvent for channel {} due to trusted counterparty {}",
							channel_id, counterparty_node_id
						);
						return Ok(());
					}
				}

				self.bump_tx_event_handler.handle_event(&bte);
			},
			LdkEvent::OnionMessageIntercepted { .. } => {
				debug_assert!(false, "We currently don't support onion message interception, so this event should never be emitted.");
			},
			LdkEvent::OnionMessagePeerConnected { .. } => {
				debug_assert!(false, "We currently don't support onion message interception, so this event should never be emitted.");
			},
		}
		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use lightning::util::test_utils::{TestLogger, TestStore};
	use std::sync::atomic::{AtomicU16, Ordering};
	use std::time::Duration;

	#[tokio::test]
	async fn event_queue_persistence() {
		let store: Arc<DynStore> = Arc::new(TestStore::new(false));
		let logger = Arc::new(TestLogger::new());
		let event_queue = Arc::new(EventQueue::new(Arc::clone(&store), Arc::clone(&logger)));
		assert_eq!(event_queue.next_event(), None);

		let expected_event = Event::ChannelReady {
			channel_id: ChannelId([23u8; 32]),
			user_channel_id: UserChannelId(2323),
			counterparty_node_id: None,
		};
		event_queue.add_event(expected_event.clone()).unwrap();

		// Check we get the expected event and that it is returned until we mark it handled.
		for _ in 0..5 {
			assert_eq!(event_queue.wait_next_event(), expected_event);
			assert_eq!(event_queue.next_event_async().await, expected_event);
			assert_eq!(event_queue.next_event(), Some(expected_event.clone()));
		}

		// Check we can read back what we persisted.
		let persisted_bytes = store
			.read(
				EVENT_QUEUE_PERSISTENCE_PRIMARY_NAMESPACE,
				EVENT_QUEUE_PERSISTENCE_SECONDARY_NAMESPACE,
				EVENT_QUEUE_PERSISTENCE_KEY,
			)
			.unwrap();
		let deser_event_queue =
			EventQueue::read(&mut &persisted_bytes[..], (Arc::clone(&store), logger)).unwrap();
		assert_eq!(deser_event_queue.wait_next_event(), expected_event);

		event_queue.event_handled().unwrap();
		assert_eq!(event_queue.next_event(), None);
	}

	#[tokio::test]
	async fn event_queue_concurrency() {
		let store: Arc<DynStore> = Arc::new(TestStore::new(false));
		let logger = Arc::new(TestLogger::new());
		let event_queue = Arc::new(EventQueue::new(Arc::clone(&store), Arc::clone(&logger)));
		assert_eq!(event_queue.next_event(), None);

		let expected_event = Event::ChannelReady {
			channel_id: ChannelId([23u8; 32]),
			user_channel_id: UserChannelId(2323),
			counterparty_node_id: None,
		};

		// Check `next_event_async` won't return if the queue is empty and always rather timeout.
		tokio::select! {
			_ = tokio::time::sleep(Duration::from_secs(1)) => {
				// Timeout
			}
			_ = event_queue.next_event_async() => {
				panic!();
			}
		}

		assert_eq!(event_queue.next_event(), None);
		// Check we get the expected number of events when polling/enqueuing concurrently.
		let enqueued_events = AtomicU16::new(0);
		let received_events = AtomicU16::new(0);
		let mut delayed_enqueue = false;

		for _ in 0..25 {
			event_queue.add_event(expected_event.clone()).unwrap();
			enqueued_events.fetch_add(1, Ordering::SeqCst);
		}

		loop {
			tokio::select! {
				_ = tokio::time::sleep(Duration::from_millis(10)), if !delayed_enqueue => {
					event_queue.add_event(expected_event.clone()).unwrap();
					enqueued_events.fetch_add(1, Ordering::SeqCst);
					delayed_enqueue = true;
				}
				e = event_queue.next_event_async() => {
					assert_eq!(e, expected_event);
					event_queue.event_handled().unwrap();
					received_events.fetch_add(1, Ordering::SeqCst);

					event_queue.add_event(expected_event.clone()).unwrap();
					enqueued_events.fetch_add(1, Ordering::SeqCst);
				}
				e = event_queue.next_event_async() => {
					assert_eq!(e, expected_event);
					event_queue.event_handled().unwrap();
					received_events.fetch_add(1, Ordering::SeqCst);
				}
			}

			if delayed_enqueue
				&& received_events.load(Ordering::SeqCst) == enqueued_events.load(Ordering::SeqCst)
			{
				break;
			}
		}
		assert_eq!(event_queue.next_event(), None);

		// Check we operate correctly, even when mixing and matching blocking and async API calls.
		let (tx, mut rx) = tokio::sync::watch::channel(());
		let thread_queue = Arc::clone(&event_queue);
		let thread_event = expected_event.clone();
		std::thread::spawn(move || {
			let e = thread_queue.wait_next_event();
			assert_eq!(e, thread_event);
			thread_queue.event_handled().unwrap();
			tx.send(()).unwrap();
		});

		let thread_queue = Arc::clone(&event_queue);
		let thread_event = expected_event.clone();
		std::thread::spawn(move || {
			// Sleep a bit before we enqueue the events everybody is waiting for.
			std::thread::sleep(Duration::from_millis(20));
			thread_queue.add_event(thread_event.clone()).unwrap();
			thread_queue.add_event(thread_event.clone()).unwrap();
		});

		let e = event_queue.next_event_async().await;
		assert_eq!(e, expected_event.clone());
		event_queue.event_handled().unwrap();

		rx.changed().await.unwrap();
		assert_eq!(event_queue.next_event(), None);
	}
}
