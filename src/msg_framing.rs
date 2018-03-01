use bitcoin::blockdata::transaction::{TxOut,Transaction};
use bitcoin::blockdata::script::Script;
use bitcoin::util::hash::Sha256dHash;
use bitcoin::network;

use bytes;
use bytes::BufMut;

use futures::future::Future;
use futures::{future,Stream,Sink};
use futures::unsync::mpsc;

use tokio::executor::current_thread;
use tokio::net;

use tokio_io::AsyncRead;
use tokio_io::codec;

use tokio_timer::Timer;

use secp256k1::key::PublicKey;
use secp256k1::Secp256k1;
use secp256k1::Signature;

use std::cell::RefCell;
use std::error::Error;
use std::net::{SocketAddr,ToSocketAddrs};
use std::fmt;
use std::io;
use std::marker;
use std::rc::Rc;
use std::time::Duration;

#[derive(Clone)]
pub struct BlockTemplate {
	pub template_id: u64,
	pub target: [u8; 32],

	pub header_version: u32,
	pub header_prevblock: [u8; 32],
	pub header_time: u32,
	pub header_nbits: u32,

	pub merkle_rhss: Vec<[u8; 32]>,
	pub coinbase_value_remaining: u64,

	pub coinbase_version: u32,
	pub coinbase_prefix: Vec<u8>,
	pub coinbase_input_sequence: u32,
	pub appended_coinbase_outputs: Vec<TxOut>,
	pub coinbase_locktime: u32,
}
impl BlockTemplate {
	pub fn encode_unsigned(&self, res: &mut bytes::BytesMut) {
		res.reserve(850); // Round upper bound assuming 2, 33-byte-sPK outputs
		res.put_u64::<bytes::LittleEndian>(self.template_id);
		res.put_slice(&self.target);

		res.put_u32::<bytes::LittleEndian>(self.header_version);
		res.put_slice(&self.header_prevblock);
		res.put_u32::<bytes::LittleEndian>(self.header_time);
		res.put_u32::<bytes::LittleEndian>(self.header_nbits);

		res.put_u8(self.merkle_rhss.len() as u8);
		for merkle_rhs in self.merkle_rhss.iter() {
			res.put_slice(merkle_rhs);
		}
		res.put_u64::<bytes::LittleEndian>(self.coinbase_value_remaining);

		res.put_u32::<bytes::LittleEndian>(self.coinbase_version);
		res.put_u8(self.coinbase_prefix.len() as u8);
		res.put_slice(&self.coinbase_prefix[..]);
		res.put_u32::<bytes::LittleEndian>(self.coinbase_input_sequence);
		res.put_u8(self.appended_coinbase_outputs.len() as u8);
		for txout in self.appended_coinbase_outputs.iter() {
			if res.remaining_mut() < 8 + 2 + txout.script_pubkey.len() + 8 {
				res.reserve(8 + 2 + txout.script_pubkey.len() + 8);
			}
			res.put_u64::<bytes::LittleEndian>(txout.value);
			res.put_u16::<bytes::LittleEndian>(txout.script_pubkey.len() as u16);
			res.put_slice(&txout.script_pubkey[..]);
		}
		res.put_u32::<bytes::LittleEndian>(self.coinbase_locktime);
	}
}

#[derive(Clone)]
pub struct CoinbasePrefixPostfix {
	pub timestamp: u64,
	pub coinbase_prefix_postfix: Vec<u8>,
}
impl CoinbasePrefixPostfix {
	pub fn encode_unsigned(&self, res: &mut bytes::BytesMut) {
		res.reserve(8 + 1 + self.coinbase_prefix_postfix.len());
		res.put_u64::<bytes::LittleEndian>(self.timestamp);
		res.put_u8(self.coinbase_prefix_postfix.len() as u8);
		res.put_slice(&self.coinbase_prefix_postfix[..]);
	}
}

#[derive(Clone)]
pub struct WinningNonce {
	pub template_id: u64,
	pub header_version: u32,
	pub header_time: u32,
	pub header_nonce: u32,
	pub coinbase_tx: Transaction,
}
impl WinningNonce {
	pub fn encode(&self, res: &mut bytes::BytesMut) {
		let tx_enc = network::serialize::serialize(&self.coinbase_tx).unwrap();
		res.reserve(8 + 4*4 + tx_enc.len());
		res.put_u64::<bytes::LittleEndian>(self.template_id);
		res.put_u32::<bytes::LittleEndian>(self.header_version);
		res.put_u32::<bytes::LittleEndian>(self.header_time);
		res.put_u32::<bytes::LittleEndian>(self.header_nonce);
		res.put_u32::<bytes::LittleEndian>(tx_enc.len() as u32);
		res.put_slice(&tx_enc[..]);
	}
}

pub struct TransactionData {
	pub template_id: u64,
	pub transactions: Vec<Transaction>,
}
impl TransactionData {
	pub fn encode_unsigned(&self, res: &mut bytes::BytesMut) {
		res.reserve(8+4);
		res.put_u64::<bytes::LittleEndian>(self.template_id);
		res.put_u32::<bytes::LittleEndian>(self.transactions.len() as u32);
		for tx in self.transactions.iter() {
			let tx_enc = network::serialize::serialize(tx).unwrap();
			res.reserve(4 + tx_enc.len());
			res.put_u32::<bytes::LittleEndian>(tx_enc.len() as u32);
			res.put_slice(&tx_enc[..]);
		}
	}
}

pub struct WorkInfo {
	pub template: Rc<BlockTemplate>,
	pub solutions: mpsc::UnboundedSender<Rc<(WinningNonce, Sha256dHash)>>,
}

pub enum WorkMessage {
	ProtocolSupport {
		max_version: u16,
		min_version: u16,
		flags: u16,
	},
	ProtocolVersion {
		selected_version: u16,
		auth_key: PublicKey,
	},
	BlockTemplate {
		signature: Signature,
		template: BlockTemplate,
	},
	WinningNonce {
		nonces: WinningNonce,
	},
	TransactionDataRequest {
		template_id: u64,
	},
	TransactionData {
		signature: Signature,
		data: TransactionData,
	},
	CoinbasePrefixPostfix {
		signature: Signature,
		coinbase_prefix_postfix: CoinbasePrefixPostfix,
	},
}

pub struct WorkMsgFramer {
	secp_ctx: Secp256k1,
}

impl WorkMsgFramer {
	pub fn new() -> WorkMsgFramer {
		WorkMsgFramer {
			secp_ctx: Secp256k1::new(),
		}
	}
}

#[derive(Debug)]
struct CodecError;
impl fmt::Display for CodecError {
	fn fmt(&self, fmt: &mut fmt::Formatter) -> Result<(), fmt::Error> {
		fmt.write_str("Bad data")
	}
}
impl Error for CodecError {
	fn description(&self) -> &str {
		"Bad data"
	}
}

impl codec::Encoder for WorkMsgFramer {
	type Item = WorkMessage;
	type Error = io::Error;

	fn encode(&mut self, msg: WorkMessage, res: &mut bytes::BytesMut) -> Result<(), io::Error> {
		match msg {
			WorkMessage::ProtocolSupport { max_version, min_version, flags } => {
				res.reserve(1 + 2*3);
				res.put_u8(1);
				res.put_u16::<bytes::LittleEndian>(max_version);
				res.put_u16::<bytes::LittleEndian>(min_version);
				res.put_u16::<bytes::LittleEndian>(flags);
			},
			WorkMessage::ProtocolVersion { selected_version, ref auth_key } => {
				res.reserve(1 + 2 + 33);
				res.put_u8(2);
				res.put_u16::<bytes::LittleEndian>(selected_version);
				res.put_slice(&auth_key.serialize());
			},
			WorkMessage::BlockTemplate { ref signature, ref template } => {
				res.reserve(1 + 33);
				res.put_u8(3);
				res.put_slice(&signature.serialize_compact(&self.secp_ctx));
				template.encode_unsigned(res);
			},
			WorkMessage::WinningNonce { ref nonces } => {
				res.reserve(1);
				res.put_u8(4);
				nonces.encode(res);
			},
			WorkMessage::TransactionDataRequest { template_id } => {
				res.reserve(1 + 8);
				res.put_u8(5);
				res.put_u64::<bytes::LittleEndian>(template_id);
			}
			WorkMessage::TransactionData { ref signature, ref data } => {
				res.reserve(1 + 33);
				res.put_u8(6);
				res.put_slice(&signature.serialize_compact(&self.secp_ctx));
				data.encode_unsigned(res);
			},
			WorkMessage::CoinbasePrefixPostfix { ref signature, ref coinbase_prefix_postfix } => {
				res.reserve(1 + 33);
				res.put_u8(7);
				res.put_slice(&signature.serialize_compact(&self.secp_ctx));
				coinbase_prefix_postfix.encode_unsigned(res);
			},
		}
		Ok(())
	}
}

#[inline]
fn slice_to_le16(v: &[u8]) -> u16 {
	((v[1] as u16) << 8*1) |
	((v[0] as u16) << 8*0)
}

#[inline]
fn slice_to_le32(v: &[u8]) -> u32 {
	((v[3] as u32) << 8*3) |
	((v[2] as u32) << 8*2) |
	((v[1] as u32) << 8*1) |
	((v[0] as u32) << 8*0)
}

#[inline]
fn slice_to_le64(v: &[u8]) -> u64 {
	((v[7] as u64) << 8*7) |
	((v[6] as u64) << 8*6) |
	((v[5] as u64) << 8*5) |
	((v[4] as u64) << 8*4) |
	((v[3] as u64) << 8*3) |
	((v[2] as u64) << 8*2) |
	((v[1] as u64) << 8*1) |
	((v[0] as u64) << 8*0)
}

impl codec::Decoder for WorkMsgFramer {
	type Item = WorkMessage;
	type Error = io::Error;

	fn decode(&mut self, bytes: &mut bytes::BytesMut) -> Result<Option<WorkMessage>, io::Error> {
		if bytes.len() == 0 { return Ok(None); }

		let mut read_pos = 1;
		macro_rules! get_slice {
			( $size: expr ) => {
				{
					if bytes.len() < read_pos + $size as usize {
						return Ok(None);
					}
					read_pos += $size as usize;
					&bytes[read_pos - ($size as usize)..read_pos]
				}
			}
		}

		macro_rules! advance_bytes {
			() => {
				bytes.advance(read_pos);
			}
		}

		match bytes[0] {
			1 => {
				let msg = WorkMessage::ProtocolSupport {
					max_version: slice_to_le16(get_slice!(2)),
					min_version: slice_to_le16(get_slice!(2)),
					flags: slice_to_le16(get_slice!(2)),
				};
				advance_bytes!();
				Ok(Some(msg))
			},
			2 => {
				let selected_version = slice_to_le16(get_slice!(2));
				let msg = WorkMessage::ProtocolVersion {
					selected_version: selected_version,
					auth_key: match PublicKey::from_slice(&self.secp_ctx, get_slice!(33)) {
						Ok(key) => key,
						Err(_) => {
							println!("Bad key {}", selected_version);
							return Err(io::Error::new(io::ErrorKind::InvalidData, CodecError))
						}
					}
				};
				advance_bytes!();
				Ok(Some(msg))
			},
			3 => {
				// Quick check to make sure we dont try to fill in a struct partially
				if bytes.len() < 64+8+32+4+32+4+4+1+8+4+1+4+1+4 {
					return Ok(None);
				}

				let signature = match Signature::from_compact(&self.secp_ctx, get_slice!(64)) {
					Ok(sig) => sig,
					Err(_) => return Err(io::Error::new(io::ErrorKind::InvalidData, CodecError))
				};
				let template_id = slice_to_le64(get_slice!(8));
				let mut target = [0; 32];
				target[..].copy_from_slice(get_slice!(32));

				let header_version = slice_to_le32(get_slice!(4));
				let mut header_prevblock = [0; 32];
				header_prevblock[..].copy_from_slice(get_slice!(32));
				let header_time = slice_to_le32(get_slice!(4));
				let header_nbits = slice_to_le32(get_slice!(4));

				let merkle_rhss_count = get_slice!(1)[0] as usize;
				if merkle_rhss_count > 15 {
					return Err(io::Error::new(io::ErrorKind::InvalidData, CodecError))
				}
				let mut merkle_rhss = Vec::with_capacity(merkle_rhss_count);
				for _ in 0..merkle_rhss_count {
					let mut merkle_rhs = [0; 32];
					merkle_rhs[..].copy_from_slice(get_slice!(32));
					merkle_rhss.push(merkle_rhs);
				}

				let coinbase_value_remaining = slice_to_le64(get_slice!(8));

				let coinbase_version = slice_to_le32(get_slice!(4));
				let coinbase_prefix_len = get_slice!(1)[0] as usize;
				if coinbase_prefix_len > 100 {
					return Err(io::Error::new(io::ErrorKind::InvalidData, CodecError))
				}
				let coinbase_prefix = get_slice!(coinbase_prefix_len).to_vec();
				let coinbase_input_sequence = slice_to_le32(get_slice!(4));

				let coinbase_output_count = get_slice!(1)[0] as usize;
				let mut appended_coinbase_outputs = Vec::with_capacity(coinbase_output_count);
				for _ in 0..coinbase_output_count {
					let value = slice_to_le64(get_slice!(8));
					let script_len = slice_to_le16(get_slice!(2));
					appended_coinbase_outputs.push(TxOut {
						value: value,
						script_pubkey: Script::from(get_slice!(script_len).to_vec()),
					})
				}

				let coinbase_locktime = slice_to_le32(get_slice!(4));

				let msg = WorkMessage::BlockTemplate {
					signature: signature,
					template: BlockTemplate {
						template_id: template_id,
						target: target,

						header_version: header_version,
						header_prevblock: header_prevblock,
						header_time: header_time,
						header_nbits: header_nbits,

						merkle_rhss: merkle_rhss,
						coinbase_value_remaining: coinbase_value_remaining,

						coinbase_version: coinbase_version,
						coinbase_prefix: coinbase_prefix,
						coinbase_input_sequence: coinbase_input_sequence,
						appended_coinbase_outputs: appended_coinbase_outputs,
						coinbase_locktime: coinbase_locktime,
					}
				};
				advance_bytes!();
				Ok(Some(msg))
			},
			4 => {
				// TODO
				Ok(None)
			},
			5 => {
				// TODO
				Ok(None)
			},
			6 => {
				let signature = match Signature::from_compact(&self.secp_ctx, get_slice!(64)) {
					Ok(sig) => sig,
					Err(_) => return Err(io::Error::new(io::ErrorKind::InvalidData, CodecError))
				};
				let template_id = slice_to_le64(get_slice!(8));

				let tx_count = slice_to_le32(get_slice!(4)) as usize;
				if bytes.len() < 64 + 8 + 4 + tx_count * 4 {
					return Ok(None)
				}
				let mut txn = Vec::with_capacity(tx_count);
				for _ in 0..tx_count {
					let tx_len = slice_to_le32(get_slice!(4));
					let tx_data = match network::serialize::deserialize(get_slice!(tx_len)) {
						Ok(tx) => tx,
						Err(_) => return Err(io::Error::new(io::ErrorKind::InvalidData, CodecError))
					};
					txn.push(tx_data);
				}

				let msg = WorkMessage::TransactionData {
					signature: signature,
					data: TransactionData {
						template_id: template_id,
						transactions: txn,
					},
				};
				advance_bytes!();
				Ok(Some(msg))
			},
			7 => {
				let signature = match Signature::from_compact(&self.secp_ctx, get_slice!(64)) {
					Ok(sig) => sig,
					Err(_) => return Err(io::Error::new(io::ErrorKind::InvalidData, CodecError))
				};
				let timestamp = slice_to_le64(get_slice!(8));
				let prefix_postfix_len = get_slice!(1)[0];
				if prefix_postfix_len > 100 {
					return Err(io::Error::new(io::ErrorKind::InvalidData, CodecError))
				}
				let msg = WorkMessage::CoinbasePrefixPostfix {
					signature: signature,
					coinbase_prefix_postfix: CoinbasePrefixPostfix {
						timestamp: timestamp,
						coinbase_prefix_postfix: get_slice!(prefix_postfix_len).to_vec(),
					}
				};
				advance_bytes!();
				Ok(Some(msg))
			},
			_ => {
				return Err(io::Error::new(io::ErrorKind::InvalidData, CodecError))
			}
		}
	}
}

#[derive(Clone)]
pub struct PoolPayoutInfo {
	pub timestamp: u64,
	/// Payout to local host as a ratio out of 1000 of the remaining_payout after appended_output
	/// values are subtracted out.
	pub self_payout_ratio_per_1000: u16,
	pub coinbase_postfix: Vec<u8>,
	pub remaining_payout: Script,
	pub appended_outputs: Vec<TxOut>,
}

impl PoolPayoutInfo {
	pub fn encode_unsigned(&self, res: &mut bytes::BytesMut) {
		res.reserve(12 + self.remaining_payout.len() + 1);
		res.put_u64::<bytes::LittleEndian>(self.timestamp);
		res.put_u16::<bytes::LittleEndian>(self.self_payout_ratio_per_1000);

		res.put_u8(self.coinbase_postfix.len() as u8);
		res.put_slice(&self.coinbase_postfix[..]);

		res.put_u16::<bytes::LittleEndian>(self.remaining_payout.len() as u16);
		res.put_slice(&self.remaining_payout[..]);

		res.put_u8(self.appended_outputs.len() as u8);
		for txout in self.appended_outputs.iter() {
			if res.remaining_mut() < 8 + 2 + txout.script_pubkey.len() {
				res.reserve(8 + 2 + txout.script_pubkey.len());
			}
			res.put_u64::<bytes::LittleEndian>(txout.value);
			res.put_u16::<bytes::LittleEndian>(txout.script_pubkey.len() as u16);
			res.put_slice(&txout.script_pubkey[..]);
		}
	}
}

#[derive(Clone)]
pub struct PoolDifficulty {
	pub share_target: [u8; 32],
	pub weak_block_target: [u8; 32],
}

#[derive(Clone)]
pub struct PoolShare {
	pub header_version: u32,
	pub header_prevblock: [u8; 32],
	pub header_time: u32,
	pub header_nbits: u32,
	pub header_nonce: u32,

	pub merkle_rhss: Vec<[u8; 32]>,
	pub coinbase_tx: Transaction,
}

#[derive(Clone)]
pub enum WeakBlockAction {
	/// Skips the next n transactions from the original sketch
	SkipN { // 0b01
		n: u8,
	},
	/// Includes the transaction at the current index from the original sketch
	IncludeTx {}, // 0b10
	/// Adds a new transaction not in the original sketch
	NewTx { // 0b11
		tx: Transaction
	},
}

#[derive(Clone)]
pub struct WeakBlock {
	pub header_version: u32,
	pub header_prevblock: [u8; 32],
	pub header_time: u32,
	pub header_nbits: u32,
	pub header_nonce: u32,

	pub sketch_id: u64,
	pub prev_sketch_id: u64,
	pub txn: Vec<WeakBlockAction>,
}

impl WeakBlock {
	pub fn encode(&self, res: &mut bytes::BytesMut) {
		res.reserve(4*4 + 8*2 + 32 + self.txn.len()/8);

		res.put_u32::<bytes::LittleEndian>(self.header_version);
		res.put_slice(&self.header_prevblock);
		res.put_u32::<bytes::LittleEndian>(self.header_time);
		res.put_u32::<bytes::LittleEndian>(self.header_nbits);
		res.put_u32::<bytes::LittleEndian>(self.header_nonce);

		res.put_u64::<bytes::LittleEndian>(self.sketch_id);
		res.put_u64::<bytes::LittleEndian>(self.prev_sketch_id);

		let mut action_buff = 0;
		for tx in self.txn.iter() {
			match tx {
				&WeakBlockAction::SkipN { n } => {
					action_buff <<= 2;
					action_buff |= 0b01;
					res.reserve(2);
					res.put_u8(action_buff);
					action_buff = 0;
					res.put_u8(n);
				},
				&WeakBlockAction::IncludeTx {} => {
					action_buff <<= 2;
					action_buff |= 0b10;
					if (action_buff & 0b11000000) != 0 {
						res.reserve(1);
						res.put_u8(action_buff);
						action_buff = 0;
					}
				},
				&WeakBlockAction::NewTx { ref tx } => {
					action_buff <<= 2;
					action_buff |= 0b11;
					let tx_enc = network::serialize::serialize(tx).unwrap();
					res.reserve(1 + 4 + tx_enc.len());
					res.put_u8(action_buff);
					action_buff = 0;
					res.put_u32::<bytes::LittleEndian>(tx_enc.len() as u32);
					res.put_slice(&tx_enc[..]);
				}
			}
		}
	}
}

pub enum PoolMessage {
	ProtocolSupport {
		max_version: u16,
		min_version: u16,
		flags: u16,
	},
	ProtocolVersion {
		selected_version: u16,
		auth_key: PublicKey,
	},
	PayoutInfo {
		signature: Signature,
		payout_info: PoolPayoutInfo,
	},
	ShareDifficulty {
		difficulty: PoolDifficulty,
	},
	Share {
		share: PoolShare,
	},
	WeakBlock {
		sketch: WeakBlock,
	},
	WeakBlockStateReset { },
	/*TODO:
	NewPoolServer {
		signature: Signature,
		new_host_ports: Vec<String>,
	},
*/
}

pub struct PoolMsgFramer {
	secp_ctx: Secp256k1,
}

impl PoolMsgFramer {
	pub fn new() -> PoolMsgFramer {
		PoolMsgFramer {
			secp_ctx: Secp256k1::new(),
		}
	}
}

impl codec::Encoder for PoolMsgFramer {
	type Item = PoolMessage;
	type Error = io::Error;

	fn encode(&mut self, msg: PoolMessage, res: &mut bytes::BytesMut) -> Result<(), io::Error> {
		//TODO: CHeck res size!!!
		match msg {
			PoolMessage::ProtocolSupport { max_version, min_version, flags } => {
				res.put_u8(1);
				res.put_u16::<bytes::LittleEndian>(max_version);
				res.put_u16::<bytes::LittleEndian>(min_version);
				res.put_u16::<bytes::LittleEndian>(flags);
			},
			PoolMessage::ProtocolVersion { selected_version, ref auth_key } => {
				res.put_u8(2);
				res.put_u16::<bytes::LittleEndian>(selected_version);
				res.put_slice(&auth_key.serialize());
			},
			PoolMessage::PayoutInfo { ref signature, ref payout_info } => {
				res.put_u8(3);
				res.put_slice(&signature.serialize_compact(&self.secp_ctx));
				payout_info.encode_unsigned(res);
			},
			PoolMessage::ShareDifficulty { ref difficulty } => {
				res.put_u8(4);
				res.put_slice(&difficulty.share_target[..]);
				res.put_slice(&difficulty.weak_block_target[..]);
			},
			PoolMessage::Share { ref share } => {
				res.put_u8(5);
				res.put_u32::<bytes::LittleEndian>(share.header_version);
				res.put_slice(&share.header_prevblock);
				res.put_u32::<bytes::LittleEndian>(share.header_time);
				res.put_u32::<bytes::LittleEndian>(share.header_nbits);
				res.put_u32::<bytes::LittleEndian>(share.header_nonce);
				res.put_u8(share.merkle_rhss.len() as u8);
				for rhs in share.merkle_rhss.iter() {
					res.put_slice(rhs);
				}
				let tx_enc = network::serialize::serialize(&share.coinbase_tx).unwrap();
				res.put_u32::<bytes::LittleEndian>(tx_enc.len() as u32);
				res.put_slice(&tx_enc[..]);
			},
			PoolMessage::WeakBlock { ref sketch } => {
				res.put_u8(6);
				sketch.encode(res);
			},
			PoolMessage::WeakBlockStateReset { } => {
				res.put_u8(7);
			}
		}
		Ok(())
	}
}


impl codec::Decoder for PoolMsgFramer {
	type Item = PoolMessage;
	type Error = io::Error;

	fn decode(&mut self, bytes: &mut bytes::BytesMut) -> Result<Option<PoolMessage>, io::Error> {
		if bytes.len() == 0 { return Ok(None); }

		let mut read_pos = 1;
		macro_rules! get_slice {
			( $size: expr ) => {
				{
					if bytes.len() < read_pos + $size as usize {
						return Ok(None);
					}
					read_pos += $size as usize;
					&bytes[read_pos - ($size as usize)..read_pos]
				}
			}
		}

		macro_rules! advance_bytes {
			() => {
				bytes.advance(read_pos);
			}
		}

		match bytes[0] {
			1 => {
				let msg = PoolMessage::ProtocolSupport {
					max_version: slice_to_le16(get_slice!(2)),
					min_version: slice_to_le16(get_slice!(2)),
					flags: slice_to_le16(get_slice!(2)),
				};
				advance_bytes!();
				Ok(Some(msg))
			},
			2 => {
				let selected_version = slice_to_le16(get_slice!(2));
				let msg = PoolMessage::ProtocolVersion {
					selected_version: selected_version,
					auth_key: match PublicKey::from_slice(&self.secp_ctx, get_slice!(33)) {
						Ok(key) => key,
						Err(_) => {
							println!("Bad key {}", selected_version);
							return Err(io::Error::new(io::ErrorKind::InvalidData, CodecError))
						}
					}
				};
				advance_bytes!();
				Ok(Some(msg))
			},
			3 => {
				let signature = match Signature::from_compact(&self.secp_ctx, get_slice!(64)) {
					Ok(sig) => sig,
					Err(_) => return Err(io::Error::new(io::ErrorKind::InvalidData, CodecError))
				};
				let timestamp = slice_to_le64(get_slice!(8));
				let payout_ratio = slice_to_le16(get_slice!(2));

				let coinbase_postfix_len = get_slice!(1)[0];
				let coinbase_postfix = get_slice!(coinbase_postfix_len).to_vec();

				let script_len = slice_to_le16(get_slice!(2));
				let script = Script::from(get_slice!(script_len).to_vec());

				let coinbase_output_count = get_slice!(1)[0] as usize;
				let mut appended_coinbase_outputs = Vec::with_capacity(coinbase_output_count);
				for _ in 0..coinbase_output_count {
					let value = slice_to_le64(get_slice!(8));
					let script_len = slice_to_le16(get_slice!(2));
					appended_coinbase_outputs.push(TxOut {
						value: value,
						script_pubkey: Script::from(get_slice!(script_len).to_vec()),
					})
				}

				let msg = PoolMessage::PayoutInfo {
					signature: signature,
					payout_info: PoolPayoutInfo {
						timestamp: timestamp,
						self_payout_ratio_per_1000: payout_ratio,
						coinbase_postfix: coinbase_postfix,
						remaining_payout: script,
						appended_outputs: appended_coinbase_outputs
					}
				};
				advance_bytes!();
				Ok(Some(msg))
			},
			4 => {
				let mut share_target = [0; 32];
				share_target.copy_from_slice(get_slice!(32));
				let mut weak_block_target = [0; 32];
				weak_block_target.copy_from_slice(get_slice!(32));

				let msg = PoolMessage::ShareDifficulty {
					difficulty: PoolDifficulty {
						share_target: share_target,
						weak_block_target: weak_block_target,
					}
				};
				advance_bytes!();
				Ok(Some(msg))
			},
			5 => {
				//TODO
				Ok(None)
			},
			6 => {
				//TODO
				Ok(None)
			},
			7 => {
				advance_bytes!();
				Ok(Some(PoolMessage::WeakBlockStateReset {}))
			},
			_ => {
				return Err(io::Error::new(io::ErrorKind::InvalidData, CodecError))
			}
		}
	}
}

pub trait ConnectionHandler<MessageType> {
	type Stream : Stream<Item = MessageType>;
	type Framer : codec::Encoder<Item = MessageType, Error = io::Error> + codec::Decoder<Item = MessageType, Error = io::Error>;
	fn new_connection(&mut self) -> (Self::Framer, Self::Stream);
	fn handle_message(&mut self, msg: MessageType) -> Result<(), io::Error>;
	fn connection_closed(&mut self);
}

pub struct ConnectionMaintainer<MessageType: 'static, HandlerProvider : ConnectionHandler<MessageType>> {
	host: String,
	cur_addrs: Option<Vec<SocketAddr>>,
	handler: HandlerProvider,
	ph : marker::PhantomData<&'static MessageType>,
}

pub static mut TIMER: Option<Timer> = None;
impl<MessageType, HandlerProvider : 'static + ConnectionHandler<MessageType>> ConnectionMaintainer<MessageType, HandlerProvider> {
	pub fn new(host: String, handler: HandlerProvider) -> ConnectionMaintainer<MessageType, HandlerProvider> {
		ConnectionMaintainer {
			host: host,
			cur_addrs: None,
			handler: handler,
			ph: marker::PhantomData,
		}
	}

	pub fn make_connection(rc: Rc<RefCell<Self>>) {
		if {
			let mut us = rc.borrow_mut();
			if us.cur_addrs.is_none() {
				//TODO: Resolve async
				match us.host.to_socket_addrs() {
					Err(_) => {
						true
					},
					Ok(addrs) => {
						us.cur_addrs = Some(addrs.collect());
						false
					}
				}
			} else { false }
		} {
			let timer: &Timer = unsafe { TIMER.as_ref().unwrap() };
			current_thread::spawn(timer.sleep(Duration::from_secs(30)).then(move |_| -> future::FutureResult<(), ()> {
				Self::make_connection(rc);
				future::result(Ok(()))
			}));
			return;
		}

		let addr_option = {
			let mut us = rc.borrow_mut();
			let addr = us.cur_addrs.as_mut().unwrap().pop();
			if addr.is_none() {
				us.cur_addrs = None;
			}
			addr
		};

		match addr_option {
			Some(addr) => {
				println!("Trying connection to {}", addr);

				current_thread::spawn(net::TcpStream::connect(&addr).then(move |res| -> future::FutureResult<(), ()> {
					match res {
						Ok(stream) => {
							println!("Connected to {}!", stream.peer_addr().unwrap());
							stream.set_nodelay(true).unwrap();

							let (framer, tx_stream) = rc.borrow_mut().handler.new_connection();
							let (tx, rx) = stream.framed(framer).split();
							let stream = tx_stream.map_err(|_| -> io::Error {
								panic!("mpsc streams cant generate errors!");
							});
							current_thread::spawn(tx.send_all(stream).then(|_| {
								println!("Disconnected on send side, will reconnect...");
								future::result(Ok(()))
							}));
							let rc_clone = rc.clone();
							let rc_clone_2 = rc.clone();
							current_thread::spawn(rx.for_each(move |msg| {
								future::result(rc_clone.borrow_mut().handler.handle_message(msg))
							}).then(move |_| {
								println!("Disconnected on recv side, will reconnect...");
								rc_clone_2.borrow_mut().handler.connection_closed();
								Self::make_connection(rc);
								future::result(Ok(()))
							}));
						},
						Err(_) => {
							Self::make_connection(rc);
						}
					};
					future::result(Ok(()))
				}));
			},
			None => {
				let timer: &Timer = unsafe { TIMER.as_ref().unwrap() };
				current_thread::spawn(timer.sleep(Duration::from_secs(30)).then(move |_| {
					Self::make_connection(rc);
					future::result(Ok(()))
				}));
			},
		}
	}
}

