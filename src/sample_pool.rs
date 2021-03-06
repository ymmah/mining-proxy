extern crate bitcoin;
extern crate bytes;
extern crate crypto;
extern crate futures;
extern crate tokio;
extern crate tokio_io;
extern crate tokio_timer;
extern crate secp256k1;

mod msg_framing;
use msg_framing::*;

mod utils;

use bitcoin::blockdata::block::BlockHeader;
use bitcoin::network::serialize::BitcoinHash;
use bitcoin::util::address::Address;
use bitcoin::util::address;
use bitcoin::util::hash::Sha256dHash;

use bytes::BufMut;

use crypto::digest::Digest;
use crypto::sha2::Sha256;

use futures::{future,Stream,Sink,Future};
use futures::unsync::mpsc;

use tokio::executor::current_thread;
use tokio::net;

use tokio_io::AsyncRead;

use secp256k1::key::PublicKey;
use secp256k1::Secp256k1;

use std::{env,io};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn slice_to_le64(v: &[u8]) -> u64 {
	((v[7] as u64) << 8*7) |
	((v[6] as u64) << 8*6) |
	((v[5] as u64) << 8*5) |
	((v[4] as u64) << 8*4) |
	((v[3] as u64) << 8*3) |
	((v[2] as u64) << 8*2) |
	((v[1] as u64) << 8*1) |
	((v[0] as u64) << 8*0)
}

const SHARE_TARGET: [u8; 32] = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff, 0, 0, 0, 0, 0, 0]; // Diff 65536
fn main() {
	println!("USAGE: sample-pool --listen_bind=IP:port --auth_key=base58privkey --payout_address=addr [--server_id=up_to_36_byte_string_for_coinbase]");
	println!("--listen_bind - the address to bind to");
	println!("--auth_key - the auth key to use to authenticate to clients");
	println!("--payout_address - the Bitcoin address on which to receive payment");

	let mut listen_bind = None;
	let mut auth_key = None;
	let mut payout_addr = None;
	let mut server_id = None;

	for arg in env::args().skip(1) {
		if arg.starts_with("--listen_bind") {
			if listen_bind.is_some() {
				println!("Cannot specify multiple listen binds");
				return;
			}
			listen_bind = Some(match arg.split_at(14).1.parse() {
				Ok(sockaddr) => sockaddr,
				Err(_) =>{
					println!("Failed to parse listen_bind into a socket address");
					return;
				}
			});
		} else if arg.starts_with("--auth_key") {
			if auth_key.is_some() {
				println!("Cannot specify multiple auth keys");
				return;
			}
			auth_key = Some(match address::Privkey::from_str(arg.split_at(11).1) {
				Ok(privkey) => {
					if !privkey.compressed {
						println!("Private key must represent a compressed key!");
						return;
					}
					privkey.key
				},
				Err(_) =>{
					println!("Failed to parse auth_key into a private key");
					return;
				}
			});
		} else if arg.starts_with("--payout_address") {
			if payout_addr.is_some() {
				println!("Cannot specify multiple payout addresses");
				return;
			}
			//TODO: check network magic byte? We're allowed to mine on any net, though...
			payout_addr = Some(match Address::from_str(arg.split_at(17).1) {
				Ok(addr) => addr.script_pubkey(),
				Err(_) => {
					println!("Failed to parse payout_address into a Bitcoin address");
					return;
				}
			});
		} else if arg.starts_with("--server_id") {
			if server_id.is_some() {
				println!("Cannot specify multiple server IDs");
				return;
			}
			server_id = Some(arg.split_at(12).1.to_string());
			if server_id.as_ref().unwrap().len() > 36 {
				println!("server_id cannot be longer than 36 bytes");
				return;
			}
		} else {
			println!("Unkown arg: {}", arg);
			return;
		}
	}

	if listen_bind.is_none() || auth_key.is_none() || payout_addr.is_none() {
		println!("Need to specify all but server_id parameters");
		return;
	}

	let clients_ref = Rc::new(RefCell::new(HashMap::new()));

	current_thread::block_on_all(future::lazy(|| -> future::FutureResult<(), ()> {
		match net::TcpListener::bind(&listen_bind.unwrap()) {
			Ok(listener) => {
				let mut max_client_id = 0;

				current_thread::spawn(listener.incoming().for_each(move |sock| {
					sock.set_nodelay(true).unwrap();

					let (tx, rx) = sock.framed(PoolMsgFramer::new()).split();
					let (mut send_sink, send_stream) = mpsc::channel(5);
					current_thread::spawn(tx.send_all(send_stream.map_err(|_| -> io::Error {
						panic!("mpsc streams cant generate errors!");
					})).then(|_| {
						future::result(Ok(()))
					}));

					let secp_ctx = Secp256k1::new();
					macro_rules! sign_message {
						($msg: expr, $msg_type: expr) => {
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

								secp_ctx.sign(&hash, &auth_key.unwrap()).unwrap()
							}
						}
					}

					let payout_addr_clone = payout_addr.as_ref().unwrap().clone();
					let server_id_clone = server_id.clone();
					let clients = clients_ref.clone();
					let client_id = max_client_id;
					max_client_id += 1;

					let mut client_coinbase_postfix = utils::le64_to_array(client_id).to_vec();
					match server_id {
						Some(ref id) => client_coinbase_postfix.extend_from_slice(id.clone().as_bytes()),
						None => {},
					};

					let mut received_protocol_support = false;
					let mut client_authed = false;
					current_thread::spawn(rx.for_each(move |msg| {
						macro_rules! send_response {
							($msg: expr) => {
								match send_sink.start_send($msg) {
									Ok(_) => {},
									Err(_) => return future::result(Err(io::Error::new(io::ErrorKind::InvalidData, utils::HandleError)))
								}
							}
						}

						match msg {
							PoolMessage::ProtocolSupport { max_version, min_version, flags } => {
								if min_version > 1 || max_version < 1 {
									return future::result(Err(io::Error::new(io::ErrorKind::InvalidData, utils::HandleError)));
								}
								if flags != 0 {
									println!("Client requested unknown flags {}", flags);
								}
								send_response!(PoolMessage::ProtocolVersion {
									selected_version: 1,
									flags: 0,
									auth_key: PublicKey::from_secret_key(&secp_ctx, &auth_key.unwrap()).unwrap(),
								});
								received_protocol_support = true;
							},
							PoolMessage::ProtocolVersion { .. } => {
								println!("Got ProtocolVersion?");
								return future::result(Err(io::Error::new(io::ErrorKind::InvalidData, utils::HandleError)));
							},
							PoolMessage::PayoutInfoRequest { user_id, .. } => {
								if !received_protocol_support {
									return future::result(Err(io::Error::new(io::ErrorKind::InvalidData, utils::HandleError)));
								}
								if client_authed {
									return future::result(Err(io::Error::new(io::ErrorKind::InvalidData, utils::HandleError)));
								}

								let addr = match Address::from_str(match String::from_utf8(user_id.clone()) {
									Ok(string) => string,
									Err(_) => return future::result(Err(io::Error::new(io::ErrorKind::InvalidData, utils::HandleError))),
								}.as_str()) {
									Ok(addr) => addr,
									Err(_) => return future::result(Err(io::Error::new(io::ErrorKind::InvalidData, utils::HandleError))),
								};
								clients.borrow_mut().insert(client_id, addr);
								client_authed = true;

								let time = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
								let timestamp = time.as_secs() * 1000 + time.subsec_nanos() as u64 / 1_000_000;
								let payout_info = PoolPayoutInfo {
									user_id,
									timestamp,
									coinbase_postfix: client_coinbase_postfix.clone(),
									remaining_payout: payout_addr_clone.clone(),
									appended_outputs: vec![],
								};
								send_response!(PoolMessage::PayoutInfo {
									signature: sign_message!(payout_info, 3),
									payout_info,
								});

								let difficulty = PoolDifficulty {
									share_target: SHARE_TARGET,
									weak_block_target: [0; 32],
								};
								send_response!(PoolMessage::ShareDifficulty {
									signature: sign_message!(difficulty, 4),
									difficulty,
								});
							},
							PoolMessage::PayoutInfo { .. } => {
								println!("Got PayoutInfo?");
								return future::result(Err(io::Error::new(io::ErrorKind::InvalidData, utils::HandleError)));
							},
							PoolMessage::ShareDifficulty { .. } => {
								println!("Got ShareDifficulty?");
								return future::result(Err(io::Error::new(io::ErrorKind::InvalidData, utils::HandleError)));
							},
							PoolMessage::Share { ref share } => {
								if !received_protocol_support || !client_authed {
									return future::result(Err(io::Error::new(io::ErrorKind::InvalidData, utils::HandleError)));
								}

								if share.coinbase_tx.input.len() != 1 || share.coinbase_tx.output.len() < 1 {
									println!("Client sent share with a coinbase_tx which had an input count other than 1 or no payout");
									return future::result(Ok(()));
								}

								let coinbase = &share.coinbase_tx.input[0].script_sig[..];
								let share_client_id = match server_id_clone {
									Some(ref server_id) => {
										if coinbase.len() < server_id.len() + 8 || !coinbase.ends_with(&server_id.as_bytes()[..]) {
											println!("Client sent share which failed to include the required coinbase postfix");
											return future::result(Ok(()));
										}
										slice_to_le64(&coinbase[coinbase.len() - server_id.len() - 8..coinbase.len() - server_id.len()])

									},
									None => {
										if coinbase.len() < 8 {
											println!("Client sent share which failed to include the required coinbase postfix");
											return future::result(Ok(()));
										}
										slice_to_le64(&coinbase[coinbase.len() - 8..coinbase.len()])
									},
								};
								let clients_ref = clients.borrow();
								let client_payout = match clients_ref.get(&share_client_id) {
									Some(payout_addr) => payout_addr,
									None => {
										println!("Client sent share with a coinbase_tx which did not pay to a known auth'ed client");
										return future::result(Ok(()));
									}
								};

								for (idx, out) in share.coinbase_tx.output.iter().enumerate() {
									if idx == 0 {
										if out.script_pubkey != payout_addr_clone {
											println!("Got share which paid out to unknown location");
											return future::result(Ok(()));
										}
									} else if out.value != 0 {
										println!("Got share which paid out excess to unkown location");
										return future::result(Ok(()));
									}
								}

								let mut merkle_lhs = [0; 32];
								merkle_lhs.copy_from_slice(&share.coinbase_tx.txid()[..]);
								let mut sha = Sha256::new();
								for rhs in share.merkle_rhss.iter() {
									sha.reset();
									sha.input(&merkle_lhs);
									sha.input(&rhs[..]);
									sha.result(&mut merkle_lhs);
									sha.reset();
									sha.input(&merkle_lhs);
									sha.result(&mut merkle_lhs);
								}

								let block_hash = BlockHeader {
									version: share.header_version,
									prev_blockhash: Sha256dHash::from(&share.header_prevblock[..]),
									merkle_root: Sha256dHash::from(&merkle_lhs[..]),
									time: share.header_time,
									bits: share.header_nbits,
									nonce: share.header_nonce,
								}.bitcoin_hash();

								if utils::does_hash_meet_target(&block_hash[..], &SHARE_TARGET) {
									println!("Got valid share from {} for payout to script: {}", String::from_utf8_lossy(&share.user_tag), client_payout.to_string());
								} else {
									println!("Got work that missed target (hashed to {}, which is greater than {})", utils::bytes_to_hex(&block_hash[..]), utils::bytes_to_hex(&SHARE_TARGET[..]));
								}
							},
							PoolMessage::WeakBlock { .. } => {
								println!("Got WeakBlock with infinite difficulty?");
								return future::result(Err(io::Error::new(io::ErrorKind::InvalidData, utils::HandleError)));
							},
							PoolMessage::WeakBlockStateReset { } => {
								println!("Got WeakBlockStateReset?");
								return future::result(Err(io::Error::new(io::ErrorKind::InvalidData, utils::HandleError)));
							},
							PoolMessage::NewPoolServer { .. } => {
								println!("Got NewPoolServer?");
								return future::result(Err(io::Error::new(io::ErrorKind::InvalidData, utils::HandleError)));
							},
						}
						future::result(Ok(()))
					}).then(|_| {
						future::result(Ok(()))
					}));

					future::result(Ok(()))
				}).then(|_| {
					future::result(Ok(()))
				}));
			},
			Err(_) => {
				println!("Failed to bind to listen bind addr");
			}
		};
		future::result(Ok(()))
	})).unwrap();
}
