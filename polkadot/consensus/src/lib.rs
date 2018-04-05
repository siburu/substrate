// Copyright 2017 Parity Technologies (UK) Ltd.
// This file is part of Polkadot.

// Polkadot is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Polkadot is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Polkadot.  If not, see <http://www.gnu.org/licenses/>.

//! Propagation and agreement of candidates.
//!
//! Authorities are split into groups by parachain, and each authority might come
//! up its own candidate for their parachain. Within groups, authorities pass around
//! their candidates and produce statements of validity.
//!
//! Any candidate that receives majority approval by the authorities in a group
//! may be subject to inclusion, unless any authorities flag that candidate as invalid.
//!
//! Wrongly flagging as invalid should be strongly disincentivized, so that in the
//! equilibrium state it is not expected to happen. Likewise with the submission
//! of invalid blocks.
//!
//! Groups themselves may be compromised by malicious authorities.

extern crate ed25519;
extern crate parking_lot;
extern crate tokio_timer;
extern crate polkadot_api;
extern crate polkadot_collator as collator;
extern crate polkadot_statement_table as table;
extern crate polkadot_primitives;
extern crate polkadot_transaction_pool as transaction_pool;
extern crate polkadot_runtime;
extern crate substrate_bft as bft;
extern crate substrate_codec as codec;
extern crate substrate_primitives as primitives;
extern crate substrate_runtime_support as runtime_support;
extern crate substrate_network;

extern crate tokio_core;
extern crate substrate_keyring;
extern crate substrate_client as client;

#[macro_use]
extern crate error_chain;

#[macro_use]
extern crate futures;

#[macro_use]
extern crate log;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use codec::Slicable;
use table::{Table, Context as TableContextTrait};
use table::generic::Statement as GenericStatement;
use runtime_support::Hashable;
use polkadot_api::{PolkadotApi, BlockBuilder};
use polkadot_primitives::{Hash, Timestamp};
use polkadot_primitives::parachain::{Id as ParaId, DutyRoster, BlockData, Extrinsic, CandidateReceipt};
use polkadot_runtime::Block as PolkadotGenericBlock;
use primitives::block::{Block as SubstrateBlock, Header as SubstrateHeader, HeaderHash, Id as BlockId, Number as BlockNumber};
use primitives::AuthorityId;
use transaction_pool::{Ready, TransactionPool, PolkadotBlock};

use futures::prelude::*;
use futures::future;
use parking_lot::Mutex;
use collation::{Collation, Collators, CollationFetch};
use dynamic_inclusion::DynamicInclusion;

pub use self::error::{ErrorKind, Error};
pub use service::Service;

mod collation;
mod dynamic_inclusion;
mod error;
mod service;

// block size limit.
const MAX_TRANSACTIONS_SIZE: usize = 4 * 1024 * 1024;

/// A handle to a statement table router.
pub trait TableRouter {
	/// Errors when fetching data from the network.
	type Error;
	/// Future that resolves when candidate data is fetched.
	type FetchCandidate: IntoFuture<Item=BlockData,Error=Self::Error>;
	/// Future that resolves when extrinsic candidate data is fetched.
	type FetchExtrinsic: IntoFuture<Item=Extrinsic,Error=Self::Error>;

	/// Note local candidate data, making it available on the network to other validators.
	fn local_candidate_data(&self, hash: Hash, block_data: BlockData, extrinsic: Extrinsic);

	/// Fetch block data for a specific candidate.
	fn fetch_block_data(&self, candidate: &CandidateReceipt) -> Self::FetchCandidate;

	/// Fetch extrinsic data for a specific candidate.
	fn fetch_extrinsic_data(&self, candidate: &CandidateReceipt) -> Self::FetchExtrinsic;
}

/// A long-lived network which can create statement table routing instances.
pub trait Network {
	/// The table router type. This should handle importing of any statements,
	/// routing statements to peers, and driving completion of any `StatementProducers`.
	type TableRouter: TableRouter;

	/// Instantiate a table router using the given shared table.
	fn table_router(&self, table: Arc<SharedTable>) -> Self::TableRouter;
}

/// Information about a specific group.
#[derive(Debug, Clone, Default)]
pub struct GroupInfo {
	/// Authorities meant to check validity of candidates.
	pub validity_guarantors: HashSet<AuthorityId>,
	/// Authorities meant to check availability of candidate data.
	pub availability_guarantors: HashSet<AuthorityId>,
	/// Number of votes needed for validity.
	pub needed_validity: usize,
	/// Number of votes needed for availability.
	pub needed_availability: usize,
}

struct TableContext {
	parent_hash: Hash,
	key: Arc<ed25519::Pair>,
	groups: HashMap<ParaId, GroupInfo>,
}

impl table::Context for TableContext {
	fn is_member_of(&self, authority: &AuthorityId, group: &ParaId) -> bool {
		self.groups.get(group).map_or(false, |g| g.validity_guarantors.contains(authority))
	}

	fn is_availability_guarantor_of(&self, authority: &AuthorityId, group: &ParaId) -> bool {
		self.groups.get(group).map_or(false, |g| g.availability_guarantors.contains(authority))
	}

	fn requisite_votes(&self, group: &ParaId) -> (usize, usize) {
		self.groups.get(group).map_or(
			(usize::max_value(), usize::max_value()),
			|g| (g.needed_validity, g.needed_availability),
		)
	}
}

impl TableContext {
	fn local_id(&self) -> AuthorityId {
		self.key.public().0
	}

	fn sign_statement(&self, statement: table::Statement) -> table::SignedStatement {
		let signature = sign_table_statement(&statement, &self.key, &self.parent_hash).into();
		let local_id = self.key.public().0;

		table::SignedStatement {
			statement,
			signature,
			sender: local_id,
		}
	}
}

/// Sign a table statement against a parent hash.
/// The actual message signed is the encoded statement concatenated with the
/// parent hash.
pub fn sign_table_statement(statement: &table::Statement, key: &ed25519::Pair, parent_hash: &Hash) -> ed25519::Signature {
	use polkadot_primitives::parachain::Statement as RawStatement;

	let raw = match *statement {
		GenericStatement::Candidate(ref c) => RawStatement::Candidate(c.clone()),
		GenericStatement::Valid(h) => RawStatement::Valid(h),
		GenericStatement::Invalid(h) => RawStatement::Invalid(h),
		GenericStatement::Available(h) => RawStatement::Available(h),
	};

	let mut encoded = raw.encode();
	encoded.extend(&parent_hash.0);

	key.sign(&encoded)
}

/// Source of statements
pub enum StatementSource {
	/// Locally produced statement.
	Local,
	/// Received statement from remote source, with optional sender.
	Remote(Option<AuthorityId>),
}

// A shared table object.
struct SharedTableInner {
	table: Table<TableContext>,
	proposed_digest: Option<Hash>,
	checked_validity: HashSet<Hash>,
	checked_availability: HashSet<Hash>,
}

impl SharedTableInner {
	// Import a single statement. Provide a handle to a table router.
	fn import_statement<R: TableRouter>(
		&mut self,
		context: &TableContext,
		router: &R,
		statement: table::SignedStatement,
		statement_source: StatementSource,
	) -> StatementProducer<<R::FetchCandidate as IntoFuture>::Future, <R::FetchExtrinsic as IntoFuture>::Future> {
		// this blank producer does nothing until we attach some futures
		// and set a candidate digest.
		let mut producer = Default::default();

		let received_from = match statement_source {
			StatementSource::Local => return producer,
			StatementSource::Remote(from) => from,
		};

		let summary = match self.table.import_statement(context, statement, received_from) {
			Some(summary) => summary,
			None => return producer,
		};

		producer.candidate_digest = Some(summary.candidate);

		let local_id = context.local_id();

		let is_validity_member = context.is_member_of(&local_id, &summary.group_id);
		let is_availability_member =
			context.is_availability_guarantor_of(&local_id, &summary.group_id);

		let digest = &summary.candidate;

		// TODO: consider a strategy based on the number of candidate votes as well.
		// only check validity if this wasn't locally proposed.
		let checking_validity = is_validity_member
			&& self.proposed_digest.as_ref().map_or(true, |d| d != digest)
			&& self.checked_validity.insert(digest.clone());

		let checking_availability = is_availability_member
			&& self.checked_availability.insert(digest.clone());

		if checking_validity || checking_availability {
			match self.table.get_candidate(&digest) {
				None => {} // TODO: handle table inconsistency somehow?
				Some(candidate) => {
					if checking_validity {
						producer.fetch_block_data = Some(
							router.fetch_block_data(candidate).into_future().fuse()
						);
					}

					if checking_availability {
						producer.fetch_extrinsic = Some(
							router.fetch_extrinsic_data(candidate).into_future().fuse()
						);
					}
				}
			}
		}

		producer
	}
}

/// Produced statements about a specific candidate.
/// Both may be `None`.
#[derive(Default)]
pub struct ProducedStatements {
	/// A statement about the validity of the candidate.
	pub validity: Option<table::Statement>,
	/// A statement about availability of data. If this is `Some`,
	/// then `block_data` and `extrinsic` should be `Some` as well.
	pub availability: Option<table::Statement>,
	/// Block data to ensure availability of.
	pub block_data: Option<BlockData>,
	/// Extrinsic data to ensure availability of.
	pub extrinsic: Option<Extrinsic>,
}

/// Future that produces statements about a specific candidate.
pub struct StatementProducer<D: Future, E: Future> {
	fetch_block_data: Option<future::Fuse<D>>,
	fetch_extrinsic: Option<future::Fuse<E>>,
	produced_statements: ProducedStatements,
	candidate_digest: Option<Hash>,
}

impl<D: Future, E: Future> Default for StatementProducer<D, E> {
	fn default() -> Self {
		StatementProducer {
			fetch_block_data: None,
			fetch_extrinsic: None,
			produced_statements: Default::default(),
			candidate_digest: Default::default(),
		}
	}
}

impl<D, E, Err> Future for StatementProducer<D, E>
	where
		D: Future<Item=BlockData,Error=Err>,
		E: Future<Item=Extrinsic,Error=Err>,
{
	type Item = ProducedStatements;
	type Error = Err;

	fn poll(&mut self) -> Poll<ProducedStatements, Err> {
		let candidate_digest = match self.candidate_digest.as_ref() {
			Some(x) => x,
			None => return Ok(Async::Ready(::std::mem::replace(&mut self.produced_statements, Default::default()))),
		};

		let mut done = true;
		if let Some(ref mut fetch_block_data) = self.fetch_block_data {
			match fetch_block_data.poll()? {
				Async::Ready(block_data) => {
					// TODO [PoC-2]: validate block data here and make statement.
					self.produced_statements.block_data = Some(block_data);
				},
				Async::NotReady => {
					done = false;
				}
			}
		}

		if let Some(ref mut fetch_extrinsic) = self.fetch_extrinsic {
			match fetch_extrinsic.poll()? {
				Async::Ready(extrinsic) => {
					self.produced_statements.extrinsic = Some(extrinsic);
				}
				Async::NotReady => {
					done = false;
				}
			}
		}

		if done {
			let mut produced = ::std::mem::replace(&mut self.produced_statements, Default::default());
			if produced.block_data.is_some() && produced.extrinsic.is_some() {
				// produce a statement about availability.
				produced.availability = Some(GenericStatement::Available(candidate_digest.clone()));
			}
			Ok(Async::Ready(produced))
		} else {
			Ok(Async::NotReady)
		}
	}
}

/// A shared table object.
pub struct SharedTable {
	context: Arc<TableContext>,
	inner: Arc<Mutex<SharedTableInner>>,
}

impl Clone for SharedTable {
	fn clone(&self) -> Self {
		SharedTable {
			context: self.context.clone(),
			inner: self.inner.clone(),
		}
	}
}

impl SharedTable {
	/// Create a new shared table.
	///
	/// Provide the key to sign with, and the parent hash of the relay chain
	/// block being built.
	pub fn new(groups: HashMap<ParaId, GroupInfo>, key: Arc<ed25519::Pair>, parent_hash: Hash) -> Self {
		SharedTable {
			context: Arc::new(TableContext { groups, key, parent_hash }),
			inner: Arc::new(Mutex::new(SharedTableInner {
				table: Table::default(),
				proposed_digest: None,
				checked_validity: HashSet::new(),
				checked_availability: HashSet::new(),
			}))
		}
	}

	/// Get group info.
	pub fn group_info(&self) -> &HashMap<ParaId, GroupInfo> {
		&self.context.groups
	}

	/// Import a single statement. Provide a handle to a table router
	/// for dispatching any other requests which come up.
	pub fn import_statement<R: TableRouter>(
		&self,
		router: &R,
		statement: table::SignedStatement,
		received_from: StatementSource,
	) -> StatementProducer<<R::FetchCandidate as IntoFuture>::Future, <R::FetchExtrinsic as IntoFuture>::Future> {
		self.inner.lock().import_statement(&*self.context, router, statement, received_from)
	}

	/// Sign and import a local statement.
	pub fn sign_and_import<R: TableRouter>(
		&self,
		router: &R,
		statement: table::Statement,
	) {
		let proposed_digest = match statement {
			GenericStatement::Candidate(ref c) => Some(c.hash()),
			_ => None,
		};

		let signed_statement = self.context.sign_statement(statement);

		let mut inner = self.inner.lock();
		if proposed_digest.is_some() {
			inner.proposed_digest = proposed_digest;
		}

		inner.import_statement(&*self.context, router, signed_statement, StatementSource::Local);
	}

	/// Import many statements at once.
	///
	/// Provide an iterator yielding pairs of (statement, statement_source).
	pub fn import_statements<R, I, U>(&self, router: &R, iterable: I) -> U
		where
			R: TableRouter,
			I: IntoIterator<Item=(table::SignedStatement, StatementSource)>,
			U: ::std::iter::FromIterator<StatementProducer<
				<R::FetchCandidate as IntoFuture>::Future,
				<R::FetchExtrinsic as IntoFuture>::Future>
			>,
	{
		let mut inner = self.inner.lock();

		iterable.into_iter().map(move |(statement, statement_source)| {
			inner.import_statement(&*self.context, router, statement, statement_source)
		}).collect()
	}

	/// Check if a proposal is valid.
	pub fn proposal_valid(&self, _proposal: &SubstrateBlock) -> bool {
		false // TODO
	}

	/// Execute a closure using a specific candidate.
	///
	/// Deadlocks if called recursively.
	pub fn with_candidate<F, U>(&self, digest: &Hash, f: F) -> U
		where F: FnOnce(Option<&CandidateReceipt>) -> U
	{
		let inner = self.inner.lock();
		f(inner.table.get_candidate(digest))
	}

	/// Get all witnessed misbehavior.
	pub fn get_misbehavior(&self) -> HashMap<AuthorityId, table::Misbehavior> {
		self.inner.lock().table.get_misbehavior().clone()
	}

	/// Fill a statement batch.
	pub fn fill_batch<B: table::StatementBatch>(&self, batch: &mut B) {
		self.inner.lock().table.fill_batch(batch);
	}

	/// Get the local proposed block's hash.
	pub fn proposed_hash(&self) -> Option<Hash> {
		self.inner.lock().proposed_digest.clone()
	}
}

fn make_group_info(roster: DutyRoster, authorities: &[AuthorityId]) -> Result<HashMap<ParaId, GroupInfo>, Error> {
	if roster.validator_duty.len() != authorities.len() {
		bail!(ErrorKind::InvalidDutyRosterLength(authorities.len(), roster.validator_duty.len()))
	}

	if roster.guarantor_duty.len() != authorities.len() {
		bail!(ErrorKind::InvalidDutyRosterLength(authorities.len(), roster.guarantor_duty.len()))
	}

	let mut map = HashMap::new();

	let duty_iter = authorities.iter().zip(&roster.validator_duty).zip(&roster.guarantor_duty);
	for ((authority, v_duty), a_duty) in duty_iter {
		use polkadot_primitives::parachain::Chain;

		match *v_duty {
			Chain::Relay => {}, // does nothing for now.
			Chain::Parachain(ref id) => {
				map.entry(id.clone()).or_insert_with(GroupInfo::default)
					.validity_guarantors
					.insert(authority.clone());
			}
		}

		match *a_duty {
			Chain::Relay => {}, // does nothing for now.
			Chain::Parachain(ref id) => {
				map.entry(id.clone()).or_insert_with(GroupInfo::default)
					.availability_guarantors
					.insert(authority.clone());
			}
		}
	}

	for live_group in map.values_mut() {
		let validity_len = live_group.validity_guarantors.len();
		let availability_len = live_group.availability_guarantors.len();

		live_group.needed_validity = validity_len / 2 + validity_len % 2;
		live_group.needed_availability = availability_len / 2 + availability_len % 2;
	}

	Ok(map)
}

/// Polkadot proposer factory.
pub struct ProposerFactory<C, N, P> {
	/// The client instance.
	pub client: Arc<C>,
	/// The transaction pool.
	pub transaction_pool: Arc<Mutex<TransactionPool>>,
	/// The backing network handle.
	pub network: N,
	/// Parachain collators.
	pub collators: Arc<P>,
	/// The duration after which parachain-empty blocks will be allowed.
	pub parachain_empty_duration: Duration,
}

impl<C, N, P> bft::ProposerFactory for ProposerFactory<C, N, P>
	where
		C: PolkadotApi,
		N: Network,
		P: Collators,
{
	type Proposer = Proposer<C, N::TableRouter, P>;
	type Error = Error;

	fn init(&self, parent_header: &SubstrateHeader, authorities: &[AuthorityId], sign_with: Arc<ed25519::Pair>) -> Result<Self::Proposer, Error> {
		let parent_hash = parent_header.blake2_256().into();

		let checked_id = self.client.check_id(BlockId::Hash(parent_hash))?;
		let duty_roster = self.client.duty_roster(&checked_id)?;

		let group_info = make_group_info(duty_roster, authorities)?;
		let n_parachains = group_info.len();
		let table = Arc::new(SharedTable::new(group_info, sign_with.clone(), parent_hash));
		let router = self.network.table_router(table.clone());
		let dynamic_inclusion = DynamicInclusion::new(
			n_parachains,
			Instant::now(),
			self.parachain_empty_duration.clone(),
		);

		Ok(Proposer {
			parent_hash,
			parent_number: parent_header.number,
			parent_id: checked_id,
			local_key: sign_with,
			client: self.client.clone(),
			transaction_pool: self.transaction_pool.clone(),
			collators: self.collators.clone(),
			dynamic_inclusion,
			table,
			router,
		})
	}
}

fn current_timestamp() -> Timestamp {
	use std::time;

	time::SystemTime::now().duration_since(time::UNIX_EPOCH)
		.expect("now always later than unix epoch; qed")
		.as_secs()
}

/// The Polkadot proposer logic.
pub struct Proposer<C: PolkadotApi, R, P> {
	parent_hash: HeaderHash,
	parent_number: BlockNumber,
	parent_id: C::CheckedBlockId,
	client: Arc<C>,
	local_key: Arc<ed25519::Pair>,
	transaction_pool: Arc<Mutex<TransactionPool>>,
	collators: Arc<P>,
	dynamic_inclusion: DynamicInclusion,
	table: Arc<SharedTable>,
	router: R,
}

impl<C, R, P> bft::Proposer for Proposer<C, R, P>
	where
		C: PolkadotApi,
		R: TableRouter,
		P: Collators,
{
	type Error = Error;
	type Create = CreateProposal<C, R, P>;
	type Evaluate = Result<bool, Error>;

	fn propose(&self) -> CreateProposal<C, R, P> {
		unimplemented!();
	}

	// TODO: certain kinds of errors here should lead to a misbehavior report.
	fn evaluate(&self, proposal: &SubstrateBlock) -> Result<bool, Error> {
		evaluate_proposal(proposal, &*self.client, current_timestamp(), &self.parent_hash, &self.parent_id)
	}

	fn import_misbehavior(&self, misbehavior: Vec<(AuthorityId, bft::Misbehavior)>) {
		use bft::generic::Misbehavior as GenericMisbehavior;
		use primitives::bft::{MisbehaviorKind, MisbehaviorReport};
		use polkadot_runtime::{Call, Extrinsic, UncheckedExtrinsic, ConsensusCall};

		let local_id = self.local_key.public().0;
		let mut pool = self.transaction_pool.lock();
		let mut next_index = {
			let readiness_evaluator = Ready::create(self.parent_id.clone(), &*self.client);

			let cur_index = pool.pending(readiness_evaluator)
				.filter(|tx| tx.as_ref().as_ref().signed == local_id)
				.last()
				.map(|tx| Ok(tx.as_ref().as_ref().index))
				.unwrap_or_else(|| self.client.index(&self.parent_id, local_id));

			match cur_index {
				Ok(cur_index) => cur_index + 1,
				Err(e) => {
					warn!(target: "consensus", "Error computing next transaction index: {}", e);
					return;
				}
			}
		};

		for (target, misbehavior) in misbehavior {
			let report = MisbehaviorReport {
				parent_hash: self.parent_hash,
				parent_number: self.parent_number,
				target,
				misbehavior: match misbehavior {
					GenericMisbehavior::ProposeOutOfTurn(_, _, _) => continue,
					GenericMisbehavior::DoublePropose(_, _, _) => continue,
					GenericMisbehavior::DoublePrepare(round, (h1, s1), (h2, s2))
						=> MisbehaviorKind::BftDoublePrepare(round as u32, (h1, s1.signature), (h2, s2.signature)),
					GenericMisbehavior::DoubleCommit(round, (h1, s1), (h2, s2))
						=> MisbehaviorKind::BftDoubleCommit(round as u32, (h1, s1.signature), (h2, s2.signature)),
				}
			};
			let extrinsic = Extrinsic {
				signed: local_id,
				index: next_index,
				function: Call::Consensus(ConsensusCall::report_misbehavior(report)),
			};

			next_index += 1;

			let signature = self.local_key.sign(&extrinsic.encode()).into();
			let uxt = UncheckedExtrinsic { extrinsic, signature };

			pool.import(uxt).expect("locally signed extrinsic is valid; qed");
		}
	}
}

/// Future which resolves upon the creation of a proposal.
pub struct CreateProposal<C: PolkadotApi, R, P: Collators>  {
	parent_hash: HeaderHash,
	parent_number: BlockNumber,
	parent_id: C::CheckedBlockId,
	client: Arc<C>,
	transaction_pool: Arc<Mutex<TransactionPool>>,
	dynamic_inclusion: DynamicInclusion,
	collation: CollationFetch<P, C>,
	router: R,
	table: Arc<SharedTable>,
}

impl<C, R, P> CreateProposal<C, R, P>
	where
		C: PolkadotApi,
		R: TableRouter,
		P: Collators,
{
	fn propose(&self) -> Result<SubstrateBlock, Error> {
		// TODO: handle case when current timestamp behind that in state.
		let mut block_builder = self.client.build_block(
			&self.parent_id,
			current_timestamp()
		)?;

		let readiness_evaluator = Ready::create(self.parent_id.clone(), &*self.client);

		{
			let mut pool = self.transaction_pool.lock();
			let mut unqueue_invalid = Vec::new();
			let mut pending_size = 0;
			for pending in pool.pending(readiness_evaluator.clone()) {
				// skip and cull transactions which are too large.
				if pending.encoded_size() > MAX_TRANSACTIONS_SIZE {
					unqueue_invalid.push(pending.hash().clone());
					continue
				}

				if pending_size + pending.encoded_size() >= MAX_TRANSACTIONS_SIZE { break }

				match block_builder.push_extrinsic(pending.as_transaction().clone()) {
					Ok(()) => {
						pending_size += pending.encoded_size();
					}
					Err(_) => {
						unqueue_invalid.push(pending.hash().clone());
					}
				}
			}

			for tx_hash in unqueue_invalid {
				pool.remove(&tx_hash, false);
			}
		}

		let polkadot_block = block_builder.bake();
		let substrate_block = Slicable::decode(&mut polkadot_block.encode().as_slice())
			.expect("polkadot blocks defined to serialize to substrate blocks correctly; qed");

		Ok(substrate_block)
	}
}

impl<C, R, P> Future for CreateProposal<C, R, P>
	where
		C: PolkadotApi,
		R: TableRouter,
		P: Collators,
{
	type Item = SubstrateBlock;
	type Error = Error;

	fn poll(&mut self) -> Poll<SubstrateBlock, Error> {
		// 1. poll local collation future.
		match self.collation.poll() {
			Ok(Async::Ready(collation)) => {
				let hash = collation.receipt.hash();
				self.router.local_candidate_data(hash, collation.block_data, collation.extrinsic);
				self.table.sign_and_import(&self.router, GenericStatement::Valid(hash));
			}
			Ok(Async::NotReady) => {},
			Err(_) => {}, // TODO: handle this failure to collate.
		}

		// 2. check readiness timer

		// 3. check reseal interval
		unimplemented!()
	}
}

fn evaluate_proposal<C: PolkadotApi>(
	proposal: &SubstrateBlock,
	client: &C,
	now: Timestamp,
	parent_hash: &HeaderHash,
	parent_id: &C::CheckedBlockId,
) -> Result<bool, Error> {
	const MAX_TIMESTAMP_DRIFT: Timestamp = 4;

	let encoded = Slicable::encode(proposal);
	let proposal = PolkadotGenericBlock::decode(&mut &encoded[..])
		.and_then(|b| PolkadotBlock::from(b).ok())
		.ok_or_else(|| ErrorKind::ProposalNotForPolkadot)?;

	let transactions_size = proposal.extrinsics.iter().fold(0, |a, tx| {
		a + Slicable::encode(tx).len()
	});

	if transactions_size > MAX_TRANSACTIONS_SIZE {
		bail!(ErrorKind::ProposalTooLarge(transactions_size))
	}

	if proposal.header.parent_hash != *parent_hash {
		bail!(ErrorKind::WrongParentHash(*parent_hash, proposal.header.parent_hash));
	}

	// no need to check number because
	// a) we assume the parent is valid.
	// b) the runtime checks that `proposal.parent_hash` == `block_hash(proposal.number - 1)`

	let block_timestamp = proposal.timestamp();

	// TODO: just defer using `tokio_timer` to delay prepare vote.
	if block_timestamp > now + MAX_TIMESTAMP_DRIFT {
		bail!(ErrorKind::TimestampInFuture)
	}

	// execute the block.
	client.evaluate_block(parent_id, proposal.into())?;
	Ok(true)
}
