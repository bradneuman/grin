// Copyright 2016 The Grin Developers
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Blocks and blockheaders

use time;
use rand::{thread_rng, Rng};
use std::collections::HashSet;

use core::{
	Committed,
	Input,
	Output,
	OutputIdentifier,
	ShortId,
	SwitchCommitHash,
	Proof,
	TxKernel,
	Transaction,
	OutputFeatures,
	KernelFeatures
};
use consensus;
use consensus::{exceeds_weight, reward, REWARD, VerifySortOrder};
use core::hash::{Hash, Hashed, ZERO_HASH};
use core::id::ShortIdentifiable;
use core::target::Difficulty;
use core::transaction;
use ser::{self, Readable, Reader, Writeable, Writer, WriteableSorted, read_and_verify_sorted};
use global;
use keychain;
use keychain::BlindingFactor;
use util;
use util::kernel_sig_msg;
use util::LOGGER;
use util::{secp, static_secp_instance};

/// Errors thrown by Block validation
#[derive(Debug, Clone, PartialEq)]
pub enum Error {
	/// The sum of output minus input commitments does not
	/// match the sum of kernel commitments
	KernelSumMismatch,
	/// Same as above but for the coinbase part of a block, including reward
	CoinbaseSumMismatch,
	/// Kernel fee can't be odd, due to half fee burning
	OddKernelFee,
	/// Too many inputs, outputs or kernels in the block
	WeightExceeded,
	/// Kernel not valid due to lock_height exceeding block header height
	KernelLockHeight(u64),
	/// Underlying tx related error
	Transaction(transaction::Error),
	/// Underlying Secp256k1 error (signature validation or invalid public key typically)
	Secp(secp::Error),
	/// Underlying keychain related error
	Keychain(keychain::Error),
	/// Underlying consensus error (sort order currently)
	Consensus(consensus::Error),
	/// Coinbase has not yet matured and cannot be spent (1,000 blocks)
	ImmatureCoinbase {
		/// The height of the block containing the input spending the coinbase output
		height: u64,
		/// The lock_height needed to be reached for the coinbase output to mature
		lock_height: u64,
	},
	/// Other unspecified error condition
	Other(String)
}

impl From<transaction::Error> for Error {
	fn from(e: transaction::Error) -> Error {
		Error::Transaction(e)
	}
}

impl From<secp::Error> for Error {
	fn from(e: secp::Error) -> Error {
		Error::Secp(e)
	}
}

impl From<keychain::Error> for Error {
	fn from(e: keychain::Error) -> Error {
		Error::Keychain(e)
	}
}

impl From<consensus::Error> for Error {
	fn from(e: consensus::Error) -> Error {
		Error::Consensus(e)
	}
}

/// Block header, fairly standard compared to other blockchains.
#[derive(Clone, Debug, PartialEq)]
pub struct BlockHeader {
	/// Version of the block
	pub version: u16,
	/// Height of this block since the genesis block (height 0)
	pub height: u64,
	/// Hash of the block previous to this in the chain.
	pub previous: Hash,
	/// Timestamp at which the block was built.
	pub timestamp: time::Tm,
	/// Merklish root of all the commitments in the UTXO set
	pub utxo_root: Hash,
	/// Merklish root of all range proofs in the UTXO set
	pub range_proof_root: Hash,
	/// Merklish root of all transaction kernels in the UTXO set
	pub kernel_root: Hash,
	/// Nonce increment used to mine this block.
	pub nonce: u64,
	/// Proof of work data.
	pub pow: Proof,
	/// Difficulty used to mine the block.
	pub difficulty: Difficulty,
	/// Total accumulated difficulty since genesis block
	pub total_difficulty: Difficulty,
	/// The single aggregate "offset" that needs to be applied for all commitments to sum
	pub kernel_offset: BlindingFactor,
}

impl Default for BlockHeader {
	fn default() -> BlockHeader {
		let proof_size = global::proofsize();
		BlockHeader {
			version: 1,
			height: 0,
			previous: ZERO_HASH,
			timestamp: time::at_utc(time::Timespec { sec: 0, nsec: 0 }),
			difficulty: Difficulty::one(),
			total_difficulty: Difficulty::one(),
			utxo_root: ZERO_HASH,
			range_proof_root: ZERO_HASH,
			kernel_root: ZERO_HASH,
			nonce: 0,
			pow: Proof::zero(proof_size),
			kernel_offset: BlindingFactor::zero(),
		}
	}
}

/// Serialization of a block header
impl Writeable for BlockHeader {
	fn write<W: Writer>(&self, writer: &mut W) -> Result<(), ser::Error> {
		ser_multiwrite!(
			writer,
			[write_u16, self.version],
			[write_u64, self.height],
			[write_fixed_bytes, &self.previous],
			[write_i64, self.timestamp.to_timespec().sec],
			[write_fixed_bytes, &self.utxo_root],
			[write_fixed_bytes, &self.range_proof_root],
			[write_fixed_bytes, &self.kernel_root]
		);

		try!(writer.write_u64(self.nonce));
		try!(self.difficulty.write(writer));
		try!(self.total_difficulty.write(writer));
		try!(self.kernel_offset.write(writer));

		if writer.serialization_mode() != ser::SerializationMode::Hash {
			try!(self.pow.write(writer));
		}
		Ok(())
	}
}

/// Deserialization of a block header
impl Readable for BlockHeader {
	fn read(reader: &mut Reader) -> Result<BlockHeader, ser::Error> {
		let (version, height) = ser_multiread!(reader, read_u16, read_u64);
		let previous = Hash::read(reader)?;
		let timestamp = reader.read_i64()?;
		let utxo_root = Hash::read(reader)?;
		let rproof_root = Hash::read(reader)?;
		let kernel_root = Hash::read(reader)?;
		let nonce = reader.read_u64()?;
		let difficulty = Difficulty::read(reader)?;
		let total_difficulty = Difficulty::read(reader)?;
		let kernel_offset = BlindingFactor::read(reader)?;
		let pow = Proof::read(reader)?;

		Ok(BlockHeader {
			version: version,
			height: height,
			previous: previous,
			timestamp: time::at_utc(time::Timespec {
				sec: timestamp,
				nsec: 0,
			}),
			utxo_root: utxo_root,
			range_proof_root: rproof_root,
			kernel_root: kernel_root,
			pow: pow,
			nonce: nonce,
			difficulty: difficulty,
			total_difficulty: total_difficulty,
			kernel_offset: kernel_offset,
		})
	}
}

/// Compact representation of a full block.
/// Each input/output/kernel is represented as a short_id.
/// A node is reasonably likely to have already seen all tx data (tx broadcast before block)
/// and can go request missing tx data from peers if necessary to hydrate a compact block
/// into a full block.
#[derive(Debug, Clone)]
pub struct CompactBlock {
	/// The header with metadata and commitments to the rest of the data
	pub header: BlockHeader,
	/// Nonce for connection specific short_ids
	pub nonce: u64,
	/// List of full outputs - specifically the coinbase output(s)
	pub out_full: Vec<Output>,
	/// List of full kernels - specifically the coinbase kernel(s)
	pub kern_full: Vec<TxKernel>,
	/// List of transaction kernels, excluding those in the full list (short_ids)
	pub kern_ids: Vec<ShortId>,
}

/// Implementation of Writeable for a compact block, defines how to write the block to a
/// binary writer. Differentiates between writing the block for the purpose of
/// full serialization and the one of just extracting a hash.
/// Note: compact block hash uses both the header *and* the nonce.
impl Writeable for CompactBlock {
	fn write<W: Writer>(&self, writer: &mut W) -> Result<(), ser::Error> {
		try!(self.header.write(writer));
		try!(writer.write_u64(self.nonce));

		if writer.serialization_mode() != ser::SerializationMode::Hash {
			ser_multiwrite!(
				writer,
				[write_u64, self.out_full.len() as u64],
				[write_u64, self.kern_full.len() as u64],
				[write_u64, self.kern_ids.len() as u64]
			);

			let mut out_full = self.out_full.clone();
			let mut kern_full = self.kern_full.clone();
			let mut kern_ids = self.kern_ids.clone();

			// Consensus rule that everything is sorted in lexicographical order on the wire.
			try!(out_full.write_sorted(writer));
			try!(kern_full.write_sorted(writer));
			try!(kern_ids.write_sorted(writer));
		}
		Ok(())
	}
}

/// Implementation of Readable for a compact block, defines how to read a compact block
/// from a binary stream.
impl Readable for CompactBlock {
	fn read(reader: &mut Reader) -> Result<CompactBlock, ser::Error> {
		let header = try!(BlockHeader::read(reader));

		let (nonce, out_full_len, kern_full_len, kern_id_len) =
			ser_multiread!(reader, read_u64, read_u64, read_u64, read_u64);

		let out_full = read_and_verify_sorted(reader, out_full_len as u64)?;
		let kern_full = read_and_verify_sorted(reader, kern_full_len as u64)?;
		let kern_ids = read_and_verify_sorted(reader, kern_id_len)?;

		Ok(CompactBlock {
			header,
			nonce,
			out_full,
			kern_full,
			kern_ids,
		})
	}
}

/// A block as expressed in the MimbleWimble protocol. The reward is
/// non-explicit, assumed to be deducible from block height (similar to
/// bitcoin's schedule) and expressed as a global transaction fee (added v.H),
/// additive to the total of fees ever collected.
#[derive(Debug, Clone)]
pub struct Block {
	/// The header with metadata and commitments to the rest of the data
	pub header: BlockHeader,
	/// List of transaction inputs
	pub inputs: Vec<Input>,
	/// List of transaction outputs
	pub outputs: Vec<Output>,
	/// List of kernels with associated proofs (note these are offset from tx_kernels)
	pub kernels: Vec<TxKernel>,
}

/// Implementation of Writeable for a block, defines how to write the block to a
/// binary writer. Differentiates between writing the block for the purpose of
/// full serialization and the one of just extracting a hash.
impl Writeable for Block {
	fn write<W: Writer>(&self, writer: &mut W) -> Result<(), ser::Error> {
		try!(self.header.write(writer));

		if writer.serialization_mode() != ser::SerializationMode::Hash {
			ser_multiwrite!(
				writer,
				[write_u64, self.inputs.len() as u64],
				[write_u64, self.outputs.len() as u64],
				[write_u64, self.kernels.len() as u64]
			);

			let mut inputs = self.inputs.clone();
			let mut outputs = self.outputs.clone();
			let mut kernels = self.kernels.clone();

			// Consensus rule that everything is sorted in lexicographical order on the wire.
			try!(inputs.write_sorted(writer));
			try!(outputs.write_sorted(writer));
			try!(kernels.write_sorted(writer));
		}
		Ok(())
	}
}

/// Implementation of Readable for a block, defines how to read a full block
/// from a binary stream.
impl Readable for Block {
	fn read(reader: &mut Reader) -> Result<Block, ser::Error> {
		let header = try!(BlockHeader::read(reader));

		let (input_len, output_len, kernel_len) =
			ser_multiread!(reader, read_u64, read_u64, read_u64);

		let inputs = read_and_verify_sorted(reader, input_len)?;
		let outputs = read_and_verify_sorted(reader, output_len)?;
		let kernels = read_and_verify_sorted(reader, kernel_len)?;

		Ok(Block {
			header: header,
			inputs: inputs,
			outputs: outputs,
			kernels: kernels,
			..Default::default()
		})
	}
}

/// Provides all information from a block that allows the calculation of total
/// Pedersen commitment.
impl Committed for Block {
	fn inputs_committed(&self) -> &Vec<Input> {
		&self.inputs
	}
	fn outputs_committed(&self) -> &Vec<Output> {
		&self.outputs
	}
	fn overage(&self) -> i64 {
		((self.total_fees() / 2) as i64) - (REWARD as i64)
	}
}

/// Default properties for a block, everything zeroed out and empty vectors.
impl Default for Block {
	fn default() -> Block {
		Block {
			header: Default::default(),
			inputs: vec![],
			outputs: vec![],
			kernels: vec![],
		}
	}
}

impl Block {
	/// Builds a new block from the header of the previous block, a vector of
	/// transactions and the private key that will receive the reward. Checks
	/// that all transactions are valid and calculates the Merkle tree.
	///
	/// Only used in tests (to be confirmed, may be wrong here).
	///
	pub fn new(
		prev: &BlockHeader,
		txs: Vec<&Transaction>,
		keychain: &keychain::Keychain,
		key_id: &keychain::Identifier,
		difficulty: Difficulty,
	) -> Result<Block, Error> {
		let fees = txs.iter().map(|tx| tx.fee()).sum();
		let (reward_out, reward_proof) = Block::reward_output(
			keychain,
			key_id,
			fees,
			prev.height + 1,
		)?;
		let block = Block::with_reward(prev, txs, reward_out, reward_proof, difficulty)?;
		Ok(block)
	}

	/// Hydrate a block from a compact block.
	/// Note: caller must validate the block themselves, we do not validate it here.
	pub fn hydrate_from(cb: CompactBlock, txs: Vec<Transaction>) -> Block {
		debug!(
			LOGGER,
			"block: hydrate_from: {}, {} txs",
			cb.hash(),
			txs.len(),
		);

		let mut all_inputs = HashSet::new();
		let mut all_outputs = HashSet::new();
		let mut all_kernels = HashSet::new();

		// collect all the inputs, outputs and kernels from the txs
		for tx in txs {
			all_inputs.extend(tx.inputs);
			all_outputs.extend(tx.outputs);
			all_kernels.extend(tx.kernels);
		}

		// include the coinbase output(s) and kernel(s) from the compact_block
		all_outputs.extend(cb.out_full);
		all_kernels.extend(cb.kern_full);

		// convert the sets to vecs
		let mut all_inputs = all_inputs.iter().cloned().collect::<Vec<_>>();
		let mut all_outputs = all_outputs.iter().cloned().collect::<Vec<_>>();
		let mut all_kernels = all_kernels.iter().cloned().collect::<Vec<_>>();

		// sort them all lexicographically
		all_inputs.sort();
		all_outputs.sort();
		all_kernels.sort();

		// finally return the full block
		// Note: we have not actually validated the block here
		// leave it to the caller to actually validate the block
		Block {
			header: cb.header,
			inputs: all_inputs,
			outputs: all_outputs,
			kernels: all_kernels,
		}.cut_through()
	}

	/// Generate the compact block representation.
	pub fn as_compact_block(&self) -> CompactBlock {
		let header = self.header.clone();
		let nonce = thread_rng().next_u64();

		// concatenate the nonce with our block_header to build the hash
		let hash = (self, nonce).hash();

		let mut out_full = self.outputs
			.iter()
			.filter(|x| x.features.contains(OutputFeatures::COINBASE_OUTPUT))
			.cloned()
			.collect::<Vec<_>>();

		let mut kern_full = vec![];
		let mut kern_ids = vec![];

		for k in &self.kernels {
			if k.features.contains(KernelFeatures::COINBASE_KERNEL) {
				kern_full.push(k.clone());
			} else {
				kern_ids.push(k.short_id(&hash));
			}
		}

		// sort all the lists
		out_full.sort();
		kern_full.sort();
		kern_ids.sort();

		CompactBlock {
			header,
			nonce,
			out_full,
			kern_full,
			kern_ids,
		}
	}

	/// Builds a new block ready to mine from the header of the previous block,
	/// a vector of transactions and the reward information. Checks
	/// that all transactions are valid and calculates the Merkle tree.
	pub fn with_reward(
		prev: &BlockHeader,
		txs: Vec<&Transaction>,
		reward_out: Output,
		reward_kern: TxKernel,
		difficulty: Difficulty,
	) -> Result<Block, Error> {
		let mut kernels = vec![];
		let mut inputs = vec![];
		let mut outputs = vec![];

		// we will sum these together at the end
		// to give us the overall offset for the block
		let mut kernel_offsets = vec![];

		// iterate over the all the txs
		// build the kernel for each
		// and collect all the kernels, inputs and outputs
		// to build the block (which we can sort of think of as one big tx?)
		for tx in txs {
			// validate each transaction and gather their kernels
			// tx has an offset k2 where k = k1 + k2
			// and the tx is signed using k1
			// the kernel excess is k1G
			// we will sum all the offsets later and store the total offset
			// on the block_header
			tx.validate()?;

			// we will summ these later to give a single aggregate offset
			kernel_offsets.push(tx.offset);

			// add all tx inputs/outputs/kernels to the block
			kernels.extend(tx.kernels.iter().cloned());
			inputs.extend(tx.inputs.iter().cloned());
			outputs.extend(tx.outputs.iter().cloned());
		}

		// include the reward kernel and output
		kernels.push(reward_kern);
		outputs.push(reward_out);

		// now sort everything so the block is built deterministically
		inputs.sort();
		outputs.sort();
		kernels.sort();

		// now sum the kernel_offsets up to give us
		// an aggregate offset for the entire block
		let kernel_offset = {
			let secp = static_secp_instance();
			let secp = secp.lock().unwrap();
			let keys = kernel_offsets
				.iter()
				.cloned()
				.filter(|x| *x != BlindingFactor::zero())
				.filter_map(|x| {
					x.secret_key(&secp).ok()
				})
				.collect::<Vec<_>>();
			if keys.is_empty() {
				BlindingFactor::zero()
			} else {
				let sum = secp.blind_sum(keys, vec![])?;

				BlindingFactor::from_secret_key(sum)
			}
		};

		Ok(
			Block {
				header: BlockHeader {
					height: prev.height + 1,
					timestamp: time::Tm {
						tm_nsec: 0,
						..time::now_utc()
					},
					previous: prev.hash(),
					total_difficulty: difficulty +
						prev.total_difficulty.clone(),
					kernel_offset: kernel_offset,
					..Default::default()
				},
				inputs: inputs,
				outputs: outputs,
				kernels: kernels,
			}.cut_through(),
		)
	}

	/// Blockhash, computed using only the header
	pub fn hash(&self) -> Hash {
		self.header.hash()
	}

	/// Sum of all fees (inputs less outputs) in the block
	pub fn total_fees(&self) -> u64 {
		self.kernels.iter().map(|p| p.fee).sum()
	}

	/// Matches any output with a potential spending input, eliminating them
	/// from the block. Provides a simple way to cut-through the block. The
	/// elimination is stable with respect to the order of inputs and outputs.
	///
	/// NOTE: exclude coinbase from cut-through process
	/// if a block contains a new coinbase output and
	/// is a transaction spending a previous coinbase
	/// we do not want to cut-through (all coinbase must be preserved)
	///
	pub fn cut_through(&self) -> Block {
		let in_set = self.inputs
			.iter()
			.map(|inp| inp.commitment())
			.collect::<HashSet<_>>();

		let out_set = self.outputs
			.iter()
			.filter(|out| !out.features.contains(OutputFeatures::COINBASE_OUTPUT))
			.map(|out| out.commitment())
			.collect::<HashSet<_>>();

		let to_cut_through = in_set.intersection(&out_set).collect::<HashSet<_>>();

		let new_inputs = self.inputs
			.iter()
			.filter(|inp| !to_cut_through.contains(&inp.commitment()))
			.map(|&inp| inp)
			.collect::<Vec<_>>();

		let new_outputs = self.outputs
			.iter()
			.filter(|out| !to_cut_through.contains(&out.commitment()))
			.map(|&out| out)
			.collect::<Vec<_>>();

		Block {
			header: BlockHeader {
				pow: self.header.pow.clone(),
				difficulty: self.header.difficulty.clone(),
				total_difficulty: self.header.total_difficulty.clone(),
				..self.header
			},
			inputs: new_inputs,
			outputs: new_outputs,
			kernels: self.kernels.clone(),
		}
	}

	/// Validates all the elements in a block that can be checked without
	/// additional data. Includes commitment sums and kernels, Merkle
	/// trees, reward, etc.
	pub fn validate(&self) -> Result<(), Error> {
		self.verify_weight()?;
		self.verify_sorted()?;
		self.verify_coinbase()?;
		self.verify_kernels()?;
		Ok(())
	}

	fn verify_weight(&self) -> Result<(), Error> {
		if exceeds_weight(self.inputs.len(), self.outputs.len(), self.kernels.len()) {
			return Err(Error::WeightExceeded);
		}
		Ok(())
	}

	fn verify_sorted(&self) -> Result<(), Error> {
		self.inputs.verify_sort_order()?;
		self.outputs.verify_sort_order()?;
		self.kernels.verify_sort_order()?;
		Ok(())
	}

	/// Verifies the sum of input/output commitments match the sum in kernels
	/// and that all kernel signatures are valid.
	fn verify_kernels(&self) -> Result<(), Error> {
		for k in &self.kernels {
			if k.fee & 1 != 0 {
				return Err(Error::OddKernelFee);
			}

			// check we have no kernels with lock_heights greater than current height
			// no tx can be included in a block earlier than its lock_height
			if k.lock_height > self.header.height {
				return Err(Error::KernelLockHeight(k.lock_height));
			}
		}

		// sum all inputs and outs commitments
		let io_sum = self.sum_commitments()?;

		// sum all kernels commitments
		let kernel_sum = {
			let mut kernel_commits = self.kernels
				.iter()
				.map(|x| x.excess)
				.collect::<Vec<_>>();

			let secp = static_secp_instance();
			let secp = secp.lock().unwrap();

			// add the kernel_offset in as necessary (unless offset is zero)
			if self.header.kernel_offset != BlindingFactor::zero() {
				let skey = self.header.kernel_offset.secret_key(&secp)?;
				let offset_commit = secp.commit(0, skey)?;
				kernel_commits.push(offset_commit);
			}

			secp.commit_sum(kernel_commits, vec![])?
		};

		// sum of kernel commitments (including kernel_offset) must match
		// the sum of input/output commitments (minus fee)
		if kernel_sum != io_sum {
			return Err(Error::KernelSumMismatch);
		}

		// verify all signatures with the commitment as pk
		for kernel in &self.kernels {
			kernel.verify()?;
		}

		Ok(())
	}

	// Validate the coinbase outputs generated by miners. Entails 2 main checks:
	//
	// * That the sum of all coinbase-marked outputs equal the supply.
	// * That the sum of blinding factors for all coinbase-marked outputs match
	//   the coinbase-marked kernels.
	fn verify_coinbase(&self) -> Result<(), Error> {
		let cb_outs = self.outputs
			.iter()
			.filter(|out| out.features.contains(OutputFeatures::COINBASE_OUTPUT))
			.cloned()
			.collect::<Vec<Output>>();

		let cb_kerns = self.kernels
			.iter()
			.filter(|kernel| kernel.features.contains(KernelFeatures::COINBASE_KERNEL))
			.cloned()
			.collect::<Vec<TxKernel>>();

		let over_commit;
		let out_adjust_sum;
		let kerns_sum;
		{
			let secp = static_secp_instance();
			let secp = secp.lock().unwrap();
			over_commit = secp.commit_value(reward(self.total_fees()))?;
			out_adjust_sum = secp.commit_sum(
				cb_outs.iter().map(|x| x.commitment()).collect(),
				vec![over_commit],
			)?;
			kerns_sum = secp.commit_sum(
				cb_kerns.iter().map(|x| x.excess).collect(),
				vec![],
			)?;
		}

		if kerns_sum != out_adjust_sum {
			return Err(Error::CoinbaseSumMismatch);
		}
		Ok(())
	}

	/// NOTE: this happens during apply_block (not the earlier validate_block)
	///
	/// Calculate lock_height as block_height + 1,000
	/// Confirm height <= lock_height
	pub fn verify_coinbase_maturity(
		&self,
		input: &Input,
		height: u64,
	) -> Result<(), Error> {
		let output = OutputIdentifier::from_input(&input);

		// We should only be calling verify_coinbase_maturity
		// if the sender claims we are spending a coinbase output
		// _and_ that we trust this claim.
		// We should have already confirmed the entry from the MMR exists
		// and has the expected hash.
		assert!(output.features.contains(OutputFeatures::COINBASE_OUTPUT));

		if let Some(_) = self.outputs
			.iter()
			.find(|x| OutputIdentifier::from_output(&x) == output)
		{
			let lock_height = self.header.height + global::coinbase_maturity();
			if lock_height > height {
				Err(Error::ImmatureCoinbase{
					height: height,
					lock_height: lock_height,
				})
			} else {
				Ok(())
			}
		} else {
			Err(Error::Other(format!("output not found in block")))
		}
	}

	/// Builds the blinded output and related signature proof for the block reward.
	pub fn reward_output(
		keychain: &keychain::Keychain,
		key_id: &keychain::Identifier,
		fees: u64,
		height: u64,
	) -> Result<(Output, TxKernel), keychain::Error> {
		let commit = keychain.commit(reward(fees), key_id)?;
		let switch_commit = keychain.switch_commit(key_id)?;
		let switch_commit_hash = SwitchCommitHash::from_switch_commit(
			switch_commit,
			keychain,
			key_id,
		);

		trace!(
			LOGGER,
			"Block reward - Pedersen Commit is: {:?}, Switch Commit is: {:?}",
			commit,
			switch_commit
		);
		trace!(
			LOGGER,
			"Block reward - Switch Commit Hash is: {:?}",
			switch_commit_hash
		);
		let msg = util::secp::pedersen::ProofMessage::empty();
		let rproof = keychain.range_proof(reward(fees), key_id, commit, msg)?;

		let output = Output {
			features: OutputFeatures::COINBASE_OUTPUT,
			commit: commit,
			switch_commit_hash: switch_commit_hash,
			proof: rproof,
		};

		let secp = static_secp_instance();
		let secp = secp.lock().unwrap();
		let over_commit = secp.commit_value(reward(fees))?;
		let out_commit = output.commitment();
		let excess = secp.commit_sum(vec![out_commit], vec![over_commit])?;

		// NOTE: Remember we sign the fee *and* the lock_height.
		// For a coinbase output the fee is 0 and the lock_height is
		// the lock_height of the coinbase output itself,
		// not the lock_height of the tx (there is no tx for a coinbase output).
		// This output will not be spendable earlier than lock_height (and we sign this here).
		let msg = secp::Message::from_slice(&kernel_sig_msg(0, height))?;
		let sig = keychain.aggsig_sign_from_key_id(&msg, &key_id)?;

		let proof = TxKernel {
			features: KernelFeatures::COINBASE_KERNEL,
			excess: excess,
			excess_sig: sig,
			fee: 0,
			// lock_height here is the height of the block (tx should be valid immediately)
			// *not* the lock_height of the coinbase output (only spendable 1,000 blocks later)
			lock_height: height,
		};
		Ok((output, proof))
	}
}

#[cfg(test)]
mod test {
	use super::*;
	use core::hash::ZERO_HASH;
	use core::Transaction;
	use core::build::{self, input, output, with_fee};
	use core::test::{tx1i2o, tx2i1o};
	use keychain::{Identifier, Keychain};
	use consensus::{MAX_BLOCK_WEIGHT, BLOCK_OUTPUT_WEIGHT};
	use std::time::Instant;

	use util::secp;

	// utility to create a block without worrying about the key or previous
	// header
	fn new_block(txs: Vec<&Transaction>, keychain: &Keychain) -> Block {
		let key_id = keychain.derive_key_id(1).unwrap();
		Block::new(
			&BlockHeader::default(),
			txs,
			keychain,
			&key_id,
			Difficulty::one()
		).unwrap()
	}

	// utility producing a transaction that spends an output with the provided
	// value and blinding key
	fn txspend1i1o(
		v: u64,
		keychain: &Keychain,
		key_id1: Identifier,
		key_id2: Identifier,
	) -> Transaction {
		build::transaction(
			vec![input(v, ZERO_HASH, key_id1), output(3, key_id2), with_fee(2)],
			&keychain,
		).unwrap()
	}

	// Too slow for now #[test]
	// TODO: make this fast enough or add similar but faster test?
	#[allow(dead_code)]
	fn too_large_block() {
		let keychain = Keychain::from_random_seed().unwrap();
		let max_out = MAX_BLOCK_WEIGHT / BLOCK_OUTPUT_WEIGHT;

		let mut pks = vec![];
		for n in 0..(max_out + 1) {
			pks.push(keychain.derive_key_id(n as u32).unwrap());
		}

		let mut parts = vec![];
		for _ in 0..max_out {
			parts.push(output(5, pks.pop().unwrap()));
		}

		let now = Instant::now();
		parts.append(&mut vec![input(500000, ZERO_HASH, pks.pop().unwrap()), with_fee(2)]);
		let mut tx = build::transaction(parts, &keychain)
			.unwrap();
		println!("Build tx: {}", now.elapsed().as_secs());

		let b = new_block(vec![&mut tx], &keychain);
		assert!(b.validate().is_err());
	}

	#[test]
	// block with no inputs/outputs/kernels
	// no fees, no reward, no coinbase
	fn very_empty_block() {
		let b = Block {
			header: BlockHeader::default(),
			inputs: vec![],
			outputs: vec![],
			kernels: vec![],
		};

		assert_eq!(
			b.verify_coinbase(),
			Err(Error::Secp(secp::Error::IncorrectCommitSum))
		);

	}

	#[test]
	// builds a block with a tx spending another and check that cut_through occurred
	fn block_with_cut_through() {
		let keychain = Keychain::from_random_seed().unwrap();
		let key_id1 = keychain.derive_key_id(1).unwrap();
		let key_id2 = keychain.derive_key_id(2).unwrap();
		let key_id3 = keychain.derive_key_id(3).unwrap();

		let mut btx1 = tx2i1o();
		let mut btx2 = build::transaction(
			vec![input(7, ZERO_HASH, key_id1), output(5, key_id2.clone()), with_fee(2)],
			&keychain,
		).unwrap();

		// spending tx2 - reuse key_id2

		let mut btx3 = txspend1i1o(5, &keychain, key_id2.clone(), key_id3);
		let b = new_block(vec![&mut btx1, &mut btx2, &mut btx3], &keychain);

		// block should have been automatically compacted (including reward
		// output) and should still be valid
		b.validate().unwrap();
		assert_eq!(b.inputs.len(), 3);
		assert_eq!(b.outputs.len(), 3);
	}

	#[test]
	fn empty_block_with_coinbase_is_valid() {
		let keychain = Keychain::from_random_seed().unwrap();
		let b = new_block(vec![], &keychain);

		assert_eq!(b.inputs.len(), 0);
		assert_eq!(b.outputs.len(), 1);
		assert_eq!(b.kernels.len(), 1);

		let coinbase_outputs = b.outputs
			.iter()
			.filter(|out| out.features.contains(OutputFeatures::COINBASE_OUTPUT))
			.map(|o| o.clone())
			.collect::<Vec<_>>();
		assert_eq!(coinbase_outputs.len(), 1);

		let coinbase_kernels = b.kernels
			.iter()
			.filter(|out| out.features.contains(KernelFeatures::COINBASE_KERNEL))
			.map(|o| o.clone())
			.collect::<Vec<_>>();
		assert_eq!(coinbase_kernels.len(), 1);

		// the block should be valid here (single coinbase output with corresponding
		// txn kernel)
		assert_eq!(b.validate(), Ok(()));
	}

	#[test]
	// test that flipping the COINBASE_OUTPUT flag on the output features
	// invalidates the block and specifically it causes verify_coinbase to fail
	// additionally verifying the merkle_inputs_outputs also fails
	fn remove_coinbase_output_flag() {
		let keychain = Keychain::from_random_seed().unwrap();
		let mut b = new_block(vec![], &keychain);

		assert!(b.outputs[0].features.contains(OutputFeatures::COINBASE_OUTPUT));
		b.outputs[0].features.remove(OutputFeatures::COINBASE_OUTPUT);

		assert_eq!(
			b.verify_coinbase(),
			Err(Error::CoinbaseSumMismatch)
		);
		assert_eq!(b.verify_kernels(), Ok(()));

		assert_eq!(
			b.validate(),
			Err(Error::CoinbaseSumMismatch)
		);
	}

	#[test]
	// test that flipping the COINBASE_KERNEL flag on the kernel features
	// invalidates the block and specifically it causes verify_coinbase to fail
	fn remove_coinbase_kernel_flag() {
		let keychain = Keychain::from_random_seed().unwrap();
		let mut b = new_block(vec![], &keychain);

		assert!(b.kernels[0].features.contains(KernelFeatures::COINBASE_KERNEL));
		b.kernels[0].features.remove(KernelFeatures::COINBASE_KERNEL);

		assert_eq!(
			b.verify_coinbase(),
			Err(Error::Secp(secp::Error::IncorrectCommitSum))
		);

		assert_eq!(
			b.validate(),
			Err(Error::Secp(secp::Error::IncorrectCommitSum))
		);
	}

	#[test]
	fn serialize_deserialize_block() {
		let keychain = Keychain::from_random_seed().unwrap();
		let b = new_block(vec![], &keychain);

		let mut vec = Vec::new();
		ser::serialize(&mut vec, &b).expect("serialization failed");
		let b2: Block = ser::deserialize(&mut &vec[..]).unwrap();

		assert_eq!(b.header, b2.header);
		assert_eq!(b.inputs, b2.inputs);
		assert_eq!(b.outputs, b2.outputs);
		assert_eq!(b.kernels, b2.kernels);
	}

	#[test]
	fn empty_block_serialized_size() {
		let keychain = Keychain::from_random_seed().unwrap();
		let b = new_block(vec![], &keychain);
		let mut vec = Vec::new();
		ser::serialize(&mut vec, &b).expect("serialization failed");
		let target_len = match Keychain::is_using_bullet_proofs() {
			true => 1_256,
			false => 5_708,
		};
		assert_eq!(
			vec.len(),
			target_len,
		);
	}

	#[test]
	fn block_single_tx_serialized_size() {
		let keychain = Keychain::from_random_seed().unwrap();
		let tx1 = tx1i2o();
		let b = new_block(vec![&tx1], &keychain);
		let mut vec = Vec::new();
		ser::serialize(&mut vec, &b).expect("serialization failed");
		let target_len = match Keychain::is_using_bullet_proofs() {
			true => 2_900,
			false => 16_256,
		};
		assert_eq!(
			vec.len(),
			target_len,
		);
	}

	#[test]
	fn empty_compact_block_serialized_size() {
		let keychain = Keychain::from_random_seed().unwrap();
		let b = new_block(vec![], &keychain);
		let mut vec = Vec::new();
		ser::serialize(&mut vec, &b.as_compact_block()).expect("serialization failed");
		let target_len = match Keychain::is_using_bullet_proofs() {
			true => 1_264,
			false => 5_716,
		};
		assert_eq!(
			vec.len(),
			target_len,
		);
	}

	#[test]
	fn compact_block_single_tx_serialized_size() {
		let keychain = Keychain::from_random_seed().unwrap();
		let tx1 = tx1i2o();
		let b = new_block(vec![&tx1], &keychain);
		let mut vec = Vec::new();
		ser::serialize(&mut vec, &b.as_compact_block()).expect("serialization failed");
		let target_len = match Keychain::is_using_bullet_proofs() {
			true => 1_270,
			false => 5_722,
		};
		assert_eq!(
			vec.len(),
			target_len,
		);
	}

	#[test]
	fn block_10_tx_serialized_size() {
		let keychain = Keychain::from_random_seed().unwrap();

		let mut txs = vec![];
		for _ in 0..10 {
			let tx = tx1i2o();
			txs.push(tx);
		}

		let b = new_block(
			txs.iter().collect(),
			&keychain,
		);
		let mut vec = Vec::new();
		ser::serialize(&mut vec, &b).expect("serialization failed");
		let target_len = match Keychain::is_using_bullet_proofs() {
			true => 17696,
			false => 111188,
		};
		assert_eq!(
			vec.len(),
			target_len,
		);
	}

	#[test]
	fn compact_block_10_tx_serialized_size() {
		let keychain = Keychain::from_random_seed().unwrap();

		let mut txs = vec![];
		for _ in 0..10 {
			let tx = tx1i2o();
			txs.push(tx);
		}

		let b = new_block(
			txs.iter().collect(),
			&keychain,
		);
		let mut vec = Vec::new();
		ser::serialize(&mut vec, &b.as_compact_block()).expect("serialization failed");
		let target_len = match Keychain::is_using_bullet_proofs() {
			true => 1_324,
			false => 5_776,
		};
		assert_eq!(
			vec.len(),
			target_len,
		);
	}

	#[test]
	fn compact_block_hash_with_nonce() {
		let keychain = Keychain::from_random_seed().unwrap();
		let tx = tx1i2o();
		let b = new_block(vec![&tx], &keychain);
		let cb1 = b.as_compact_block();
		let cb2 = b.as_compact_block();

		// random nonce included in hash each time we generate a compact_block
		// so the hash will always be unique (we use this to generate unique short_ids)
		assert!(cb1.hash() != cb2.hash());

		assert!(cb1.kern_ids[0] != cb2.kern_ids[0]);

		// check we can identify the specified kernel from the short_id
		// in either of the compact_blocks
		assert_eq!(cb1.kern_ids[0], tx.kernels[0].short_id(&cb1.hash()));
		assert_eq!(cb2.kern_ids[0], tx.kernels[0].short_id(&cb2.hash()));
	}

	#[test]
	fn convert_block_to_compact_block() {
		let keychain = Keychain::from_random_seed().unwrap();
		let tx1 = tx1i2o();
		let b = new_block(vec![&tx1], &keychain);
		let cb = b.as_compact_block();

		assert_eq!(cb.out_full.len(), 1);
		assert_eq!(cb.kern_full.len(), 1);
		assert_eq!(cb.kern_ids.len(), 1);

		assert_eq!(
			cb.kern_ids[0],
			b.kernels
				.iter()
				.find(|x| !x.features.contains(KernelFeatures::COINBASE_KERNEL))
				.unwrap()
				.short_id(&cb.hash())
		);
	}

	#[test]
	fn hydrate_empty_compact_block() {
		let keychain = Keychain::from_random_seed().unwrap();
		let b = new_block(vec![], &keychain);
		let cb = b.as_compact_block();
		let hb = Block::hydrate_from(cb, vec![]);
		assert_eq!(hb.header, b.header);
		assert_eq!(hb.outputs, b.outputs);
		assert_eq!(hb.kernels, b.kernels);
	}

	#[test]
	fn serialize_deserialize_compact_block() {
		let b = CompactBlock {
			header: BlockHeader::default(),
			nonce: 0,
			out_full: vec![],
			kern_full: vec![],
			kern_ids: vec![ShortId::zero()],
		};

		let mut vec = Vec::new();
		ser::serialize(&mut vec, &b).expect("serialization failed");
		let b2: CompactBlock = ser::deserialize(&mut &vec[..]).unwrap();

		assert_eq!(b.header, b2.header);
		assert_eq!(b.kern_ids, b2.kern_ids);
	}
}
