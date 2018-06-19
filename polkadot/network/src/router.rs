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

//! Statement routing and consenuss table router implementation.

use polkadot_api::{PolkadotApi, LocalPolkadotApi};
use polkadot_consensus::{SharedTable, TableRouter, SignedStatement, Statement, GenericStatement};
use polkadot_primitives::{Hash, BlockId, SessionKey};
use polkadot_primitives::parachain::{BlockData, Extrinsic, CandidateReceipt};

use futures::{future, prelude::*};
use tokio::runtime::TaskExecutor;
use parking_lot::Mutex;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use super::NetworkService;

/// Table routing implementation.
pub struct Router<P: PolkadotApi> {
	table: Arc<SharedTable>,
	network: Arc<NetworkService>,
	api: Arc<P>,
	task_executor: TaskExecutor,
	parent_hash: Option<P::CheckedBlockId>,
	deferred_statements: Arc<Mutex<DeferredStatements>>,
}

impl<P: PolkadotApi> Router<P> {
	pub(crate) fn new(
		table: Arc<SharedTable>,
		network: Arc<NetworkService>,
		api: Arc<P>,
		task_executor: TaskExecutor,
		parent_hash: Option<P::CheckedBlockId>,
	) -> Self {
		Router {
			table,
			network,
			api,
			task_executor,
			parent_hash,
			deferred_statements: Arc::new(Mutex::new(DeferredStatements::new())),
		}
	}

	pub(crate) fn session_key(&self) -> SessionKey {
		self.table.session_key()
	}
}

impl<P: PolkadotApi> Clone for Router<P> {
	fn clone(&self) -> Self {
		Router {
			table: self.table.clone(),
			network: self.network.clone(),
			api: self.api.clone(),
			task_executor: self.task_executor.clone(),
			parent_hash: self.parent_hash.clone(),
			deferred_statements: self.deferred_statements.clone(),
		}
	}
}

impl<P: LocalPolkadotApi + Send + Sync + 'static> Router<P> where P::CheckedBlockId: Send {
	/// Import a statement whose signature has been checked already.
	pub(crate) fn import_statement(&self, statement: SignedStatement) {
		// defer any statements for which we haven't imported the candidate yet
		let defer = match statement.statement {
			GenericStatement::Candidate(_) => false,
			GenericStatement::Valid(ref hash)
				| GenericStatement::Invalid(ref hash)
				| GenericStatement::Available(ref hash)
				=> self.table.with_candidate(hash, |c| c.is_none()),
		};

		if defer {
			self.deferred_statements.lock().push(statement);
			return;
		}

		// import all statements pending on this candidate
		let (pending, _traces) = if let GenericStatement::Candidate(ref candidate) = statement.statement {
			self.deferred_statements.lock().get_deferred(&candidate.hash())
		} else {
			(Vec::new(), Vec::new())
		};

		let producers: Vec<_> = self.table.import_remote_statements(
			self,
			::std::iter::once(statement).chain(pending),
		);

		// dispatch future work as necessary.
		for producer in producers.into_iter().filter(|p| !p.is_blank()) {
			let api = self.api.clone();
			let parent_hash = self.parent_hash.clone();

			let validate = move |collation| -> Option<bool> {
				let checked = parent_hash.clone()?;

				match ::polkadot_consensus::validate_collation(&*api, &checked, &collation) {
					Ok(()) => Some(true),
					Err(e) => {
						debug!(target: "p_net", "Encountered bad collation: {}", e);
						Some(false)
					}
				}
			};

			let table = self.table.clone();
			let work = producer.prime(validate).map(move |produced| {
				// TODO: ensure availability of block/extrinsic
				// and propagate these statements.
				if let Some(validity) = produced.validity {
					table.sign_and_import(validity);
				}

				if let Some(availability) = produced.availability {
					table.sign_and_import(availability);
				}
			});

			self.task_executor.spawn(work);
		}
	}
}

impl<P: LocalPolkadotApi + Send> TableRouter for Router<P> where P::CheckedBlockId: Send {
	type Error = ();
	type FetchCandidate = future::Empty<BlockData, Self::Error>;
	type FetchExtrinsic = Result<Extrinsic, Self::Error>;

	fn local_candidate_data(&self, _hash: Hash, _block_data: BlockData, _extrinsic: Extrinsic) {
		// give to network to make available and multicast
	}

	fn fetch_block_data(&self, _candidate: &CandidateReceipt) -> Self::FetchCandidate {
		future::empty()
	}

	fn fetch_extrinsic_data(&self, _candidate: &CandidateReceipt) -> Self::FetchExtrinsic {
		Ok(Extrinsic)
	}
}

// A unique trace for valid statements issued by a validator.
#[derive(Hash, PartialEq, Eq, Clone, Debug)]
enum StatementTrace {
	Valid(SessionKey, Hash),
	Invalid(SessionKey, Hash),
	Available(SessionKey, Hash),
}

// helper for deferring statements whose associated candidate is unknown.
struct DeferredStatements {
	deferred: HashMap<Hash, Vec<SignedStatement>>,
	known_traces: HashSet<StatementTrace>,
}

impl DeferredStatements {
	fn new() -> Self {
		DeferredStatements {
			deferred: HashMap::new(),
			known_traces: HashSet::new(),
		}
	}

	fn push(&mut self, statement: SignedStatement) {
		let (hash, trace) = match statement.statement {
			GenericStatement::Candidate(_) => return,
			GenericStatement::Valid(hash) => (hash, StatementTrace::Valid(statement.sender, hash)),
			GenericStatement::Invalid(hash) => (hash, StatementTrace::Invalid(statement.sender, hash)),
			GenericStatement::Available(hash) => (hash, StatementTrace::Available(statement.sender, hash)),
		};

		if self.known_traces.insert(trace) {
			self.deferred.entry(hash).or_insert_with(Vec::new).push(statement);
		}
	}

	fn get_deferred(&mut self, hash: &Hash) -> (Vec<SignedStatement>, Vec<StatementTrace>) {
		match self.deferred.remove(hash) {
			None => (Vec::new(), Vec::new()),
			Some(deferred) => {
				let mut traces = Vec::new();
				for statement in deferred.iter() {
					let trace = match statement.statement {
						GenericStatement::Candidate(_) => continue,
						GenericStatement::Valid(hash) => StatementTrace::Valid(statement.sender, hash),
						GenericStatement::Invalid(hash) => StatementTrace::Invalid(statement.sender, hash),
						GenericStatement::Available(hash) => StatementTrace::Available(statement.sender, hash),
					};

					self.known_traces.remove(&trace);
					traces.push(trace);
				}

				(deferred, traces)
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use substrate_primitives::H512;

	#[test]
	fn deferred_statements_works() {
		let mut deferred = DeferredStatements::new();
		let hash = [1; 32].into();
		let sig = H512([2; 64]).into();
		let sender = [255; 32].into();

		let statement = SignedStatement {
			statement: GenericStatement::Valid(hash),
			sender,
			signature: sig,
		};

		// pre-push.
		{
			let (signed, traces) = deferred.get_deferred(&hash);
			assert!(signed.is_empty());
			assert!(traces.is_empty());
		}

		deferred.push(statement.clone());
		deferred.push(statement.clone());

		// draining: second push should have been ignored.
		{
			let (signed, traces) = deferred.get_deferred(&hash);
			assert_eq!(signed.len(), 1);

			assert_eq!(traces.len(), 1);
			assert_eq!(signed[0].clone(), statement);
			assert_eq!(traces[0].clone(), StatementTrace::Valid(sender, hash));
		}

		// after draining
		{
			let (signed, traces) = deferred.get_deferred(&hash);
			assert!(signed.is_empty());
			assert!(traces.is_empty());
		}
	}
}
