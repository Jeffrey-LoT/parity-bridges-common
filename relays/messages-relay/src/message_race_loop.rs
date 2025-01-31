// Copyright 2019-2020 Parity Technologies (UK) Ltd.
// This file is part of Parity Bridges Common.

// Parity Bridges Common is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Parity Bridges Common is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

//! Loop that is serving single race within message lane. This could be
//! message delivery race, receiving confirmations race or processing
//! confirmations race.
//!
//! The idea of the race is simple - we have `nonce`-s on source and target
//! nodes. We're trying to prove that the source node has this nonce (and
//! associated data - like messages, lane state, etc) to the target node by
//! generating and submitting proof.

use crate::message_lane_loop::ClientState;

use async_trait::async_trait;
use bp_message_lane::MessageNonce;
use futures::{
	future::FutureExt,
	stream::{FusedStream, StreamExt},
};
use relay_utils::{process_future_result, retry_backoff, FailedClient, MaybeConnectionError};
use std::{
	fmt::Debug,
	ops::RangeInclusive,
	time::{Duration, Instant},
};

/// One of races within lane.
pub trait MessageRace {
	/// Header id of the race source.
	type SourceHeaderId: Debug + Clone + PartialEq;
	/// Header id of the race source.
	type TargetHeaderId: Debug + Clone + PartialEq;

	/// Message nonce used in the race.
	type MessageNonce: Debug + Clone;
	/// Proof that is generated and delivered in this race.
	type Proof: Clone;

	/// Name of the race source.
	fn source_name() -> String;
	/// Name of the race target.
	fn target_name() -> String;
}

/// State of race source client.
type SourceClientState<P> = ClientState<<P as MessageRace>::SourceHeaderId, <P as MessageRace>::TargetHeaderId>;

/// State of race target client.
type TargetClientState<P> = ClientState<<P as MessageRace>::TargetHeaderId, <P as MessageRace>::SourceHeaderId>;

/// Inclusive nonces range.
pub trait NoncesRange: Debug + Sized {
	/// Get begin of the range.
	fn begin(&self) -> MessageNonce;
	/// Get end of the range.
	fn end(&self) -> MessageNonce;
	/// Returns new range with current range nonces that are greater than the passed `nonce`.
	/// If there are no such nonces, `None` is returned.
	fn greater_than(self, nonce: MessageNonce) -> Option<Self>;
}

/// Nonces on the race source client.
#[derive(Debug, Clone)]
pub struct SourceClientNonces<NoncesRange> {
	/// New nonces range known to the client. `New` here means all nonces generated after
	/// `prev_latest_nonce` passed to the `SourceClient::nonces` method.
	pub new_nonces: NoncesRange,
	/// Latest nonce that is confirmed to the bridged client. This nonce only makes
	/// sense in some races. In other races it is `None`.
	pub confirmed_nonce: Option<MessageNonce>,
}

/// Nonces on the race target client.
#[derive(Debug, Clone)]
pub struct TargetClientNonces {
	/// Latest nonce that is known to the target client.
	pub latest_nonce: MessageNonce,
	/// Latest nonce that is confirmed to the bridged client. This nonce only makes
	/// sense in some races. In other races it is `None`.
	pub confirmed_nonce: Option<MessageNonce>,
}

/// One of message lane clients, which is source client for the race.
#[async_trait]
pub trait SourceClient<P: MessageRace> {
	/// Type of error this clients returns.
	type Error: std::fmt::Debug + MaybeConnectionError;
	/// Type of nonces range returned by the source client.
	type NoncesRange: NoncesRange;
	/// Additional proof parameters required to generate proof.
	type ProofParameters;

	/// Return nonces that are known to the source client.
	async fn nonces(
		&self,
		at_block: P::SourceHeaderId,
		prev_latest_nonce: MessageNonce,
	) -> Result<(P::SourceHeaderId, SourceClientNonces<Self::NoncesRange>), Self::Error>;
	/// Generate proof for delivering to the target client.
	async fn generate_proof(
		&self,
		at_block: P::SourceHeaderId,
		nonces: RangeInclusive<MessageNonce>,
		proof_parameters: Self::ProofParameters,
	) -> Result<(P::SourceHeaderId, RangeInclusive<MessageNonce>, P::Proof), Self::Error>;
}

/// One of message lane clients, which is target client for the race.
#[async_trait]
pub trait TargetClient<P: MessageRace> {
	/// Type of error this clients returns.
	type Error: std::fmt::Debug + MaybeConnectionError;

	/// Return nonces that are known to the target client.
	async fn nonces(&self, at_block: P::TargetHeaderId)
		-> Result<(P::TargetHeaderId, TargetClientNonces), Self::Error>;
	/// Submit proof to the target client.
	async fn submit_proof(
		&self,
		generated_at_block: P::SourceHeaderId,
		nonces: RangeInclusive<MessageNonce>,
		proof: P::Proof,
	) -> Result<RangeInclusive<MessageNonce>, Self::Error>;
}

/// Race strategy.
pub trait RaceStrategy<SourceHeaderId, TargetHeaderId, Proof> {
	/// Type of nonces range expected from the source client.
	type SourceNoncesRange: NoncesRange;
	/// Additional proof parameters required to generate proof.
	type ProofParameters;

	/// Should return true if nothing has to be synced.
	fn is_empty(&self) -> bool;
	/// Return best nonce at source node.
	fn best_at_source(&self) -> MessageNonce;
	/// Return best nonce at target node.
	fn best_at_target(&self) -> MessageNonce;

	/// Called when nonces are updated at source node of the race.
	fn source_nonces_updated(&mut self, at_block: SourceHeaderId, nonces: SourceClientNonces<Self::SourceNoncesRange>);
	/// Called when nonces are updated at target node of the race.
	fn target_nonces_updated(
		&mut self,
		nonces: TargetClientNonces,
		race_state: &mut RaceState<SourceHeaderId, TargetHeaderId, Proof>,
	);
	/// Should return `Some(nonces)` if we need to deliver proof of `nonces` (and associated
	/// data) from source to target node.
	/// Additionally, parameters required to generate proof are returned.
	fn select_nonces_to_deliver(
		&mut self,
		race_state: &RaceState<SourceHeaderId, TargetHeaderId, Proof>,
	) -> Option<(RangeInclusive<MessageNonce>, Self::ProofParameters)>;
}

/// State of the race.
#[derive(Debug)]
pub struct RaceState<SourceHeaderId, TargetHeaderId, Proof> {
	/// Source state, if known.
	pub source_state: Option<ClientState<SourceHeaderId, TargetHeaderId>>,
	/// Target state, if known.
	pub target_state: Option<ClientState<TargetHeaderId, SourceHeaderId>>,
	/// Range of nonces that we have selected to submit.
	pub nonces_to_submit: Option<(SourceHeaderId, RangeInclusive<MessageNonce>, Proof)>,
	/// Range of nonces that is currently submitted.
	pub nonces_submitted: Option<RangeInclusive<MessageNonce>>,
}

/// Run race loop until connection with target or source node is lost.
pub async fn run<P: MessageRace, SC: SourceClient<P>>(
	race_source: SC,
	race_source_updated: impl FusedStream<Item = SourceClientState<P>>,
	race_target: impl TargetClient<P>,
	race_target_updated: impl FusedStream<Item = TargetClientState<P>>,
	stall_timeout: Duration,
	mut strategy: impl RaceStrategy<
		P::SourceHeaderId,
		P::TargetHeaderId,
		P::Proof,
		SourceNoncesRange = SC::NoncesRange,
		ProofParameters = SC::ProofParameters,
	>,
) -> Result<(), FailedClient> {
	let mut progress_context = Instant::now();
	let mut race_state = RaceState::default();
	let mut stall_countdown = Instant::now();

	let mut source_retry_backoff = retry_backoff();
	let mut source_client_is_online = true;
	let mut source_nonces_required = false;
	let source_nonces = futures::future::Fuse::terminated();
	let source_generate_proof = futures::future::Fuse::terminated();
	let source_go_offline_future = futures::future::Fuse::terminated();

	let mut target_retry_backoff = retry_backoff();
	let mut target_client_is_online = true;
	let mut target_nonces_required = false;
	let target_nonces = futures::future::Fuse::terminated();
	let target_submit_proof = futures::future::Fuse::terminated();
	let target_go_offline_future = futures::future::Fuse::terminated();

	futures::pin_mut!(
		race_source_updated,
		source_nonces,
		source_generate_proof,
		source_go_offline_future,
		race_target_updated,
		target_nonces,
		target_submit_proof,
		target_go_offline_future,
	);

	loop {
		futures::select! {
			// when headers ids are updated
			source_state = race_source_updated.next() => {
				if let Some(source_state) = source_state {
					if race_state.source_state.as_ref() != Some(&source_state) {
						source_nonces_required = true;
						race_state.source_state = Some(source_state);
					}
				}
			},
			target_state = race_target_updated.next() => {
				if let Some(target_state) = target_state {
					if race_state.target_state.as_ref() != Some(&target_state) {
						target_nonces_required = true;
						race_state.target_state = Some(target_state);
					}
				}
			},

			// when nonces are updated
			nonces = source_nonces => {
				source_nonces_required = false;

				source_client_is_online = process_future_result(
					nonces,
					&mut source_retry_backoff,
					|(at_block, nonces)| {
						log::debug!(
							target: "bridge",
							"Received nonces from {}: {:?}",
							P::source_name(),
							nonces,
						);

						strategy.source_nonces_updated(at_block, nonces);
					},
					&mut source_go_offline_future,
					|delay| async_std::task::sleep(delay),
					|| format!("Error retrieving nonces from {}", P::source_name()),
				).fail_if_connection_error(FailedClient::Source)?;
			},
			nonces = target_nonces => {
				target_nonces_required = false;

				target_client_is_online = process_future_result(
					nonces,
					&mut target_retry_backoff,
					|(_, nonces)| {
						log::debug!(
							target: "bridge",
							"Received nonces from {}: {:?}",
							P::target_name(),
							nonces,
						);

						strategy.target_nonces_updated(nonces, &mut race_state);
					},
					&mut target_go_offline_future,
					|delay| async_std::task::sleep(delay),
					|| format!("Error retrieving nonces from {}", P::target_name()),
				).fail_if_connection_error(FailedClient::Target)?;
			},

			// proof generation and submission
			proof = source_generate_proof => {
				source_client_is_online = process_future_result(
					proof,
					&mut source_retry_backoff,
					|(at_block, nonces_range, proof)| {
						log::debug!(
							target: "bridge",
							"Received proof for nonces in range {:?} from {}",
							nonces_range,
							P::source_name(),
						);

						race_state.nonces_to_submit = Some((at_block, nonces_range, proof));
					},
					&mut source_go_offline_future,
					|delay| async_std::task::sleep(delay),
					|| format!("Error generating proof at {}", P::source_name()),
				).fail_if_connection_error(FailedClient::Source)?;
			},
			proof_submit_result = target_submit_proof => {
				target_client_is_online = process_future_result(
					proof_submit_result,
					&mut target_retry_backoff,
					|nonces_range| {
						log::debug!(
							target: "bridge",
							"Successfully submitted proof of nonces {:?} to {}",
							nonces_range,
							P::target_name(),
						);

						race_state.nonces_to_submit = None;
						race_state.nonces_submitted = Some(nonces_range);
					},
					&mut target_go_offline_future,
					|delay| async_std::task::sleep(delay),
					|| format!("Error submitting proof {}", P::target_name()),
				).fail_if_connection_error(FailedClient::Target)?;
			}
		}

		progress_context = print_race_progress::<P, _>(progress_context, &strategy);

		if stall_countdown.elapsed() > stall_timeout {
			return Err(FailedClient::Both);
		} else if race_state.nonces_to_submit.is_none() && race_state.nonces_submitted.is_none() && strategy.is_empty()
		{
			stall_countdown = Instant::now();
		}

		if source_client_is_online {
			source_client_is_online = false;

			let nonces_to_deliver = select_nonces_to_deliver(&race_state, &mut strategy);

			if let Some((at_block, nonces_range, proof_parameters)) = nonces_to_deliver {
				log::debug!(
					target: "bridge",
					"Asking {} to prove nonces in range {:?} at block {:?}",
					P::source_name(),
					nonces_range,
					at_block,
				);
				source_generate_proof.set(
					race_source
						.generate_proof(at_block, nonces_range, proof_parameters)
						.fuse(),
				);
			} else if source_nonces_required {
				log::debug!(target: "bridge", "Asking {} about message nonces", P::source_name());
				let at_block = race_state
					.source_state
					.as_ref()
					.expect("source_nonces_required is only true when source_state is Some; qed")
					.best_self
					.clone();
				source_nonces.set(race_source.nonces(at_block, strategy.best_at_source()).fuse());
			} else {
				source_client_is_online = true;
			}
		}

		if target_client_is_online {
			target_client_is_online = false;

			if let Some((at_block, nonces_range, proof)) = race_state.nonces_to_submit.as_ref() {
				log::debug!(
					target: "bridge",
					"Going to submit proof of messages in range {:?} to {} node",
					nonces_range,
					P::target_name(),
				);
				target_submit_proof.set(
					race_target
						.submit_proof(at_block.clone(), nonces_range.clone(), proof.clone())
						.fuse(),
				);
			}
			if target_nonces_required {
				log::debug!(target: "bridge", "Asking {} about message nonces", P::target_name());
				let at_block = race_state
					.target_state
					.as_ref()
					.expect("target_nonces_required is only true when target_state is Some; qed")
					.best_self
					.clone();
				target_nonces.set(race_target.nonces(at_block).fuse());
			} else {
				target_client_is_online = true;
			}
		}
	}
}

impl<SourceHeaderId, TargetHeaderId, Proof> Default for RaceState<SourceHeaderId, TargetHeaderId, Proof> {
	fn default() -> Self {
		RaceState {
			source_state: None,
			target_state: None,
			nonces_to_submit: None,
			nonces_submitted: None,
		}
	}
}

/// Print race progress.
fn print_race_progress<P, S>(prev_time: Instant, strategy: &S) -> Instant
where
	P: MessageRace,
	S: RaceStrategy<P::SourceHeaderId, P::TargetHeaderId, P::Proof>,
{
	let now_time = Instant::now();

	let need_update = now_time.saturating_duration_since(prev_time) > Duration::from_secs(10);
	if !need_update {
		return prev_time;
	}

	let now_best_nonce_at_source = strategy.best_at_source();
	let now_best_nonce_at_target = strategy.best_at_target();
	log::info!(
		target: "bridge",
		"Synced {:?} of {:?} nonces in {} -> {} race",
		now_best_nonce_at_target,
		now_best_nonce_at_source,
		P::source_name(),
		P::target_name(),
	);
	now_time
}

fn select_nonces_to_deliver<SourceHeaderId, TargetHeaderId, Proof, Strategy>(
	race_state: &RaceState<SourceHeaderId, TargetHeaderId, Proof>,
	strategy: &mut Strategy,
) -> Option<(SourceHeaderId, RangeInclusive<MessageNonce>, Strategy::ProofParameters)>
where
	SourceHeaderId: Clone,
	Strategy: RaceStrategy<SourceHeaderId, TargetHeaderId, Proof>,
{
	race_state.target_state.as_ref().and_then(|target_state| {
		strategy
			.select_nonces_to_deliver(&race_state)
			.map(|(nonces_range, proof_parameters)| (target_state.best_peer.clone(), nonces_range, proof_parameters))
	})
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::message_race_strategy::BasicStrategy;
	use relay_utils::HeaderId;

	#[test]
	fn proof_is_generated_at_best_block_known_to_target_node() {
		const GENERATED_AT: u64 = 6;
		const BEST_AT_SOURCE: u64 = 10;
		const BEST_AT_TARGET: u64 = 8;

		// target node only knows about source' BEST_AT_TARGET block
		// source node has BEST_AT_SOURCE > BEST_AT_TARGET block
		let mut race_state = RaceState::<_, _, ()> {
			source_state: Some(ClientState {
				best_self: HeaderId(BEST_AT_SOURCE, BEST_AT_SOURCE),
				best_peer: HeaderId(0, 0),
			}),
			target_state: Some(ClientState {
				best_self: HeaderId(0, 0),
				best_peer: HeaderId(BEST_AT_TARGET, BEST_AT_TARGET),
			}),
			nonces_to_submit: None,
			nonces_submitted: None,
		};

		// we have some nonces to deliver and they're generated at GENERATED_AT < BEST_AT_SOURCE
		let mut strategy = BasicStrategy::new();
		strategy.source_nonces_updated(
			HeaderId(GENERATED_AT, GENERATED_AT),
			SourceClientNonces {
				new_nonces: 0..=10,
				confirmed_nonce: None,
			},
		);
		strategy.target_nonces_updated(
			TargetClientNonces {
				latest_nonce: 5u64,
				confirmed_nonce: None,
			},
			&mut race_state,
		);

		// the proof will be generated on source, but using BEST_AT_TARGET block
		assert_eq!(
			select_nonces_to_deliver(&race_state, &mut strategy),
			Some((HeaderId(BEST_AT_TARGET, BEST_AT_TARGET), 6..=10, (),))
		);
	}
}
