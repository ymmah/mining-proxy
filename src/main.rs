extern crate bitcoin;
extern crate bytes;
extern crate futures;
extern crate tokio;
extern crate tokio_io;
extern crate tokio_timer;
extern crate crypto;
extern crate secp256k1;

#[macro_use]
extern crate serde_json;

mod msg_framing;
use msg_framing::*;

mod stratum_server;
use stratum_server::*;

mod utils;

use bitcoin::blockdata::transaction::{TxOut,Transaction};
use bitcoin::blockdata::script::Script;
use bitcoin::util::address::Address;
use bitcoin::util::base58::FromBase58;
use bitcoin::util::hash::Sha256dHash;

use bytes::BufMut;

use futures::future;
use futures::unsync::{mpsc,oneshot};
use futures::{Future,Stream,Sink};

use tokio::executor::current_thread;
use tokio::net;

use crypto::digest::Digest;
use crypto::sha2::Sha256;

use secp256k1::key::PublicKey;
use secp256k1::Secp256k1;

use std::cell::RefCell;
use std::collections::HashMap;
use std::env;
use std::error::Error;
use std::fmt;
use std::io;
use std::net::ToSocketAddrs;
use std::rc::Rc;

#[derive(Debug)]
struct HandleError;
impl fmt::Display for HandleError {
	fn fmt(&self, fmt: &mut fmt::Formatter) -> Result<(), fmt::Error> {
		fmt.write_str("Failed to handle message")
	}
}
impl Error for HandleError {
	fn description(&self) -> &str {
		"Failed to handle message"
	}
}

/// A future, essentially
struct Eventual<Value> {
	// We dont really want Fn here, we want FnOnce, but we can't because that'd require a move of
	// the function onto stack, which is of unknown size, so we cant...
	callees: Vec<Box<Fn(&Value)>>,
	value: Option<Value>,
}
impl<Value: 'static> Eventual<Value> {
	fn new() -> (Rc<RefCell<Self>>, oneshot::Sender<Value>) {
		let us = Rc::new(RefCell::new(Self {
			callees: Vec::new(),
			value: None,
		}));
		let (tx, rx) = oneshot::channel();
		let us_ref = us.clone();
		current_thread::spawn(rx.and_then(move |res| {
			let mut us = us_ref.borrow_mut();
			for callee in us.callees.iter() {
				(callee)(&res);
			}
			us.callees.clear();
			us.value = Some(res);
			future::result(Ok(()))
		}).then(|_| {
			future::result(Ok(()))
		}));
		(us, tx)
	}

	fn get_and(&mut self, then: Box<Fn(&Value)>) {
		match &self.value {
			&Some(ref value) => {
				then(value);
			},
			&None => {
				self.callees.push(then);
			},
		}
	}
}

pub struct JobProviderHandler {
	have_pool: bool,

	stream: Option<mpsc::UnboundedSender<WorkMessage>>,
	auth_key: Option<PublicKey>,

	cur_template: Option<BlockTemplate>,
	cur_prefix_postfix: Option<CoinbasePrefixPostfix>,

	pending_tx_data_requests: HashMap<u64, oneshot::Sender<TransactionData>>,
	job_stream: mpsc::Sender<(BlockTemplate, Option<CoinbasePrefixPostfix>, Rc<RefCell<Eventual<TransactionData>>>)>,

	secp_ctx: Secp256k1,
}

impl JobProviderHandler {
	fn new(expected_auth_key: Option<PublicKey>, have_pool: bool) -> (Rc<RefCell<JobProviderHandler>>, mpsc::Receiver<(BlockTemplate, Option<CoinbasePrefixPostfix>, Rc<RefCell<Eventual<TransactionData>>>)>) {
		let (work_sender, work_receiver) = mpsc::channel(10);

		(Rc::new(RefCell::new(JobProviderHandler {
			have_pool: have_pool,

			stream: None,
			auth_key: expected_auth_key,

			cur_template: None,
			cur_prefix_postfix: None,

			pending_tx_data_requests: HashMap::new(),
			job_stream: work_sender,

			secp_ctx: Secp256k1::new(),
		})), work_receiver)
	}

	fn send_nonce(&mut self, work: WinningNonce) {
		match &self.stream {
			&Some(ref stream) => {
				match stream.unbounded_send(WorkMessage::WinningNonce {
					nonces: work
				}) {
					Ok(_) => { println!("Submitted job-matching (ie full-block) nonce!"); },
					Err(_) => { println!("Failed to submit job-matching (ie full-block) nonce as job provider disconnected"); }
				}
			},
			&None => {
				println!("Failed to submit job-matching (ie full-block) nonce!");
			}
		}
	}
}

impl ConnectionHandler<WorkMessage> for Rc<RefCell<JobProviderHandler>> {
	type Stream = mpsc::UnboundedReceiver<WorkMessage>;
	type Framer = WorkMsgFramer;

	fn new_connection(&mut self) -> (WorkMsgFramer, mpsc::UnboundedReceiver<WorkMessage>) {
		let mut us = self.borrow_mut();

		let (mut tx, rx) = mpsc::unbounded();
		match tx.start_send(WorkMessage::ProtocolSupport {
			max_version: 1,
			min_version: 1,
			flags: if us.have_pool { 0 } else { 1 },
		}) {
			Ok(_) => {
				us.stream = Some(tx);
			},
			Err(_) => {},
		}
		(WorkMsgFramer::new(), rx)
	}

	fn connection_closed(&mut self) {
		self.borrow_mut().stream = None;
	}

	fn handle_message(&mut self, msg: WorkMessage) -> Result<(), io::Error> {
		let mut us = self.borrow_mut();

		macro_rules! check_msg_sig {
			($msg_type: expr, $msg: expr, $signature: expr) => {
				{
					let mut msg_signed = bytes::BytesMut::with_capacity(1000);
					msg_signed.put_u8($msg_type);
					$msg.encode_unsigned(&mut msg_signed);
					let hash = {
						let mut sha = Sha256::new();
						sha.input(&msg_signed[..]);
						let mut h = [0; 32];
						sha.result(&mut h);
						secp256k1::Message::from_slice(&h).unwrap()
					};

					match us.auth_key {
						Some(pubkey) => match us.secp_ctx.verify(&hash, &$signature, &pubkey) {
							Ok(()) => {},
							Err(_) => return Err(io::Error::new(io::ErrorKind::InvalidData, HandleError))
						},
						None => return Err(io::Error::new(io::ErrorKind::InvalidData, HandleError))
					}
				}
			}
		}

		match msg {
			WorkMessage::ProtocolSupport { .. } => {
				println!("Received ProtocolSupport");
				return Err(io::Error::new(io::ErrorKind::InvalidData, HandleError));
			},
			WorkMessage::ProtocolVersion { selected_version, ref auth_key } => {
				if selected_version != 1 {
					return Err(io::Error::new(io::ErrorKind::InvalidData, HandleError));
				}
				if us.auth_key.is_none() {
					us.auth_key = Some(auth_key.clone());
				} else {
					if us.auth_key.unwrap() != *auth_key {
						println!("Got unexpected auth key");
						return Err(io::Error::new(io::ErrorKind::InvalidData, HandleError));
					}
				}
				println!("Received ProtocolVersion, using version {}", selected_version);
			},
			WorkMessage::BlockTemplate { signature, template } => {
				check_msg_sig!(3, template, signature);

				if us.cur_template.is_none() || us.cur_template.as_ref().unwrap().template_id < template.template_id {
					println!("Received new BlockTemplate");
					let (txn, txn_tx) = Eventual::new();
					let cur_postfix_prefix = us.cur_prefix_postfix.clone();
					match us.job_stream.start_send((template.clone(), cur_postfix_prefix.clone(), txn)) {
						Ok(_) => {},
						Err(_) => {
							println!("Job provider sending jobs too quickly");
							return Err(io::Error::new(io::ErrorKind::InvalidData, HandleError));
						}
					}
					match us.stream.as_ref().unwrap().unbounded_send(WorkMessage::TransactionDataRequest { template_id: template.template_id }) {
						Ok(_) => {},
						Err(_) => { panic!("unbounded streams should never fail"); }
					}
					us.pending_tx_data_requests.insert(template.template_id, txn_tx);
					us.cur_template = Some(template);
				}
			},
			WorkMessage::WinningNonce { .. } => {
				println!("Received WinningNonce?");
				return Err(io::Error::new(io::ErrorKind::InvalidData, HandleError));
			},
			WorkMessage::TransactionDataRequest { .. } => {
				println!("Received TransactionDataRequest?");
				return Err(io::Error::new(io::ErrorKind::InvalidData, HandleError));
			},
			WorkMessage::TransactionData { signature, data } => {
				check_msg_sig!(6, data, signature);

				match us.pending_tx_data_requests.remove(&data.template_id) {
					Some(chan) => {
						match chan.send(data) {
							Ok(()) => {},
							Err(_) => {
								println!("We gave up on job before job provider sent us transactions");
							}
						}
					},
					None => {
						println!("Received unknown TransactionData?");
					}
				}
			},
			WorkMessage::CoinbasePrefixPostfix { signature, coinbase_prefix_postfix } => {
				check_msg_sig!(7, coinbase_prefix_postfix, signature);

				if us.cur_prefix_postfix.is_none() || us.cur_prefix_postfix.as_ref().unwrap().timestamp < coinbase_prefix_postfix.timestamp {
					println!("Received new CoinbasePrefixPostfix");
					us.cur_prefix_postfix = Some(coinbase_prefix_postfix);
					if us.cur_template.is_some() {
						let cur_prefix_postfix = us.cur_prefix_postfix.clone();
						let template = us.cur_template.as_ref().unwrap().clone();

						let (txn, txn_tx) = Eventual::new();
						//TODO: This is pretty lazy...we should cache these instead of requesting
						//new ones from the server...hopefully they dont update the coinbase prefix
						//postfix very often...
						match us.stream.as_ref().unwrap().unbounded_send(WorkMessage::TransactionDataRequest { template_id: template.template_id }) {
							Ok(_) => {},
							Err(_) => { panic!("unbounded streams should never fail"); }
						}
						us.pending_tx_data_requests.insert(template.template_id, txn_tx);

						match us.job_stream.start_send((template, cur_prefix_postfix, txn)) {
							Ok(_) => {},
							Err(_) => {
								println!("Job provider sending jobs too quickly");
								return Err(io::Error::new(io::ErrorKind::InvalidData, HandleError));
							}
						}
					}
				}
			},
		}
		Ok(())
	}
}

struct PoolHandler {
	pool_priority: usize,
	stream: Option<mpsc::UnboundedSender<PoolMessage>>,
	auth_key: Option<PublicKey>,

	cur_payout_info: Option<PoolPayoutInfo>,
	cur_difficulty: Option<PoolDifficulty>,
	last_weak_block: Option<WeakBlock>,

	job_stream: mpsc::Sender<(PoolPayoutInfo, Option<PoolDifficulty>)>,

	secp_ctx: Secp256k1,
}

impl PoolHandler {
	fn new(expected_auth_key: Option<PublicKey>, pool_priority: usize) -> (Rc<RefCell<PoolHandler>>, mpsc::Receiver<(PoolPayoutInfo, Option<PoolDifficulty>)>) {
		let (work_sender, work_receiver) = mpsc::channel(5);

		(Rc::new(RefCell::new(PoolHandler {
			pool_priority: pool_priority,
			stream: None,
			auth_key: expected_auth_key,

			cur_payout_info: None,
			cur_difficulty: None,
			last_weak_block: None,

			job_stream: work_sender,

			secp_ctx: Secp256k1::new(),
		})), work_receiver)
	}

	fn is_connected(&self) -> bool {
		self.stream.is_some()
	}

	fn get_priority(&self) -> usize {
		self.pool_priority
	}

	fn send_nonce(&mut self, work: &(WinningNonce, Sha256dHash), template: &Rc<BlockTemplate>, post_coinbase_txn: &Vec<Transaction>) {
		match self.cur_difficulty {
			Some(ref difficulty) => {
				if utils::does_hash_meet_target(&work.1[..], &difficulty.share_target[..]) {
					match self.stream {
						Some(ref stream) => {
							match stream.unbounded_send(PoolMessage::Share {
								share: PoolShare {
									header_version: work.0.header_version,
									header_prevblock: template.header_prevblock.clone(),
									header_time: work.0.header_time,
									header_nbits: template.header_nbits,
									header_nonce: work.0.header_nonce,
									merkle_rhss: template.merkle_rhss.clone(),
									coinbase_tx: work.0.coinbase_tx.clone(),
								}
							}) {
								Ok(_) => { println!("Submitted share!"); },
								Err(_) => { println!("Failed to submit nonce as pool connection lost"); },
							}
						},
						None => {
							println!("Failed to submit nonce as pool connection lost");
						}
					}
				}
				if utils::does_hash_meet_target(&work.1[..], &difficulty.weak_block_target[..]) {
					//TODO
				}
			},
			None => {
				println!("Got share but failed to submit because pool has not yet provided difficulty information!");
			}
		}
	}
}

impl ConnectionHandler<PoolMessage> for Rc<RefCell<PoolHandler>> {
	type Stream = mpsc::UnboundedReceiver<PoolMessage>;
	type Framer = PoolMsgFramer;

	fn new_connection(&mut self) -> (PoolMsgFramer, mpsc::UnboundedReceiver<PoolMessage>) {
		let (tx, rx) = mpsc::unbounded();
		let mut us = self.borrow_mut();
		us.stream = Some(tx);
		us.last_weak_block = None;
		(PoolMsgFramer::new(), rx)
	}

	fn connection_closed(&mut self) {
		self.borrow_mut().stream = None;
	}

	fn handle_message(&mut self, msg: PoolMessage) -> Result<(), io::Error> {
		let mut us = self.borrow_mut();
		match msg {
			PoolMessage::ProtocolSupport { .. } => {
				println!("Received ProtocolSupport");
				return Err(io::Error::new(io::ErrorKind::InvalidData, HandleError));
			},
			PoolMessage::ProtocolVersion { selected_version, ref auth_key } => {
				if selected_version != 1 {
					return Err(io::Error::new(io::ErrorKind::InvalidData, HandleError));
				}
				if us.auth_key.is_none() {
					us.auth_key = Some(auth_key.clone());
				} else {
					if us.auth_key.unwrap() != *auth_key {
						println!("Got unexpected auth key");
						return Err(io::Error::new(io::ErrorKind::InvalidData, HandleError));
					}
				}
				println!("Received ProtocolVersion, using version {}", selected_version);
			},
			PoolMessage::PayoutInfo { signature, payout_info } => {
				let mut msg_signed = bytes::BytesMut::with_capacity(100);
				msg_signed.put_u8(3);
				payout_info.encode_unsigned(&mut msg_signed);
				let hash = {
					let mut sha = Sha256::new();
					sha.input(&msg_signed[..]);
					let mut h = [0; 32];
					sha.result(&mut h);
					secp256k1::Message::from_slice(&h).unwrap()
				};

				match us.auth_key {
					Some(pubkey) => match us.secp_ctx.verify(&hash, &signature, &pubkey) {
						Ok(()) => {},
						Err(_) => return Err(io::Error::new(io::ErrorKind::InvalidData, HandleError))
					},
					None => return Err(io::Error::new(io::ErrorKind::InvalidData, HandleError))
				}

				if us.cur_payout_info.is_none() || us.cur_payout_info.as_ref().unwrap().timestamp < payout_info.timestamp {
					println!("Received new payout info!");
					let cur_difficulty = us.cur_difficulty.clone();
					match us.job_stream.start_send((payout_info.clone(), cur_difficulty.clone())) {
						Ok(_) => {},
						Err(_) => {
							println!("Pool updating payout info too quickly");
							return Err(io::Error::new(io::ErrorKind::InvalidData, HandleError));
						}
					}
					us.cur_payout_info = Some(payout_info);
				}
			},
			PoolMessage::ShareDifficulty { difficulty } => {
				println!("Received new difficulty!");
				us.cur_difficulty = Some(difficulty);
				if us.cur_payout_info.is_some() {
					let cur_difficulty = us.cur_difficulty.clone();
					let payout_info = us.cur_payout_info.as_ref().unwrap().clone();
					match us.job_stream.start_send((payout_info, cur_difficulty)) {
						Ok(_) => {},
						Err(_) => {
							println!("Pool updating difficulty too quickly");
							return Err(io::Error::new(io::ErrorKind::InvalidData, HandleError));
						}
					}
				}
			},
			PoolMessage::Share { .. } => {
				println!("Received Share?");
				return Err(io::Error::new(io::ErrorKind::InvalidData, HandleError));
			},
			PoolMessage::WeakBlock { .. } => {
				println!("Received WeakBlock?");
				return Err(io::Error::new(io::ErrorKind::InvalidData, HandleError));
			},
			PoolMessage::WeakBlockStateReset { } => {
				println!("Received WeakBlocKStateReset");
				us.last_weak_block = None;
			},
		}
		Ok(())
	}
}

fn merge_job_pool(our_payout_script: Script, job_info: &Option<(BlockTemplate, Option<CoinbasePrefixPostfix>, Rc<RefCell<Eventual<TransactionData>>>)>, job_source: Option<Rc<RefCell<JobProviderHandler>>>, payout_info: &Option<(PoolPayoutInfo, Option<PoolDifficulty>)>, payout_source: Option<Rc<RefCell<PoolHandler>>>) -> Option<WorkInfo> {
	match job_info {
		&Some((ref template_ref, ref coinbase_prefix_postfix, ref tx_data)) => {
			let mut template = template_ref.clone();

			let mut outputs = Vec::with_capacity(template.appended_coinbase_outputs.len() + 2);
			let mut constant_value_output = 0;
			for output in template.appended_coinbase_outputs.iter() {
				if output.value > 21000000*100000000 {
					return None;
				}
				constant_value_output += output.value;
			}

			match coinbase_prefix_postfix {
				&Some(ref postfix) => {
					template.coinbase_prefix.extend_from_slice(&postfix.coinbase_prefix_postfix[..]);
				},
				&None => {}
			}

			let mut self_payout_ratio_per_1000 = 0;
			match payout_info {
				&Some((ref info, _)) => {
					for output in info.appended_outputs.iter() {
						if output.value > 21000000*100000000 {
							return None;
						}
						constant_value_output += output.value;
					}

					self_payout_ratio_per_1000 = info.self_payout_ratio_per_1000;
				},
				&None => {}
			}

			let value_remaining = (template.coinbase_value_remaining as i64) - (constant_value_output as i64);
			if value_remaining < 0 {
				return None;
			}

			let our_value = value_remaining * (self_payout_ratio_per_1000 as i64) / 1000;
			outputs.push(TxOut {
				value: our_value as u64,
				script_pubkey: our_payout_script,
			});

			match payout_info {
				&Some((ref info, ref difficulty)) => {
					outputs.push(TxOut {
						value: (value_remaining - our_value) as u64,
						script_pubkey: info.remaining_payout.clone(),
					});

					outputs.extend_from_slice(&info.appended_outputs[..]);

					match difficulty {
						&Some(ref pool_diff) => {
							template.target = utils::min_le(template.target, pool_diff.share_target);
							template.target = utils::min_le(template.target, pool_diff.weak_block_target);
						},
						&None => {}
					}

					template.coinbase_prefix.extend_from_slice(&info.coinbase_postfix[..]);
				},
				&None => {}
			}

			outputs.extend_from_slice(&template.appended_coinbase_outputs[..]);

			template.appended_coinbase_outputs = outputs;

			let template_rc = Rc::new(template);

			let (solution_tx, solution_rx) = mpsc::unbounded();
			let tx_data_ref = tx_data.clone();
			let template_ref = template_rc.clone();
			current_thread::spawn(solution_rx.for_each(move |nonces: Rc<(WinningNonce, Sha256dHash)>| {
				match job_source {
					Some(ref source) => {
						if utils::does_hash_meet_target(&nonces.1[..], &template_ref.target[..]) {
							source.borrow_mut().send_nonce(nonces.0.clone());
						}
					},
					None => {}
				}
				match payout_source {
					Some(ref source) => {
						let source_ref = source.clone();
						let template_ref_2 = template_ref.clone();
						tx_data_ref.borrow_mut().get_and(Box::new(move |txn| {
							let source_clone = source_ref.clone();
							source_clone.borrow_mut().send_nonce(&nonces, &template_ref_2, &txn.transactions);
						}));
					},
					None => {}
				}
				future::result(Ok(()))
			}).then(|_| {
				future::result(Ok(()))
			}));

			Some(WorkInfo {
				template: template_rc,
				solutions: solution_tx
			})
		},
		&None => None
	}
}

struct JobInfo {
	payout_script: Script,
	cur_job: Option<(BlockTemplate, Option<CoinbasePrefixPostfix>, Rc<RefCell<Eventual<TransactionData>>>)>,
	cur_job_source: Option<Rc<RefCell<JobProviderHandler>>>,
	cur_pool: Option<(PoolPayoutInfo, Option<PoolDifficulty>)>,
	cur_pool_source: Option<Rc<RefCell<PoolHandler>>>,
	job_tx: mpsc::Sender<WorkInfo>,
}

fn main() {
	println!("USAGE: stratum-proxy (--job_provider=host:port)* (--pool_server=host:port)* --listen_port=port --payout_address=addr");
	println!("--job_provider - bitcoind(s) running as mining server(s) to get work from");
	println!("--pool_server - pool server(s) to get payout address from/submit shares to");
	println!("--stratum_listen_bind - the address to bind to to announce stratum jobs on");
	println!("--payout_address - the Bitcoin address on which to receive payment");
	println!("We always try to keep exactly one connection open per argument, no matter how");
	println!("many hosts a DNS name may resolve to. We try each hostname until one works.");
	println!("Job providers are not prioritized (the latest job is always used), pools are");
	println!("prioritized in the order they appear on the command line.");

	let mut job_provider_hosts = Vec::new();
	let mut pool_server_hosts = Vec::new();
	let mut stratum_listen_bind = None;
	let mut payout_addr = None;

	for arg in env::args().skip(1) {
		if arg.starts_with("--job_provider") {
			match arg.split_at(15).1.to_socket_addrs() {
				Err(_) => {
					println!("Bad address resolution: {}", arg);
					return;
				},
				Ok(_) => job_provider_hosts.push(arg.split_at(15).1.to_string())
			}
		} else if arg.starts_with("--pool_server") {
			match arg.split_at(14).1.to_socket_addrs() {
				Err(_) => {
					println!("Bad address resolution: {}", arg);
					return;
				},
				Ok(_) => pool_server_hosts.push(arg.split_at(14).1.to_string())
			}
		} else if arg.starts_with("--stratum_listen_bind") {
			if stratum_listen_bind.is_some() {
				println!("Cannot specify multiple listen binds");
				return;
			}
			stratum_listen_bind = Some(match arg.split_at(22).1.parse() {
				Ok(sockaddr) => sockaddr,
				Err(_) =>{
					println!("Failed to parse stratum_listen_bind into a socket address");
					return;
				}
			});
		} else if arg.starts_with("--payout_address") {
			if payout_addr.is_some() {
				println!("Cannot specify multiple payout addresses");
				return;
			}
			//TODO: bech32, check network magic byte
			payout_addr = Some(match Address::from_base58check(arg.split_at(17).1) {
				Ok(addr) => addr,
				Err(_) => {
					println!("Failed to parse payout_address into a Bitcoin address");
					return;
				}
			});
		} else {
			println!("Unkown arg: {}", arg);
			return;
		}
	}

	if job_provider_hosts.is_empty() {
		println!("Need at least some job providers");
		return;
	}
	if stratum_listen_bind.is_none() {
		println!("Need some listen bind");
		return;
	}
	if payout_addr.is_none() {
		println!("Need some payout address");
		return;
	}

	unsafe {
		TIMER = Some(tokio_timer::Timer::default());
	}

	let (job_tx, job_rx) = mpsc::channel(5);
	let cur_work_rc = Rc::new(RefCell::new(JobInfo {
		payout_script: payout_addr.clone().unwrap().script_pubkey(),
		cur_job: None,
		cur_job_source: None,
		cur_pool: None,
		cur_pool_source: None,
		job_tx: job_tx,
	}));

	current_thread::run(|_| {
		for host in job_provider_hosts {
			let (mut handler, mut job_rx) = JobProviderHandler::new(None, !pool_server_hosts.is_empty());
			let work_rc = cur_work_rc.clone();
			let handler_rc = handler.clone();
			current_thread::spawn(job_rx.for_each(move |job| {
				let mut cur_work = work_rc.borrow_mut();
				if cur_work.cur_job.is_none() || cur_work.cur_job.as_ref().unwrap().0.template_id < job.0.template_id {
					let new_job = Some(job);
					match merge_job_pool(cur_work.payout_script.clone(), &new_job, Some(handler_rc.clone()), &cur_work.cur_pool, cur_work.cur_pool_source.clone()) {
						Some(work) => {
							match cur_work.job_tx.start_send(work) {
								Ok(_) => {},
								Err(_) => {
									println!("Job provider is providing work faster than we can process it");
								}
							}
							cur_work.cur_job = new_job;
							cur_work.cur_job_source = Some(handler_rc.clone());
						},
						None => {}
					}
				}
				future::result(Ok(()))
			}).then(|_| {
				future::result(Ok(()))
			}));
			ConnectionMaintainer::make_connection(Rc::new(RefCell::new(ConnectionMaintainer::new(host, handler))));
		}

		for (idx, host) in pool_server_hosts.iter().enumerate() {
			let (mut handler, mut pool_rx) = PoolHandler::new(None, idx);
			let work_rc = cur_work_rc.clone();
			let handler_rc = handler.clone();
			current_thread::spawn(pool_rx.for_each(move |pool_info| {
				let mut cur_work = work_rc.borrow_mut();
				match cur_work.cur_pool_source {
					Some(ref cur_pool) => {
						let pool = cur_pool.borrow();
						//TODO: Fallback to lower-priority pool when one gets disconnected
						if pool.is_connected() && pool.get_priority() < handler_rc.borrow().get_priority() {
							return future::result(Ok(()));
						}
					},
					None => {}
				}
				let new_pool = Some(pool_info);
				match merge_job_pool(cur_work.payout_script.clone(), &cur_work.cur_job, cur_work.cur_job_source.clone(), &new_pool, Some(handler_rc.clone())) {
					Some(work) => {
						match cur_work.job_tx.start_send(work) {
							Ok(_) => {},
							Err(_) => {
								println!("Job provider is providing work faster than we can process it");
							}
						}
						cur_work.cur_pool = new_pool;
						cur_work.cur_pool_source = Some(handler_rc.clone());
					},
					None => {
						if cur_work.cur_job.is_none() {
							cur_work.cur_pool = new_pool;
							cur_work.cur_pool_source = Some(handler_rc.clone());
						}
					}
				}
				future::result(Ok(()))
			}).then(|_| {
				future::result(Ok(()))
			}));
			ConnectionMaintainer::make_connection(Rc::new(RefCell::new(ConnectionMaintainer::new(host.clone(), handler))));
		}

		let stratum_server = StratumServer::new(job_rx);
		match net::TcpListener::bind(&stratum_listen_bind.unwrap()) {
			Ok(listener) => {
				current_thread::spawn(listener.incoming().for_each(move |sock| {
					StratumServer::new_connection(stratum_server.clone(), sock);
					future::result(Ok(()))
				}).then(|_| {
					future::result(Ok(()))
				}));
			},
			Err(_) => {
				println!("Failed to bind to listen bind addr");
				return;
			}
		};
	});
}